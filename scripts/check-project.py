#!/usr/bin/env python3
"""Check maintained docs, source hygiene and release metadata without building.

Uses only Python >= 3.11 standard library. External links and hardware are not
validated. Generated/ignored build trees are deliberately excluded.
"""
from __future__ import annotations

import ast
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    errors: list[str] = []
    listed = subprocess.check_output(["git", "-C", str(ROOT), "ls-files", "-z", "--cached", "--others", "--exclude-standard"], timeout=20)
    paths = sorted({Path(name.decode()) for name in listed.split(b"\0") if name})
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    for package in lock["package"]:
        if package["name"].startswith("nd-") and package["version"] != version:
            errors.append(f"Cargo.lock: inconsistent {package['name']} version")
    # pkgbuild/PKGBUILD deliberately derives pkgver/pkgrel from the build clock:
    # the distribution publishes rolling builds of a moving branch, so it carries
    # no workspace version to compare against.
    if 'env!("BIGNETSCREEN_VERSION")' not in (ROOT / "crates/nd-gui/src/main.rs").read_text():
        errors.append("GUI version is not the build date stamped by build.rs")
    if "+%y.%m.%d" not in (ROOT / "crates/nd-gui/build.rs").read_text():
        errors.append("build.rs no longer stamps the PKGBUILD's pkgver format")
    for required in ("README.md", "README.pt-BR.md", "AGENTS.md", "CONTRIBUTING.md", "SECURITY.md", "SUPPORT.md", "ARCHITECTURE.md", "vendor/gst-plugin-ndi/Cargo.toml"):
        if not (ROOT / required).is_file():
            errors.append(f"required tracked source/document missing: {required}")
    checked_docs = 0
    for relative in paths:
        path = ROOT / relative
        if not path.exists():
            # A staged/tracked deletion is checked by Git, not read as a file.
            continue
        if relative.name == "CLAUDE.md" or relative.suffix in (".rej", ".orig", ".pyc", ".swp"):
            errors.append(f"development residue: {relative}")
        if path.is_symlink() or not path.is_file():
            continue
        if relative.suffix == ".py":
            try:
                ast.parse(path.read_text(encoding="utf-8"), filename=str(relative))
            except (SyntaxError, UnicodeError) as error:
                errors.append(str(error))
        if relative.parts[0] == "vendor":
            continue  # upstream keeps its own formatting/documentation policy
        if relative.suffix in (".rs", ".sh", ".py") and re.search(r"/usr/lib/python3\.\d+/site-packages", path.read_text(encoding="utf-8")):
            errors.append(f"version-pinned Python runtime path: {relative}")
        if relative.suffix != ".md":
            continue
        # Audit evidence/history records old layouts and is explicitly not
        # maintained guidance; check only currently maintained Markdown links.
        if relative.parts[0] == "audit" or "history" in relative.parts:
            continue
        text = path.read_text(encoding="utf-8")
        checked_docs += 1
        fences = re.findall(r"^\s*```", text, re.M)
        if len(fences) % 2:
            errors.append(f"unclosed Markdown code fence: {relative}")
        plain = re.sub(r"```[^\n]*\n.*?```", "", text, flags=re.S)
        for target in re.findall(r"!?\[[^\]]*\]\(([^\s)]+)(?:\s+[^)]*)?\)", plain):
            url = urlsplit(target.strip("<>"))
            if url.scheme or target.startswith(("#", "//")):
                continue
            if url.path and not (path.parent / unquote(url.path)).exists():
                errors.append(f"missing local link: {relative} -> {target}")
    for error in errors:
        print(f"FAIL: {error}", file=sys.stderr)
    print(f"checked {len(paths)} source paths, {checked_docs} maintained Markdown documents, version {version}; errors={len(errors)}")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
