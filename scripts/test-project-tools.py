#!/usr/bin/env python3
"""Fast, isolated regression tests for the translation/release helpers."""
import contextlib
import importlib.util
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("bns_extract", ROOT / "po/extract.py")
extract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extract)


class TranslationExtraction(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        (self.root / "po").mkdir()
        (self.root / "src").mkdir()
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.4"\n')
        (self.root / "po/POTFILES.in").write_text("src/main.rs\n")
        (self.root / "src/main.rs").write_text('tr!("Open"); tr!("Open"); tr_n!("{} file", "{} files", count);\n')
        self.output = self.root / "po/bignetscreen.pot"
        self.patchers = [patch.object(extract, "ROOT", self.root), patch.object(extract, "POTFILES", self.root / "po/POTFILES.in"), patch.object(extract, "OUTPUT", self.output), patch.dict(os.environ, {}, clear=True)]
        for patcher in self.patchers:
            patcher.start()
        self.addCleanup(self.temp.cleanup)
        for patcher in self.patchers:
            self.addCleanup(patcher.stop)

    def test_repeated_extraction_is_identical_and_deduplicates_strings(self):
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(extract.main(), 0)
            first = self.output.read_bytes()
            self.assertEqual(extract.main(), 0)
        self.assertEqual(first, self.output.read_bytes())
        self.assertEqual(first.count(b'msgid "Open"'), 1)
        self.assertIn(b'Project-Id-Version: bignetscreen 0.1.4', first)
        self.assertIn(b'msgid_plural "{} files"', first)

    def test_explicit_epoch_is_utc_and_reproducible(self):
        with patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "0"}):
            self.assertEqual(extract._timestamp(), "1970-01-01 00:00+0000")

    def test_invalid_epoch_is_not_silently_replaced_by_current_time(self):
        for epoch in ("-1", "not-a-timestamp", "9" * 60):
            with patch.dict(os.environ, {"SOURCE_DATE_EPOCH": epoch}), self.assertRaises(ValueError):
                extract._timestamp()

    def test_missing_listed_source_does_not_erase_the_existing_template(self):
        self.output.write_text('"POT-Creation-Date: 2026-09-20 00:00+0000\\n"\nKEEP\n')
        old = self.output.read_bytes()
        (self.root / "src/main.rs").unlink()
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(extract.main(), 1)
        self.assertEqual(self.output.read_bytes(), old)

    def test_unlisted_source_with_strings_does_not_rewrite_the_template(self):
        self.output.write_text("KEEP\n")
        extra = self.root / "crates/nd-gui/src/extra.rs"
        extra.parent.mkdir(parents=True)
        extra.write_text('tr!("Close");\n')
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors):
            self.assertEqual(extract.main(), 1)
        self.assertIn("crates/nd-gui/src/extra.rs", errors.getvalue())
        self.assertEqual(self.output.read_text(), "KEEP\n")


if __name__ == "__main__":
    unittest.main()
