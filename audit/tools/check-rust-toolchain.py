#!/usr/bin/env python3
"""Inspect an already-installed standalone toolchain. Never install or download."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess

VERSION = "1.98.1"
HOST = "x86_64-unknown-linux-gnu"
TOOLS = ("rustc", "cargo", "rustdoc", "cargo-clippy", "clippy-driver", "rustfmt", "cargo-fmt")


def check(prefix: Path) -> dict:
    prefix = prefix.absolute()
    report = {
        "status": "ENV", "prefix": str(prefix), "required_version": VERSION,
        "required_host": HOST, "tools": {}, "commands": [], "errors": [],
        "warning": "Version checks are not compilation or project tests.",
    }
    root = prefix.resolve()
    # Do not invoke PATH fallbacks or rustup proxies, even to ask their versions.
    for name in TOOLS:
        path = prefix / "bin" / name
        entry = {"path": str(path), "status": "MISSING"}
        try:
            resolved = path.resolve(strict=True)
            if not resolved.is_relative_to(root) or resolved.name == "rustup":
                entry["status"] = "REJECTED_PROXY_OR_EXTERNAL_TOOL"
            elif not resolved.is_file() or not os.access(path, os.X_OK):
                entry["status"] = "NOT_EXECUTABLE"
            else:
                entry["status"] = "PRESENT"
        except (OSError, RuntimeError) as exc:
            entry["detail"] = str(exc)
        report["tools"][name] = entry
        if entry["status"] != "PRESENT":
            report["errors"].append(f"{name}: {entry['status']}")
    if report["errors"]:
        return report

    env = os.environ.copy()
    env.update({
        "PATH": str(prefix / "bin") + os.pathsep + env.get("PATH", ""),
        "CARGO_NET_OFFLINE": "true", "CARGO": str(prefix / "bin/cargo"),
        "RUSTC": str(prefix / "bin/rustc"), "RUSTDOC": str(prefix / "bin/rustdoc"),
        "RUSTFMT": str(prefix / "bin/rustfmt"),
        "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
    })
    # Run the exact four user-requested commands, plus verbose host/patch checks.
    commands = [
        ["rustc", "--version"], ["cargo", "--version"],
        ["cargo", "clippy", "--version"], ["rustfmt", "--version"],
        ["rustc", "--version", "--verbose"], ["rustdoc", "--version"],
    ]
    for args in commands:
        command = [str(prefix / "bin" / args[0]), *args[1:]]
        entry = {"command": command}
        try:
            proc = subprocess.run(command, env=env, text=True, capture_output=True,
                                  timeout=10, check=False)
            entry.update(exit_code=proc.returncode, stdout=proc.stdout.strip(),
                         stderr=proc.stderr.strip())
            if proc.returncode:
                report["errors"].append(f"{' '.join(args)}: exit {proc.returncode}")
        except (OSError, subprocess.TimeoutExpired) as exc:
            entry["error"] = str(exc)
            report["errors"].append(f"{' '.join(args)}: {type(exc).__name__}")
        report["commands"].append(entry)

    for index, label in ((0, "rustc"), (1, "cargo"), (5, "rustdoc")):
        text = report["commands"][index].get("stdout", "")
        if not re.match(rf"^{label} {re.escape(VERSION)}(?:\s|$)", text):
            report["errors"].append(f"{label}: expected exactly {VERSION}")
    verbose = report["commands"][4].get("stdout", "")
    if not re.search(rf"^host: {re.escape(HOST)}$", verbose, re.MULTILINE):
        report["errors"].append(f"rustc: expected host {HOST}")
    if not re.search(rf"^release: {re.escape(VERSION)}$", verbose, re.MULTILINE):
        report["errors"].append(f"rustc: expected stable release {VERSION}")
    # Component version schemes differ from rustc; retain their actual output.
    for index, label in ((2, "clippy"), (3, "rustfmt")):
        text = report["commands"][index].get("stdout", "")
        if not re.match(rf"^{label} \d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?(?:\s|$)", text):
            report["errors"].append(f"{label}: unrecognized version output")
    if not report["errors"]:
        report["status"] = "PASS_VERSIONS_ONLY"
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, default=Path("/mnt/data/toolchains/rust-1.98.1"))
    args = parser.parse_args()
    report = check(args.prefix)
    print(json.dumps(report, indent=2))
    return 0 if report["status"] == "PASS_VERSIONS_ONLY" else 78


if __name__ == "__main__":
    raise SystemExit(main())
