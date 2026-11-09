#!/usr/bin/env python3
"""Generate the CAYW THIRD-PARTY-NOTICES file from Cargo metadata.

The generator is offline and deterministic:

- ``cargo metadata`` runs with ``--locked --offline`` for
  ``aarch64-apple-darwin`` for the workspace (with the packaged
  ``cayw-runtime`` feature set) and for the independent
  ``examples/headless-client`` workspace.
- Only normal and build dependencies reachable from the packaged binaries
  are included. Dev-only dependencies are ignored.
- The two closures are unioned by full package ID (name, version, source), so
  a client-only version or transitive dependency cannot be skipped.
- Every third-party crate must declare a license expression.
- Copyleft-only licenses are rejected unless the expression offers a
  permissive alternative.
- Every license in the branch CAYW complies with needs matching text from the
  package, the same repository, or a pinned offline upstream asset in
  ``third-party/licenses``. The asset records its source URL and package
  mapping. Missing text is a hard packaging failure; the generator never
  invents a copyright holder or substitutes a synthetic license.

The generated file is distributed inside the macOS bundle and listed in
``SHA256SUMS``. Do not edit it by hand; regenerate it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

TARGET = "aarch64-apple-darwin"
ROOT_PACKAGES = ("cayw-runtime", "cayw-tui", "cayw-webui")
CLIENT_PACKAGES = ("cayw-client",)
CLIENT_MANIFEST = Path("examples") / "headless-client" / "Cargo.toml"
UPSTREAM_LICENSE_MANIFEST = Path("third-party") / "licenses" / "upstream.json"
CARGO_FEATURES = ("cayw-runtime/plugins",)

ALLOWED_LICENSES = {
    "0BSD",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "CC0-1.0",
    "CDLA-Permissive-2.0",
    "ISC",
    "MIT",
    "MIT-0",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
}
ALLOWED_EXCEPTIONS = {"LLVM-exception"}

# Text checks used to prove that a license file actually carries the license
# required by the SPDX branch we comply with. Each entry lists markers that
# must all appear (case-insensitive).
LICENSE_MARKERS = {
    "0BSD": ("zero-clause bsd",),
    "Apache-2.0": ("apache license", "version 2.0"),
    "BSD-2-Clause": ("redistribution and use in source and binary forms",),
    "BSD-3-Clause": (
        "redistribution and use in source and binary forms",
        "neither the name",
    ),
    "BSL-1.0": ("boost software license",),
    "CC0-1.0": ("cc0 1.0",),
    "CDLA-Permissive-2.0": ("community data license agreement",),
    "ISC": ("permission to use, copy, modify, and/or distribute this software",),
    "MIT": ("permission is hereby granted, free of charge", "copyright"),
    "MIT-0": ("mit no attribution",),
    "Unicode-3.0": ("unicode license",),
    "Unlicense": ("this is free and unencumbered software",),
    "Zlib": ("this software is provided 'as-is'",),
}

TOKEN_RE = re.compile(r"\(|\)|[A-Za-z0-9][A-Za-z0-9.+-]*")


class NoticeError(Exception):
    """A license problem that must stop packaging."""


@dataclass(frozen=True)
class PackageNotice:
    name: str
    version: str
    source: str
    expression: str
    required: tuple[str, ...]
    repository: str
    files: tuple[tuple[str, str], ...]


def _tokens(expression: str) -> list[str]:
    # crates.io historically uses "/" for a dual-license choice. Treat it as
    # SPDX OR so old manifests are evaluated correctly.
    return TOKEN_RE.findall(expression.replace("/", " OR "))


class _Parser:
    def __init__(self, tokens: list[str], expression: str) -> None:
        self.tokens = tokens
        self.expression = expression
        self.index = 0

    def parse(self) -> tuple:
        node = self._parse_or()
        if self.index != len(self.tokens):
            raise NoticeError(
                f"unexpected token {self.tokens[self.index]!r} in license "
                f"expression {self.expression!r}"
            )
        return node

    def _peek(self) -> str | None:
        if self.index >= len(self.tokens):
            return None
        return self.tokens[self.index]

    def _take(self, expected: str | None = None) -> str:
        token = self._peek()
        if token is None:
            raise NoticeError(f"truncated license expression {self.expression!r}")
        if expected is not None and token != expected:
            raise NoticeError(
                f"expected {expected!r} in license expression "
                f"{self.expression!r}, got {token!r}"
            )
        self.index += 1
        return token

    def _parse_or(self) -> tuple:
        node = self._parse_and()
        while self._peek() == "OR":
            self._take("OR")
            node = ("or", node, self._parse_and())
        return node

    def _parse_and(self) -> tuple:
        node = self._parse_with()
        while self._peek() == "AND":
            self._take("AND")
            node = ("and", node, self._parse_with())
        return node

    def _parse_with(self) -> tuple:
        node = self._parse_atom()
        if self._peek() == "WITH":
            self._take("WITH")
            exception = self._take()
            node = ("with", node[1], exception)
        return node

    def _parse_atom(self) -> tuple:
        token = self._peek()
        if token == "(":
            self._take("(")
            node = self._parse_or()
            self._take(")")
            return node
        if token in (None, ")", "AND", "OR", "WITH"):
            raise NoticeError(
                f"expected a license identifier in expression {self.expression!r}"
            )
        return ("id", self._take())


def _choices(node: tuple) -> list[set[str]]:
    kind = node[0]
    if kind == "id":
        name = node[1]
        return [{name}] if name in ALLOWED_LICENSES else []
    if kind == "with":
        base, exception = node[1], node[2]
        if base in ALLOWED_LICENSES and exception in ALLOWED_EXCEPTIONS:
            # The exception adds permissions; the base license obligations and
            # text still apply.
            return [{base}]
        return []
    if kind == "or":
        return _dedupe(_choices(node[1]) + _choices(node[2]))
    if kind == "and":
        combined: list[set[str]] = []
        for left in _choices(node[1]):
            for right in _choices(node[2]):
                combined.append(left | right)
        return _dedupe(combined)
    raise AssertionError(f"unknown license node {node!r}")


def _dedupe(choices: list[set[str]]) -> list[set[str]]:
    seen: set[frozenset[str]] = set()
    result: list[set[str]] = []
    for choice in choices:
        key = frozenset(choice)
        if key not in seen:
            seen.add(key)
            result.append(choice)
    return result


def license_choices(expression: str) -> tuple[tuple[str, ...], ...]:
    """Return every allowed set of license obligations for an SPDX expression,
    smallest first. Raises when no permissive branch exists."""

    tokens = _tokens(expression)
    if not tokens:
        raise NoticeError(f"empty license expression {expression!r}")
    node = _Parser(tokens, expression).parse()
    choices = _choices(node)
    if not choices:
        identifiers = sorted(
            {
                token
                for token in tokens
                if token not in {"(", ")", "AND", "OR", "WITH"}
            }
        )
        unsupported = [
            identifier
            for identifier in identifiers
            if identifier not in ALLOWED_LICENSES
            and identifier not in ALLOWED_EXCEPTIONS
        ]
        detail = ", ".join(unsupported) if unsupported else ", ".join(identifiers)
        raise NoticeError(
            f"license expression {expression!r} has no allowed branch "
            f"(disallowed identifiers: {detail})"
        )
    ordered = sorted(
        (tuple(sorted(choice)) for choice in choices),
        key=lambda choice: (len(choice), choice),
    )
    return tuple(ordered)


def best_license_choice(expression: str) -> tuple[str, ...]:
    """Return the smallest allowed set of license obligations."""

    return license_choices(expression)[0]


def _license_text_covers(license_id: str, text: str) -> bool:
    markers = LICENSE_MARKERS.get(license_id)
    if markers is None:
        return False
    lowered = text.lower()
    return all(marker in lowered for marker in markers)


def _license_files(package: dict) -> list[Path]:
    manifest = Path(package["manifest_path"]).resolve()
    base = manifest.parent
    files: list[Path] = []
    try:
        entries = sorted(base.iterdir(), key=lambda path: path.name.lower())
    except OSError as error:
        raise NoticeError(
            f"cannot read package directory for {package['name']} "
            f"{package['version']}: {error}"
        ) from error
    for entry in entries:
        if not entry.is_file():
            continue
        lowered = entry.name.lower()
        if any(
            marker in lowered
            for marker in ("license", "licence", "copying", "notice", "unlicense")
        ):
            files.append(entry)
    declared = package.get("license_file")
    if declared:
        path = (base / declared).resolve()
        if path.is_file() and path not in files:
            files.append(path)
    return files


def _normalize_repository(value: str) -> str:
    repository = value.strip().rstrip("/")
    if repository.endswith(".git"):
        repository = repository[:-4]
    return repository.lower()


def load_upstream_licenses(
    root: Path,
) -> dict[tuple[str, str, str], tuple[tuple[str, Path], ...]]:
    """Load pinned upstream license files that published crates omit.

    The manifest records the upstream repository, tag, path, raw source URL,
    license, package mapping, and SHA-256. Nothing is fetched at generation
    time; a bad or missing asset is a packaging failure.
    """

    manifest_path = root / UPSTREAM_LICENSE_MANIFEST
    if not manifest_path.is_file():
        return {}
    try:
        entries = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise NoticeError(f"cannot read {manifest_path}: {error}") from error
    if not isinstance(entries, list):
        raise NoticeError(f"{manifest_path} must contain a JSON array")

    root = root.resolve()
    index: dict[tuple[str, str, str], list[tuple[str, Path]]] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            raise NoticeError(f"{manifest_path}: every entry must be an object")

        def field(name: str) -> str:
            value = entry.get(name)
            if not isinstance(value, str) or not value.strip():
                raise NoticeError(
                    f"{manifest_path}: entry is missing a non-empty {name!r}"
                )
            return value.strip()

        repository = field("repository")
        repository_key = _normalize_repository(repository)
        tag = field("tag")
        upstream_path = field("path")
        source_url = field("source_url")
        license_id = field("license")
        file_name = field("file")
        expected_digest = field("sha256").lower()
        commit = entry.get("commit")
        if commit is not None and (
            not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit)
        ):
            raise NoticeError(
                f"{manifest_path}: commit must be a 40-character lowercase SHA"
            )
        if license_id not in ALLOWED_LICENSES:
            raise NoticeError(
                f"{manifest_path}: unsupported upstream license {license_id!r}"
            )

        file_path = (root / file_name).resolve()
        if not file_path.is_relative_to(root):
            raise NoticeError(
                f"{manifest_path}: {file_name!r} escapes the repository root"
            )
        try:
            raw = file_path.read_bytes()
        except OSError as error:
            raise NoticeError(
                f"{manifest_path}: cannot read upstream license {file_path}: {error}"
            ) from error
        if len(raw) > 2 * 1024 * 1024:
            raise NoticeError(
                f"{manifest_path}: {file_path.name} exceeds 2 MiB"
            )
        actual_digest = hashlib.sha256(raw).hexdigest()
        if actual_digest != expected_digest:
            raise NoticeError(
                f"{manifest_path}: {file_path.name} has SHA-256 "
                f"{actual_digest}, expected {expected_digest}"
            )
        text = raw.decode("utf-8", errors="replace")
        if not _license_text_covers(license_id, text):
            raise NoticeError(
                f"{manifest_path}: {file_path.name} does not contain "
                f"{license_id} text"
            )

        packages = entry.get("packages")
        if not isinstance(packages, list) or not packages:
            raise NoticeError(
                f"{manifest_path}: entry for {source_url} maps no packages"
            )
        pin = f"{tag}, commit {commit}" if commit else tag
        label = (
            f"offline upstream {source_url} "
            f"({upstream_path} at {pin}, {repository})"
        )
        for package in packages:
            if not isinstance(package, dict):
                raise NoticeError(
                    f"{manifest_path}: package mappings must be objects"
                )
            name = package.get("name")
            version = package.get("version")
            if not isinstance(name, str) or not isinstance(version, str):
                raise NoticeError(
                    f"{manifest_path}: package mapping needs name and version"
                )
            index.setdefault(
                (name, version, repository_key), []
            ).append((label, file_path))
    return {key: tuple(value) for key, value in index.items()}


def build_notice(
    package: dict,
    repository_files: tuple[tuple[str, Path], ...] = (),
    upstream_files: tuple[tuple[str, Path], ...] = (),
) -> PackageNotice:
    name = package["name"]
    version = package["version"]
    expression = package.get("license")
    if not expression:
        raise NoticeError(f"{name} {version} has no declared license expression")
    options = license_choices(expression)

    files: list[tuple[str, str]] = []
    seen: set[str] = set()

    def add_file(label: str, path: Path, kind: str) -> None:
        try:
            raw = path.read_bytes()
        except OSError as error:
            raise NoticeError(
                f"{name} {version}: cannot read {kind} {path}: {error}"
            ) from error
        if len(raw) > 2 * 1024 * 1024:
            raise NoticeError(f"{name} {version}: {kind} {path.name} exceeds 2 MiB")
        digest = hashlib.sha256(raw).hexdigest()
        if digest in seen:
            return
        seen.add(digest)
        text = raw.decode("utf-8", errors="replace")
        text = text.replace("\r\n", "\n").replace("\r", "\n").strip("\n")
        files.append((label, text))

    for path in _license_files(package):
        add_file(path.name, path, "license file")

    def choose() -> tuple[tuple[str, ...] | None, list[str]]:
        combined = "\n".join(text for _, text in files)
        failures: list[str] = []
        for candidate in options:
            missing = [
                license_id
                for license_id in candidate
                if not _license_text_covers(license_id, combined)
            ]
            if missing:
                failures.append(
                    f"{'/'.join(candidate)} misses text for {', '.join(missing)}"
                )
                continue
            return candidate, failures
        return None, failures

    required, failures = choose()
    if required is None:
        # Some workspace-shaped projects publish only part of their license
        # texts with each crate. Prefer a license file from another crate in
        # the same repository. There is no synthetic fallback: if the upstream
        # repository also lacks the text, packaging stops.
        for owner, path in repository_files:
            add_file(
                f"repository LICENSE from {owner} ({path.name})",
                path,
                "repository license file",
            )
        required, failures = choose()
    if required is None:
        # A published crate may omit a license file that exists in its
        # upstream tag. Use only an explicitly pinned asset with recorded
        # source URL and package mapping; never synthesize the text.
        for label, path in upstream_files:
            add_file(label, path, "offline upstream license file")
        required, failures = choose()
    if required is None:
        raise NoticeError(
            f"{name} {version}: no text found for an allowed license branch of "
            f"{expression!r} ({'; '.join(failures)})"
        )

    repository = package.get("repository") or "not declared"
    return PackageNotice(
        name=name,
        version=version,
        source=package.get("source") or "local",
        expression=expression,
        required=required,
        repository=repository,
        files=tuple(files),
    )


def reachable_third_party(
    metadata: dict, root_names: tuple[str, ...] = ROOT_PACKAGES
) -> list[dict]:
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [
        package["id"]
        for package in metadata["packages"]
        if package.get("source") is None and package["name"] in root_names
    ]
    if not roots:
        raise NoticeError(
            "metadata has none of the expected root packages: "
            + ", ".join(root_names)
        )

    seen: set[str] = set()
    pending = list(roots)
    while pending:
        node_id = pending.pop()
        if node_id in seen:
            continue
        seen.add(node_id)
        node = nodes.get(node_id)
        if node is None:
            continue
        for dependency in node.get("deps", []):
            kinds = [kind.get("kind") for kind in dependency.get("dep_kinds", [])]
            # cargo uses null for a normal dependency. Skip edges that only
            # exist for tests, benches, and examples.
            if kinds and all(kind == "dev" for kind in kinds):
                continue
            pending.append(dependency["pkg"])

    third_party = [
        packages[node_id]
        for node_id in seen
        if packages[node_id].get("source") is not None
    ]
    if not third_party:
        raise NoticeError("no third-party packages are reachable from the packaged binaries")
    return sorted(
        third_party,
        key=lambda package: (
            package["name"].lower(),
            package["version"],
            package.get("source") or "",
        ),
    )


def _render(notice: PackageNotice) -> str:
    lines = [
        "=" * 78,
        f"Package: {notice.name} {notice.version}",
        f"Source: {notice.source}",
        f"License expression: {notice.expression}",
        f"Required notice(s): {', '.join(notice.required)}",
        f"Repository: {notice.repository}",
    ]
    if notice.files:
        lines.append(
            "License file(s): " + ", ".join(name for name, _ in notice.files)
        )
    else:
        lines.append("License file(s): none shipped in the Cargo package")
    lines.append("-" * 78)
    for name, text in notice.files:
        lines.append(f"[{name}]")
        lines.append(text)
    return "\n".join(lines)


def package_key(package: dict) -> tuple[str, str, str]:
    """The full package ID used to union the two metadata closures."""

    return (
        package["name"],
        package["version"],
        package.get("source") or "local",
    )


def merge_packages(*groups: list[dict]) -> list[dict]:
    """Union package groups by full ID, keeping deterministic order.

    The same name and version from different sources are distinct packages and
    both stay in the result.
    """

    merged: dict[tuple[str, str, str], dict] = {}
    for group in groups:
        for package in group:
            merged.setdefault(package_key(package), package)
    return sorted(
        merged.values(),
        key=lambda package: (
            package["name"].lower(),
            package["version"],
            package.get("source") or "",
        ),
    )


def repository_license_index(
    metadata: dict,
) -> dict[str, tuple[tuple[str, Path], ...]]:
    index: dict[str, list[tuple[str, Path]]] = {}
    for package in metadata["packages"]:
        repository = package.get("repository")
        if not repository:
            continue
        for path in _license_files(package):
            index.setdefault(repository, []).append(
                (f"{package['name']} {package['version']}", path)
            )
    return {
        repository: tuple(
            sorted(
                set(files),
                key=lambda item: (
                    item[0],
                    item[1].name.lower(),
                    str(item[1]),
                ),
            )
        )
        for repository, files in index.items()
    }


def merge_repository_indexes(
    *indexes: dict[str, tuple[tuple[str, Path], ...]],
) -> dict[str, tuple[tuple[str, Path], ...]]:
    merged: dict[str, set[tuple[str, Path]]] = {}
    for index in indexes:
        for repository, files in index.items():
            merged.setdefault(repository, set()).update(files)
    return {
        repository: tuple(
            sorted(
                files,
                key=lambda item: (
                    item[0],
                    item[1].name.lower(),
                    str(item[1]),
                ),
            )
        )
        for repository, files in merged.items()
    }


def generate(
    root: Path,
    metadata_path: Path | None = None,
    client_metadata_path: Path | None = None,
) -> str:
    root_metadata = load_metadata(root, metadata_path)
    client_metadata = load_metadata(
        root, client_metadata_path, root / CLIENT_MANIFEST
    )
    packages = merge_packages(
        reachable_third_party(root_metadata, ROOT_PACKAGES),
        reachable_third_party(client_metadata, CLIENT_PACKAGES),
    )
    repository_files = merge_repository_indexes(
        repository_license_index(root_metadata),
        repository_license_index(client_metadata),
    )
    upstream_files = load_upstream_licenses(root)
    notices: list[PackageNotice] = []
    failures: list[str] = []
    for package in packages:
        try:
            notices.append(
                build_notice(
                    package,
                    repository_files.get(package.get("repository"), ()),
                    upstream_files.get(
                        (
                            package["name"],
                            package["version"],
                            _normalize_repository(
                                package.get("repository") or ""
                            ),
                        ),
                        (),
                    ),
                )
            )
        except NoticeError as error:
            # Collect every deficient package so one packaging run reports the
            # full set instead of hiding later problems behind the first one.
            failures.append(str(error))
    if failures:
        raise NoticeError(
            f"{len(failures)} package(s) have no traceable license text:\n"
            + "\n".join(failures)
        )
    header = [
        "CAYW THIRD-PARTY NOTICES",
        "",
        "Generated by scripts/generate-third-party-notices.py. Do not edit by hand.",
        f"Target platform: {TARGET}",
        "Packaged binaries: cayw, cayw-tui, cayw-webui, cayw-client",
        "Sources: cargo metadata --locked --offline --filter-platform "
        f"{TARGET} --features {', '.join(CARGO_FEATURES)} (workspace),",
        "         cargo metadata --locked --offline --filter-platform "
        f"{TARGET} --manifest-path {CLIENT_MANIFEST.as_posix()} (client)",
        "The two third-party closures are unioned by name, version, and source;",
        "client-only versions and transitive dependencies are included.",
        "Packages whose published crate omits the license text use pinned offline",
        "assets from third-party/licenses/upstream.json; each such notice names",
        "its upstream source URL. Generation performs no network access.",
        "",
        f"Third-party packages: {len(notices)}",
        "",
        "CAYW itself is licensed under Apache-2.0; see LICENSE.",
        "The CDLA-Permissive-2.0 and pi MIT texts are also shipped under",
        "third-party/ in this bundle.",
        "",
        "Each section lists the license branch CAYW relies on. Dual-license",
        "expressions may therefore show fewer required notices than the full",
        "expression; the permissive alternative is the one being used.",
    ]
    body = "\n\n".join(_render(notice) for notice in notices)
    return "\n".join(header) + "\n\n" + body + "\n"


def load_metadata(
    root: Path,
    metadata_path: Path | None,
    manifest_path: Path | None = None,
) -> dict:
    if metadata_path is not None:
        try:
            return json.loads(metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise NoticeError(f"cannot read metadata {metadata_path}: {error}") from error

    command = [
        os.environ.get("CARGO", "cargo"),
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--offline",
        "--filter-platform",
        TARGET,
    ]
    if manifest_path is not None:
        command += ["--manifest-path", str(manifest_path)]
    else:
        command += ["--features", ",".join(CARGO_FEATURES)]
    try:
        result = subprocess.run(
            command,
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise NoticeError(f"cannot run cargo metadata: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise NoticeError(f"cargo metadata failed: {detail}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise NoticeError(f"cargo metadata returned invalid JSON: {error}") from error


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate or verify THIRD-PARTY-NOTICES from Cargo metadata."
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="repository root (default: parent of this script)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="write the notices here (default: standard output)",
    )
    parser.add_argument(
        "--check",
        type=Path,
        metavar="FILE",
        help="fail when FILE differs from the generated notices",
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        help="read saved workspace `cargo metadata` JSON instead of running cargo",
    )
    parser.add_argument(
        "--client-metadata",
        type=Path,
        help="read saved examples/headless-client `cargo metadata` JSON instead of running cargo",
    )
    args = parser.parse_args(argv)
    if args.output is not None and args.check is not None:
        parser.error("--output and --check cannot be used together")

    try:
        text = generate(args.root.resolve(), args.metadata, args.client_metadata)
        encoded = text.encode("utf-8")
        if args.check is not None:
            try:
                existing = args.check.read_bytes()
            except OSError as error:
                raise NoticeError(f"cannot read {args.check}: {error}") from error
            if existing != encoded:
                print(
                    f"{args.check} is stale or differs from generated notices "
                    f"({len(existing)} bytes vs {len(encoded)} bytes)",
                    file=sys.stderr,
                )
                return 1
            print(f"{args.check} is up to date ({len(encoded)} bytes)")
            return 0
        if args.output is not None:
            try:
                args.output.write_bytes(encoded)
            except OSError as error:
                raise NoticeError(f"cannot write {args.output}: {error}") from error
            print(
                f"wrote {args.output} ({len(encoded)} bytes, "
                f"{text.count(chr(10) + 'Package: ')} packages)",
                file=sys.stderr,
            )
            return 0
        sys.stdout.write(text)
        return 0
    except NoticeError as error:
        print(f"third-party notices: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
