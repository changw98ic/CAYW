#!/usr/bin/env python3
"""Run cargo, then delete old copies that this run did not use.

A file is removed only when this run used the same crate and a different
hash. Artifacts for crates this run did not build are left in place.

Usage:
  scripts/sweep-target.py +1.95.0 test --workspace --locked
  scripts/sweep-target.py --dry-run +1.95.0 check -p cayw-ai --locked --offline
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

HASH = re.compile(r"-([0-9a-f]{16})(?=\.|$)")
COMPILE_COMMANDS = {"b", "bench", "build", "c", "check", "clippy", "r", "run", "rustc", "t", "test"}


def cargo_command(args: list[str]) -> list[str]:
    command = ["cargo", *args]
    if any(arg == "--message-format" or arg.startswith("--message-format=") for arg in args):
        return command
    for index, arg in enumerate(args):
        if not arg.startswith(("+", "-")):
            command.insert(index + 2, "--message-format=json")
            break
    return command


def subcommand(args: list[str]) -> str | None:
    for arg in args:
        if not arg.startswith(("+", "-")):
            return arg
    return None


def stem_and_hash(name: str) -> tuple[str, str] | None:
    match = HASH.search(name)
    if not match:
        return None
    return name[: match.start()], match.group(1)


def record(message: dict, files: set[Path], directories: set[Path]) -> None:
    if message.get("reason") == "compiler-artifact":
        paths = list(message.get("filenames") or [])
        if message.get("executable"):
            paths.append(message["executable"])
        files.update(Path(raw) for raw in paths)
    elif message.get("reason") == "build-script-executed" and message.get("out_dir"):
        directories.add(Path(message["out_dir"]))


def show(message: dict, forward_json: bool, raw: str) -> None:
    if forward_json:
        print(raw)
        return
    reason = message.get("reason")
    if reason == "compiler-artifact":
        state = "Fresh" if message.get("fresh") else "Compiling"
        print(f"{state} {message.get('target', {}).get('name', '')}", file=sys.stderr)
    elif reason == "compiler-message":
        rendered = message.get("message", {}).get("rendered")
        if rendered:
            print(rendered, end="" if rendered.endswith("\n") else "\n", file=sys.stderr)


def directory_size(path: Path) -> int:
    if path.is_file():
        return path.stat().st_size
    total = 0
    for child in path.rglob("*"):
        if child.is_file() and not child.is_symlink():
            total += child.stat().st_size
    return total


def sweep(files: set[Path], directories: set[Path], dry_run: bool) -> tuple[int, int]:
    protected = {path.resolve() for path in directories}
    protected.update(parent for path in list(protected) for parent in path.parents)
    deps_dirs = {path.parent for path in files if path.parent.name == "deps"}
    removed = 0
    bytes_removed = 0

    def drop(path: Path) -> None:
        nonlocal removed, bytes_removed
        bytes_removed += directory_size(path)
        removed += 1
        if dry_run or not path.exists():
            return
        if path.is_dir():
            subprocess.run(["rm", "-rf", "--", str(path)], check=True)
        else:
            path.unlink()

    for deps in deps_dirs:
        live: dict[str, set[str]] = {}
        for path in files:
            if path.parent != deps:
                continue
            parsed = stem_and_hash(path.name)
            if parsed:
                live.setdefault(parsed[0], set()).add(parsed[1])
        obsolete: set[str] = set()
        for path in list(deps.iterdir()):
            if not path.is_file() or path.is_symlink():
                continue
            parsed = stem_and_hash(path.name)
            if not parsed:
                continue
            stem, digest = parsed
            if stem in live and digest not in live[stem]:
                obsolete.add(digest)
                drop(path)
        for kind in (".fingerprint", "build"):
            root = deps.parent / kind
            if not root.is_dir():
                continue
            for path in list(root.iterdir()):
                if path.is_symlink():
                    continue
                parsed = stem_and_hash(path.name)
                if not parsed or parsed[1] not in obsolete:
                    continue
                if path.resolve() in protected:
                    continue
                drop(path)
    return removed, bytes_removed


def main(argv: list[str]) -> int:
    dry_run = False
    if argv and argv[0] == "--dry-run":
        dry_run = True
        argv = argv[1:]
    if not argv:
        print(
            "usage: scripts/sweep-target.py [--dry-run] CARGO_ARGS...",
            file=sys.stderr,
        )
        return 2
    forward_json = any(
        arg == "--message-format" or arg.startswith("--message-format=") for arg in argv
    )
    files: set[Path] = set()
    directories: set[Path] = set()
    process = subprocess.Popen(cargo_command(argv), stdout=subprocess.PIPE, text=True, bufsize=1)
    assert process.stdout is not None
    for raw in process.stdout:
        line = raw.rstrip("\n")
        if not line.startswith("{"):
            print(raw, end="" if raw.endswith("\n") else "\n", file=sys.stderr)
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            print(raw, end="" if raw.endswith("\n") else "\n", file=sys.stderr)
            continue
        record(message, files, directories)
        show(message, forward_json, line)
    code = process.wait()
    if code != 0 or subcommand(argv) not in COMPILE_COMMANDS:
        return code
    if not files:
        print("no artifacts reported; left target unchanged", file=sys.stderr)
        return 0
    removed, total = sweep(files, directories, dry_run)
    action = "would remove" if dry_run else "removed"
    print(f"{action} {removed} stale artifacts, {total / 1024 / 1024:.1f} MiB", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
