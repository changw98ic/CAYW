#!/usr/bin/env python3
"""Tests for scripts/generate-third-party-notices.py.

Run with: python3 scripts/test-generate-third-party-notices.py
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import shutil
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "generate-third-party-notices.py"

# Interactive shells in this workspace do not always put Cargo on PATH.
if shutil.which("cargo") is None:
    cargo = pathlib.Path.home() / ".cargo" / "bin" / "cargo"
    if cargo.is_file():
        os.environ["CARGO"] = str(cargo)

_SPEC = importlib.util.spec_from_file_location("generate_third_party_notices", SCRIPT)
if _SPEC is None or _SPEC.loader is None:
    raise RuntimeError(f"cannot load {SCRIPT}")
notices = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = notices
_SPEC.loader.exec_module(notices)


def run_main(arguments: list[str]) -> tuple[int, str, str]:
    stdout = io.StringIO()
    stderr = io.StringIO()
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        code = notices.main(arguments)
    return code, stdout.getvalue(), stderr.getvalue()


MIT_TEXT = (
    "MIT License\n\n"
    "Copyright (c) 2024 Fixture Holder\n\n"
    "Permission is hereby granted, free of charge, to any person obtaining a "
    "copy of this software and associated documentation files (the "
    '"Software"), to deal in the Software without restriction.\n'
)


def write_fixture_package(
    base: pathlib.Path, directory: str, name: str, version: str
) -> str:
    package_dir = base / directory
    package_dir.mkdir()
    (package_dir / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
    (package_dir / "LICENSE").write_text(MIT_TEXT, encoding="utf-8")
    return str(package_dir / "Cargo.toml")


def fixture_metadata(
    root: pathlib.Path,
) -> tuple[pathlib.Path, pathlib.Path]:
    """Two workspaces whose package IDs overlap only partially."""

    root_packages = [
        {
            "id": "root",
            "name": "cayw-runtime",
            "version": "0.1.0",
            "source": None,
            "manifest_path": str(root / "Cargo.toml"),
        },
        {
            "id": "dup-1",
            "name": "dup",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index",
            "license": "MIT",
            "manifest_path": write_fixture_package(root, "dup-1", "dup", "1.0.0"),
        },
        {
            "id": "split-a",
            "name": "split",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index-a",
            "license": "MIT",
            "manifest_path": write_fixture_package(root, "split-a", "split", "1.0.0"),
        },
    ]
    client_packages = [
        {
            "id": "client-root",
            "name": "cayw-client",
            "version": "0.1.0",
            "source": None,
            "manifest_path": str(root / "client" / "Cargo.toml"),
        },
        {
            "id": "dup-1-client",
            "name": "dup",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index",
            "license": "MIT",
            "manifest_path": str(root / "dup-1" / "Cargo.toml"),
        },
        {
            "id": "dup-2-client",
            "name": "dup",
            "version": "2.0.0",
            "source": "registry+https://example.invalid/index",
            "license": "MIT",
            "manifest_path": write_fixture_package(root, "dup-2", "dup", "2.0.0"),
        },
        {
            "id": "client-only",
            "name": "client-only",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index",
            "license": "MIT",
            "manifest_path": write_fixture_package(
                root, "client-only", "client-only", "1.0.0"
            ),
        },
        {
            "id": "client-transitive",
            "name": "client-transitive",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index",
            "license": "MIT",
            "manifest_path": write_fixture_package(
                root, "client-transitive", "client-transitive", "1.0.0"
            ),
        },
        {
            "id": "split-b",
            "name": "split",
            "version": "1.0.0",
            "source": "registry+https://example.invalid/index-b",
            "license": "MIT",
            "manifest_path": write_fixture_package(root, "split-b", "split", "1.0.0"),
        },
    ]
    root_metadata = {
        "packages": root_packages,
        "resolve": {
            "nodes": [
                {
                    "id": "root",
                    "deps": [
                        {"pkg": "dup-1", "dep_kinds": [{"kind": None}]},
                        {"pkg": "split-a", "dep_kinds": [{"kind": None}]},
                    ],
                },
                {"id": "dup-1", "deps": []},
                {"id": "split-a", "deps": []},
            ]
        },
    }
    client_metadata = {
        "packages": client_packages,
        "resolve": {
            "nodes": [
                {
                    "id": "client-root",
                    "deps": [
                        {"pkg": "dup-1-client", "dep_kinds": [{"kind": None}]},
                        {"pkg": "dup-2-client", "dep_kinds": [{"kind": None}]},
                        {"pkg": "client-only", "dep_kinds": [{"kind": None}]},
                        {"pkg": "split-b", "dep_kinds": [{"kind": None}]},
                    ],
                },
                {
                    "id": "client-only",
                    "deps": [
                        {"pkg": "client-transitive", "dep_kinds": [{"kind": None}]}
                    ],
                },
                {"id": "client-transitive", "deps": []},
                {"id": "dup-1-client", "deps": []},
                {"id": "dup-2-client", "deps": []},
                {"id": "split-b", "deps": []},
            ]
        },
    }
    root_path = root / "root-metadata.json"
    client_path = root / "client-metadata.json"
    root_path.write_text(json.dumps(root_metadata), encoding="utf-8")
    client_path.write_text(json.dumps(client_metadata), encoding="utf-8")
    return root_path, client_path


class SpdxTests(unittest.TestCase):
    def test_permissive_alternatives_and_combinations(self) -> None:
        self.assertEqual(
            notices.best_license_choice("MIT OR GPL-3.0-only"), ("MIT",)
        )
        self.assertEqual(
            notices.best_license_choice("MIT AND BSD-3-Clause"),
            ("BSD-3-Clause", "MIT"),
        )
        self.assertEqual(
            notices.best_license_choice("Apache-2.0 WITH LLVM-exception"),
            ("Apache-2.0",),
        )
        self.assertEqual(
            notices.best_license_choice("MIT/Apache-2.0"), ("Apache-2.0",)
        )
        self.assertEqual(
            notices.best_license_choice(
                "(MIT OR Apache-2.0) AND Unicode-3.0"
            ),
            ("Apache-2.0", "Unicode-3.0"),
        )

    def test_copyleft_only_and_unknown_exceptions_fail(self) -> None:
        for expression in (
            "GPL-3.0-only",
            "AGPL-3.0-only",
            "MIT AND GPL-3.0-only",
            "Apache-2.0 WITH Classpath-exception-2.0",
            "LicenseRef-proprietary",
        ):
            with self.subTest(expression=expression):
                with self.assertRaises(notices.NoticeError):
                    notices.best_license_choice(expression)

    def test_malformed_expression_fails(self) -> None:
        for expression in ("", "MIT AND", "(MIT OR Apache-2.0", "MIT LICENSE"):
            with self.subTest(expression=expression):
                with self.assertRaises(notices.NoticeError):
                    notices.best_license_choice(expression)


class CoverageTests(unittest.TestCase):
    def test_missing_expression_is_an_error(self) -> None:
        package = {
            "name": "example",
            "version": "1.0.0",
            "manifest_path": "/does/not/matter/Cargo.toml",
            "license": None,
        }
        with self.assertRaises(notices.NoticeError):
            notices.build_notice(package)

    def test_missing_non_mit_text_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory)
            (base / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            package = {
                "name": "example",
                "version": "1.0.0",
                "manifest_path": str(base / "Cargo.toml"),
                "license": "Apache-2.0",
            }
            with self.assertRaises(notices.NoticeError):
                notices.build_notice(package)

    def test_missing_mit_file_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory)
            (base / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            package = {
                "name": "example",
                "version": "1.0.0",
                "manifest_path": str(base / "Cargo.toml"),
                "license": "MIT",
            }
            with self.assertRaises(notices.NoticeError):
                notices.build_notice(package)
            self.assertFalse(hasattr(notices, "CANONICAL_MIT"))

    def test_package_mit_without_named_copyright_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = pathlib.Path(directory)
            (base / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            (base / "LICENSE-MIT").write_text(
                "MIT License\n\nPermission is hereby granted, free of charge, "
                "to any person obtaining a copy.\n\n"
                "The above copyright notice and this permission notice shall "
                "be included in all copies.\n",
                encoding="utf-8",
            )
            package = {
                "name": "example",
                "version": "1.0.0",
                "manifest_path": str(base / "Cargo.toml"),
                "license": "MIT",
            }
            notice = notices.build_notice(package)
            self.assertEqual(notice.required, ("MIT",))
            self.assertTrue(
                any(label == "LICENSE-MIT" for label, _ in notice.files)
            )

    def test_same_repository_license_fills_missing_package_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            package_dir = root / "package"
            sibling_dir = root / "sibling"
            package_dir.mkdir()
            sibling_dir.mkdir()
            (package_dir / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            (sibling_dir / "LICENSE").write_text(MIT_TEXT, encoding="utf-8")
            package = {
                "name": "package",
                "version": "1.0.0",
                "manifest_path": str(package_dir / "Cargo.toml"),
                "license": "MIT",
                "repository": "https://example.invalid/repo",
            }
            notice = notices.build_notice(
                package, (("sibling", sibling_dir / "LICENSE"),)
            )
            self.assertEqual(notice.required, ("MIT",))
            self.assertTrue(
                any("repository LICENSE from sibling" in label for label, _ in notice.files)
            )

    def test_dev_only_dependencies_are_ignored(self) -> None:
        metadata = {
            "packages": [
                {
                    "id": "root",
                    "name": "cayw-runtime",
                    "version": "0.1.0",
                    "source": None,
                },
                {
                    "id": "normal",
                    "name": "normal-dep",
                    "version": "1.0.0",
                    "source": "registry+https://example.invalid/index",
                },
                {
                    "id": "dev",
                    "name": "dev-dep",
                    "version": "1.0.0",
                    "source": "registry+https://example.invalid/index",
                },
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "root",
                        "deps": [
                            {"pkg": "normal", "dep_kinds": [{"kind": None}]},
                            {"pkg": "dev", "dep_kinds": [{"kind": "dev"}]},
                        ],
                    },
                    {"id": "normal", "deps": []},
                    {"id": "dev", "deps": []},
                ]
            },
        }
        names = [
            package["name"]
            for package in notices.reachable_third_party(metadata)
        ]
        self.assertEqual(names, ["normal-dep"])

    def test_prohibited_reachable_package_stops_generation(self) -> None:
        metadata = {
            "packages": [
                {
                    "id": "root",
                    "name": "cayw-runtime",
                    "version": "0.1.0",
                    "source": None,
                },
                {
                    "id": "bad",
                    "name": "bad-dep",
                    "version": "1.0.0",
                    "source": "registry+https://example.invalid/index",
                    "license": "GPL-3.0-only",
                    "manifest_path": "/does/not/matter/Cargo.toml",
                },
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "root",
                        "deps": [{"pkg": "bad", "dep_kinds": [{"kind": None}]}],
                    },
                    {"id": "bad", "deps": []},
                ]
            },
        }
        with self.assertRaises(notices.NoticeError):
            notices.build_notice(notices.reachable_third_party(metadata)[0])


class UpstreamLicenseTests(unittest.TestCase):
    def make_root(
        self,
        directory: str,
        digest: str | None = None,
        packages: list[dict] | None = None,
    ) -> tuple[pathlib.Path, dict]:
        root = pathlib.Path(directory)
        licenses = root / "third-party" / "licenses"
        licenses.mkdir(parents=True)
        asset = ROOT / "third-party" / "licenses" / "Nugine-simd-v0.8.0-LICENSE"
        local_asset = licenses / asset.name
        shutil.copyfile(asset, local_asset)
        entry = {
            "repository": "https://github.com/Nugine/simd",
            "tag": "v0.8.0",
            "commit": "d74c030d9dc4f3cae02146d1f497ff62726ef09a",
            "path": "LICENSE",
            "source_url": "https://raw.githubusercontent.com/Nugine/simd/d74c030d9dc4f3cae02146d1f497ff62726ef09a/LICENSE",
            "license": "MIT",
            "file": f"third-party/licenses/{asset.name}",
            "sha256": digest
            or hashlib.sha256(asset.read_bytes()).hexdigest(),
            "packages": packages
            if packages is not None
            else [
                {"name": "uuid-simd", "version": "0.8.0"},
                {"name": "vsimd", "version": "0.8.0"},
            ],
        }
        (licenses / "upstream.json").write_text(
            json.dumps([entry]), encoding="utf-8"
        )

        package_dir = root / "package"
        package_dir.mkdir()
        (package_dir / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        package = {
            "name": "uuid-simd",
            "version": "0.8.0",
            "manifest_path": str(package_dir / "Cargo.toml"),
            "license": "MIT",
            "repository": "https://github.com/Nugine/simd",
        }
        return root, package

    def test_pinned_asset_supplies_missing_package_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, package = self.make_root(directory)
            index = notices.load_upstream_licenses(root)
            key = (
                "uuid-simd",
                "0.8.0",
                notices._normalize_repository("https://github.com/Nugine/simd"),
            )
            files = index[key]
            self.assertNotIn(
                (
                    "uuid-simd",
                    "0.8.0",
                    notices._normalize_repository("https://example.invalid/fork"),
                ),
                index,
            )
            notice = notices.build_notice(package, (), files)
            self.assertEqual(notice.required, ("MIT",))
            self.assertTrue(
                any(
                    "raw.githubusercontent.com/Nugine/simd/d74c030d9dc4f3cae02146d1f497ff62726ef09a/LICENSE"
                    in label
                    for label, _ in notice.files
                )
            )
            text = "\n".join(value for _, value in notice.files)
            self.assertIn("Copyright (c) 2021 Nugine", text)

    def test_unmapped_package_cannot_use_synthetic_mit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, package = self.make_root(directory)
            index = notices.load_upstream_licenses(root)
            other = dict(package, version="0.7.0")
            other_key = (
                "uuid-simd",
                "0.7.0",
                notices._normalize_repository("https://github.com/Nugine/simd"),
            )
            with self.assertRaises(notices.NoticeError):
                notices.build_notice(other, (), index.get(other_key, ()))
            with self.assertRaises(notices.NoticeError):
                notices.build_notice(package)
            self.assertFalse(hasattr(notices, "CANONICAL_MIT"))

    def test_digest_mismatch_stops_generation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, _ = self.make_root(directory, digest="0" * 64)
            with self.assertRaises(notices.NoticeError):
                notices.load_upstream_licenses(root)


class ClosureTests(unittest.TestCase):
    def test_union_by_full_package_id_includes_client_packages(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            root_metadata, client_metadata = fixture_metadata(root)
            text = notices.generate(root, root_metadata, client_metadata)
            self.assertIn("Package: dup 1.0.0", text)
            self.assertIn("Package: dup 2.0.0", text)
            self.assertIn("Package: client-only 1.0.0", text)
            self.assertIn("Package: client-transitive 1.0.0", text)
            self.assertNotIn("Package: cayw-runtime ", text)
            self.assertNotIn("Package: cayw-client ", text)

    def test_same_name_and_version_from_different_sources_stay_distinct(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            root_metadata, client_metadata = fixture_metadata(root)
            text = notices.generate(root, root_metadata, client_metadata)
            self.assertEqual(text.count("Package: split 1.0.0"), 2)
            self.assertIn("Source: registry+https://example.invalid/index-a", text)
            self.assertIn("Source: registry+https://example.invalid/index-b", text)

    def test_fixture_generation_is_deterministic_and_checkable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            root_metadata, client_metadata = fixture_metadata(root)
            first = notices.generate(root, root_metadata, client_metadata)
            second = notices.generate(root, root_metadata, client_metadata)
            self.assertEqual(first, second)

            output = root / "THIRD-PARTY-NOTICES"
            code, _, stderr = run_main(
                [
                    "--root",
                    str(root),
                    "--metadata",
                    str(root_metadata),
                    "--client-metadata",
                    str(client_metadata),
                    "--output",
                    str(output),
                ]
            )
            self.assertEqual(code, 0, stderr)
            code, _, stderr = run_main(
                [
                    "--root",
                    str(root),
                    "--metadata",
                    str(root_metadata),
                    "--client-metadata",
                    str(client_metadata),
                    "--check",
                    str(output),
                ]
            )
            self.assertEqual(code, 0, stderr)

            output.write_text(first + "stale\n", encoding="utf-8")
            code, _, stderr = run_main(
                [
                    "--root",
                    str(root),
                    "--metadata",
                    str(root_metadata),
                    "--client-metadata",
                    str(client_metadata),
                    "--check",
                    str(output),
                ]
            )
            self.assertEqual(code, 1)
            self.assertIn("stale or differs", stderr)


class PackagingTests(unittest.TestCase):
    def test_packaging_script_wires_license_artifacts(self) -> None:
        script = (ROOT / "scripts" / "package-cli.sh").read_text(encoding="utf-8")
        self.assertIn("generate-third-party-notices.py", script)
        self.assertIn("THIRD-PARTY-NOTICES", script)
        self.assertIn("third-party/CDLA-Permissive-2.0.txt", script)
        self.assertIn("third-party/pi-MIT.txt", script)
        self.assertIn("shasum -a 256", script)


class RealMetadataTests(unittest.TestCase):
    def test_locked_workspace_and_client_metadata_resolve(self) -> None:
        root_metadata = notices.load_metadata(ROOT, None)
        client_metadata = notices.load_metadata(
            ROOT, None, ROOT / notices.CLIENT_MANIFEST
        )
        local_names = [
            package["name"]
            for package in client_metadata["packages"]
            if package.get("source") is None
        ]
        self.assertIn("cayw-client", local_names)
        root_names = {
            package["name"]
            for package in notices.reachable_third_party(
                root_metadata, notices.ROOT_PACKAGES
            )
        }
        self.assertTrue({"unicode-ident", "subtle", "ring", "jsonschema-regex"} <= root_names)
        client_names = {
            package["name"]
            for package in notices.reachable_third_party(
                client_metadata, notices.CLIENT_PACKAGES
            )
        }
        self.assertIn("serde", client_names)

    def test_current_locked_closure_generates_deterministically(self) -> None:
        text = notices.generate(ROOT)
        self.assertGreater(len(text), 100_000)
        self.assertEqual(text, notices.generate(ROOT))
        for name in ("unicode-ident", "subtle", "ring", "jsonschema-regex"):
            self.assertIn(f"Package: {name} ", text)
        for name in ("uuid-simd", "vsimd"):
            self.assertIn(f"Package: {name} 0.8.0", text)
        self.assertIn(
            "https://raw.githubusercontent.com/Nugine/simd/"
            "d74c030d9dc4f3cae02146d1f497ff62726ef09a/LICENSE",
            text,
        )
        self.assertNotIn("Package: cayw-runtime ", text)

    def test_cli_output_and_check_modes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "THIRD-PARTY-NOTICES"
            code, _, stderr = run_main(
                ["--root", str(ROOT), "--output", str(output)]
            )
            self.assertEqual(code, 0, stderr)
            self.assertTrue(output.is_file())
            self.assertGreater(output.stat().st_size, 100_000)

            code, _, stderr = run_main(
                ["--root", str(ROOT), "--check", str(output)]
            )
            self.assertEqual(code, 0, stderr)

            output.write_text(
                output.read_text(encoding="utf-8") + "stale\n",
                encoding="utf-8",
            )
            code, _, stderr = run_main(
                ["--root", str(ROOT), "--check", str(output)]
            )
            self.assertEqual(code, 1)
            self.assertIn("stale or differs", stderr)


if __name__ == "__main__":
    unittest.main()
