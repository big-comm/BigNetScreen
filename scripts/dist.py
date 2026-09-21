#!/usr/bin/env python3
"""Archive a clean, committed worktree with its checksum and provenance.

For attaching an immutable source revision to a release. The distribution
package builds from the Git branch instead, so no PKGBUILD is generated here.

No network, service changes, builds or package installation. Python >= 3.11.
The output directory must not exist, so no previous delivery is overwritten.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True, timeout=30).strip()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if git("status", "--porcelain", "--untracked-files=normal"):
        parser.error("commit and verify the worktree before generating a release input")
    head = git("rev-parse", "HEAD")
    epoch = int(git("show", "-s", "--format=%ct", "HEAD"))
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){2}", version):
        parser.error("unsupported package version syntax")
    output = args.output.resolve()
    if output.exists():
        parser.error("output already exists; choose a new directory")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".bns-dist-", dir=output.parent) as temporary:
        staging = Path(temporary)
        source_name = f"bignetscreen-{version}.tar.gz"
        tar = staging / "source.tar"
        subprocess.run(["git", "-C", str(ROOT), "archive", "--format=tar", f"--prefix=bignetscreen-{version}/", "--output", str(tar), head], check=True, timeout=60)
        with tar.open("rb") as src, (staging / source_name).open("wb") as target:
            with gzip.GzipFile(filename="", mode="wb", fileobj=target, mtime=epoch) as compressed:
                shutil.copyfileobj(src, compressed, length=1024 * 1024)
        tar.unlink()
        with (staging / source_name).open("rb") as stream:
            checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        metadata = {"commit": head, "version": version, "SOURCE_DATE_EPOCH": epoch, "source": source_name, "sha256": checksum, "binary_build_verified": False}
        (staging / "source-build.json").write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
        checksums = []
        for file in sorted(staging.iterdir()):
            with file.open("rb") as stream:
                checksums.append(f"{hashlib.file_digest(stream, 'sha256').hexdigest()}  {file.name}\n")
        (staging / "SHA256SUMS").write_text("".join(checksums), encoding="ascii")
        # Exclusive destination creation prevents clobbering an existing delivery.
        output.mkdir()
        for file in staging.iterdir():
            shutil.move(str(file), str(output / file.name))
    print(json.dumps(metadata, indent=2))
    print(f"Generated source input only: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
