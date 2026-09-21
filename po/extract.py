#!/usr/bin/env python3
"""Extract `tr!()`/`tr_n!()` strings from the Rust sources into a POT file.

This exists because `xgettext` with its C parser reads a Rust lifetime
(`'static`) as an unterminated character constant and aborts the extraction.
"""
from __future__ import annotations

import os
import re
import tomllib
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
POTFILES = ROOT / "po" / "POTFILES.in"
OUTPUT = ROOT / "po" / "bignetscreen.pot"

# tr!("text") and tr!("text", args...) — the string may span several lines.
SINGULAR = re.compile(r'\btr!\(\s*"((?:[^"\\]|\\.)*)"', re.S)
PLURAL = re.compile(r'\btr_n!\(\s*"((?:[^"\\]|\\.)*)"\s*,\s*"((?:[^"\\]|\\.)*)"', re.S)


def _timestamp() -> str:
    """POT-Creation-Date in the format gettext uses."""
    from datetime import datetime, timezone

    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch is not None:
        try:
            value = int(epoch)
            if value < 0:
                raise ValueError("negative epoch")
            return datetime.fromtimestamp(value, timezone.utc).strftime("%Y-%m-%d %H:%M%z")
        except (ValueError, OverflowError, OSError) as error:
            raise ValueError("SOURCE_DATE_EPOCH must be a non-negative supported Unix timestamp") from error
    if OUTPUT.exists():
        match = re.search(r'POT-Creation-Date: ([^"\\]+)', OUTPUT.read_text(encoding="utf-8"))
        if match:
            return match.group(1)
    # No clock-dependent output: callers making a new template can set the epoch.
    return "1970-01-01 00:00+0000"


def clean(raw: str) -> str:
    """Resolve escapes and join Rust line continuations (`\\` + newline)."""
    text = re.sub(r"\\\s*\n\s*", "", raw)
    return text.replace('\\"', '"').replace("\\\\", "\\")


def escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def main() -> int:
    stamp = _timestamp()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not POTFILES.exists():
        print(f"{POTFILES} not found", file=sys.stderr)
        return 1

    sources = [
        ROOT / line.strip()
        for line in POTFILES.read_text().splitlines()
        if line.strip() and not line.startswith("#") and line.strip().endswith(".rs")
    ]

    # msgid -> (references, optional plural)
    entries: dict[str, tuple[list[str], str | None]] = {}

    for path in sources:
        if not path.exists():
            print(f"error: listed translation source {path} does not exist", file=sys.stderr)
            return 1
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)

        for match in PLURAL.finditer(text):
            line = text[: match.start()].count("\n") + 1
            singular, plural = clean(match.group(1)), clean(match.group(2))
            refs, _ = entries.get(singular, ([], None))
            refs.append(f"{rel}:{line}")
            entries[singular] = (refs, plural)

        for match in SINGULAR.finditer(text):
            line = text[: match.start()].count("\n") + 1
            msgid = clean(match.group(1))
            refs, plural = entries.get(msgid, ([], None))
            refs.append(f"{rel}:{line}")
            entries[msgid] = (refs, plural)

    # The header must NOT be marked `fuzzy`, and its fields must not be left at
    # the template defaults. gettext ignores the metadata of a fuzzy header, and
    # tooling that reads the catalogue then sees a project with no name at all —
    # which is exactly how translation tools end up reporting an empty
    # textdomain.
    out = [
        "# BigNetScreen translation template.",
        "# Copyright (C) 2026 BigCommunity",
        "# This file is distributed under the same license as BigNetScreen.",
        "#",
        'msgid ""',
        'msgstr ""',
        f'"Project-Id-Version: bignetscreen {version}\\n"',
        '"Report-Msgid-Bugs-To: https://github.com/big-comm/BigNetScreen/issues\\n"',
        f'"POT-Creation-Date: {stamp}\\n"',
        '"PO-Revision-Date: YEAR-MO-DA HO:MI+ZONE\\n"',
        '"Last-Translator: BigCommunity <contato@communitybig.org>\\n"',
        '"Language-Team: BigCommunity <contato@communitybig.org>\\n"',
        '"Language: \\n"',
        '"MIME-Version: 1.0\\n"',
        '"Content-Type: text/plain; charset=UTF-8\\n"',
        '"Content-Transfer-Encoding: 8bit\\n"',
        '"Plural-Forms: nplurals=2; plural=(n != 1);\\n"',
        "",
    ]

    for msgid in sorted(entries):
        refs, plural = entries[msgid]
        out.append(f"#: {' '.join(refs)}")
        out.append(f'msgid "{escape(msgid)}"')
        if plural is not None:
            out.append(f'msgid_plural "{escape(plural)}"')
            out.append('msgstr[0] ""')
            out.append('msgstr[1] ""')
        else:
            out.append('msgstr ""')
        out.append("")

    OUTPUT.write_text("\n".join(out), encoding="utf-8")
    print(f"{len(entries)} strings extracted into {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
