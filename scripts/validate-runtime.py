#!/usr/bin/env python3
"""Record local offline validation and the exact Cargo source inputs.

Usage: python3 -I scripts/validate-runtime.py --out /tmp/cayw-validation
This never invokes live-model examples or modifies remote services.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inventory(root: Path, cargo: list[str]) -> dict[str, str]:
    metadata = json.loads(subprocess.check_output(
        [*cargo, "metadata", "--format-version", "1", "--no-deps", "--locked"],
        cwd=root, text=True, stderr=subprocess.PIPE,
    ))
    # Cargo owns the package input inventory; no source search is needed.
    paths = {root / "Cargo.toml", root / "Cargo.lock", root / "LICENSE", Path(__file__).resolve(),
             root / "scripts/validate-linux.sh", root / "scripts/validate-headless.py",
             root / "scripts/serve-webui-validation.py", root / "scripts/validate-webui.mjs"}
    for package in metadata["packages"]:
        manifest = Path(package["manifest_path"])
        package_root = manifest.parent
        paths.add(manifest)
        listing = subprocess.check_output(
            [*cargo, "package", "--list", "--allow-dirty", "--locked", "--manifest-path", str(manifest)],
            cwd=root, text=True, stderr=subprocess.PIPE,
        )
        for name in listing.splitlines():
            path = package_root / name
            # Cargo adds generated manifest and VCS entries that need not exist on disk.
            if path.is_file():
                if path.is_symlink():
                    raise ValueError(f"package input is a symlink: {path.relative_to(root)}")
                paths.add(path)
    return {str(path.relative_to(root)): sha256(path) for path in sorted(paths) if path.is_file()}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cargo", default=shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo"))
    parser.add_argument("--toolchain", default="1.95.0")
    parser.add_argument("--timeout", type=int, default=1800, help="maximum seconds for each command")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    root = Path(__file__).resolve().parent.parent
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cargo = [args.cargo, f"+{args.toolchain}"]
    summary = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version,
        "mode": "offline; no real-model requests",
        "commands": [],
        "failures": [],
        "passed": False,
    }
    try:
        sources = inventory(root, cargo)
        (output / "sources.json").write_text(json.dumps(sources, indent=2) + "\n")
        summary["source_count"] = len(sources)
        summary["source_manifest_sha256"] = sha256(output / "sources.json")
        checks = [
            ("rustc", [str(Path(args.cargo).with_name("rustc")), f"+{args.toolchain}", "--version", "--verbose"]),
            ("cargo", [*cargo, "--version"]),
            ("fmt", [*cargo, "fmt", "--all", "--", "--check"]),
            ("check", [*cargo, "check", "--workspace", "--all-targets", "--all-features", "--locked"]),
            ("tests", [*cargo, "test", "--workspace", "--all-features", "--locked"]),
            ("minimal-tests", [*cargo, "test", "--workspace", "--no-default-features", "--locked"]),
            ("clippy", [*cargo, "clippy", "--workspace", "--all-targets", "--all-features", "--locked", "--", "-D", "warnings"]),
            ("offline-example", [*cargo, "run", "-p", "cayw-agent", "--example", "offline", "--locked"]),
            ("headless-example", [*cargo, "run", "-p", "cayw-runtime", "--example", "offline", "--locked", "--", str(output / "headless-demo")]),
            ("build-headless", [*cargo, "build", "-p", "cayw-runtime", "--bin", "cayw", "--locked"]),
            ("independent-client", [sys.executable, "-I", str(root / "scripts/validate-headless.py"),
                                    "--binary", str(root / "target/debug" / ("cayw.exe" if sys.platform == "win32" else "cayw")),
                                    "--out", str(output / "independent-client")]),
            ("web-state", [shutil.which("node") or "node", "--test", str(root / "crates/cayw-webui/tests/state.test.mjs")]),
        ]
        for name, command in checks:
            print(f"Running {name}", flush=True)
            started = datetime.now(timezone.utc)
            with (output / f"{name}.log").open("w") as log:
                try:
                    result = subprocess.run(command, cwd=root, stdout=log, stderr=subprocess.STDOUT, check=False, timeout=args.timeout)
                    exit_code = result.returncode
                except subprocess.TimeoutExpired:
                    exit_code = 124
                    log.write(f"\nValidation command timed out after {args.timeout} seconds.\n")
            finished = datetime.now(timezone.utc)
            summary["commands"].append({"name": name, "command": command, "exit_code": exit_code,
                                        "started_utc": started.isoformat(), "finished_utc": finished.isoformat(),
                                        "duration_seconds": (finished - started).total_seconds()})
            if exit_code:
                summary["failures"].append(name)
            print(f"{name}: exit {exit_code}", flush=True)
        after = inventory(root, cargo)
        changes = sorted(name for name in sources.keys() | after.keys() if sources.get(name) != after.get(name))
        summary["changed_during_validation"] = changes
        summary["passed"] = not summary["failures"] and not changes
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        summary["failures"].append("validation-runner")
        summary["error"] = str(error)
        if isinstance(error, subprocess.CalledProcessError):
            (output / "runner-error.log").write_text(error.stderr or str(error))
    summary["finished_utc"] = datetime.now(timezone.utc).isoformat()
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"passed": summary["passed"], "failures": summary["failures"],
                      "changed_sources": summary.get("changed_during_validation", []), "output": str(output)}), flush=True)
    return 0 if summary["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
