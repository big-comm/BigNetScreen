#!/usr/bin/env python3
"""Unit tests of the preflight checker using mocks. No Rust is executed here."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

FILE = Path(__file__).with_name("check-rust-toolchain.py")
SPEC = importlib.util.spec_from_file_location("rust_check", FILE)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class PreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.prefix = Path(self.temp.name) / "isolated prefix"
        (self.prefix / "bin").mkdir(parents=True)
        for name in CHECK.TOOLS:
            path = self.prefix / "bin" / name
            path.write_text("unit-test placeholder, never executed\n")
            path.chmod(0o700)
        self.outputs = {
            ("rustc", "--version"): "rustc 1.98.1 (test 2026-09-01)",
            ("rustc", "--version", "--verbose"): "rustc 1.98.1\nhost: x86_64-unknown-linux-gnu\nrelease: 1.98.1",
            ("cargo", "--version"): "cargo 1.98.1 (test 2026-09-01)",
            ("cargo", "clippy", "--version"): "clippy 0.1.98 (test 2026-09-01)",
            ("rustfmt", "--version"): "rustfmt 1.9.0-stable (test 2026-09-01)",
            ("rustdoc", "--version"): "rustdoc 1.98.1 (test 2026-09-01)",
        }

    def invoke(self, argv, **kwargs):
        self.assertTrue(Path(argv[0]).is_relative_to(self.prefix))
        self.assertEqual(kwargs["env"]["CARGO_NET_OFFLINE"], "true")
        self.assertEqual(kwargs["env"]["RUSTC"], str(self.prefix / "bin/rustc"))
        self.assertEqual(kwargs["env"]["RUSTFMT"], str(self.prefix / "bin/rustfmt"))
        self.assertEqual(kwargs["env"]["RUSTC_WRAPPER"], "")
        self.assertEqual(kwargs["env"]["RUSTC_WORKSPACE_WRAPPER"], "")
        key = (Path(argv[0]).name, *argv[1:])
        return SimpleNamespace(returncode=0, stdout=self.outputs[key], stderr="")

    def inspect(self):
        with patch.object(CHECK.subprocess, "run", side_effect=self.invoke):
            return CHECK.check(self.prefix)

    def test_exact_versions_and_stable_rustfmt_suffix(self):
        report = self.inspect()
        self.assertEqual(report["status"], "PASS_VERSIONS_ONLY")
        self.assertEqual(len(report["commands"]), 6)

    def test_missing_prefix_never_invokes_path_tools(self):
        with patch.object(CHECK.subprocess, "run") as run:
            report = CHECK.check(self.prefix / "absent")
        run.assert_not_called()
        self.assertEqual(report["status"], "ENV")

    def test_external_symlink_rejected_before_execution(self):
        path = self.prefix / "bin/rustc"
        path.unlink()
        outside = Path(self.temp.name) / "rustc"
        outside.write_text("not executed")
        outside.chmod(0o700)
        path.symlink_to(outside)
        with patch.object(CHECK.subprocess, "run") as run:
            report = CHECK.check(self.prefix)
        run.assert_not_called()
        self.assertEqual(report["tools"]["rustc"]["status"], "REJECTED_PROXY_OR_EXTERNAL_TOOL")

    def test_rustup_symlink_rejected_even_inside_prefix(self):
        target = self.prefix / "bin/rustup"
        target.write_text("not executed")
        target.chmod(0o700)
        (self.prefix / "bin/rustc").unlink()
        (self.prefix / "bin/rustc").symlink_to("rustup")
        with patch.object(CHECK.subprocess, "run") as run:
            report = CHECK.check(self.prefix)
        run.assert_not_called()
        self.assertEqual(report["status"], "ENV")

    def test_nonexecutable_rejected(self):
        (self.prefix / "bin/rustc").chmod(0o600)
        self.assertEqual(CHECK.check(self.prefix)["tools"]["rustc"]["status"], "NOT_EXECUTABLE")

    def test_missing_components_rejected_before_execution(self):
        for tool in ("cargo-clippy", "clippy-driver", "cargo-fmt", "rustfmt", "rustdoc"):
            with self.subTest(tool=tool):
                path = self.prefix / "bin" / tool
                path.rename(path.with_suffix(".saved"))
                with patch.object(CHECK.subprocess, "run") as run:
                    report = CHECK.check(self.prefix)
                run.assert_not_called()
                self.assertEqual(report["status"], "ENV")
                path.with_suffix(".saved").rename(path)

    def test_wrong_patch_and_prerelease_versions_rejected(self):
        for label in ("rustc", "cargo", "rustdoc"):
            key = (label, "--version")
            saved = self.outputs[key]
            for version in ("1.98.0", "1.98.10", "1.98.1-nightly", "1.99.0"):
                with self.subTest(label=label, version=version):
                    self.outputs[key] = f"{label} {version}"
                    self.assertEqual(self.inspect()["status"], "ENV")
            self.outputs[key] = saved

    def test_wrong_host_rejected(self):
        key = ("rustc", "--version", "--verbose")
        self.outputs[key] = self.outputs[key].replace(CHECK.HOST, "aarch64-unknown-linux-gnu")
        self.assertEqual(self.inspect()["status"], "ENV")

    def test_nonstable_verbose_release_rejected(self):
        key = ("rustc", "--version", "--verbose")
        self.outputs[key] = self.outputs[key].replace("release: 1.98.1", "release: 1.98.1-dev")
        self.assertEqual(self.inspect()["status"], "ENV")

    def test_component_execution_failure_not_success(self):
        with patch.object(CHECK.subprocess, "run", return_value=SimpleNamespace(
                returncode=1, stdout="", stderr="missing shared library")):
            self.assertEqual(CHECK.check(self.prefix)["status"], "ENV")

    def test_timeout_not_success(self):
        with patch.object(CHECK.subprocess, "run", side_effect=subprocess.TimeoutExpired("tool", 10)):
            self.assertEqual(CHECK.check(self.prefix)["status"], "ENV")

    def test_real_cli_missing_prefix_exit_78_and_json(self):
        import sys
        proc = subprocess.run([sys.executable, str(FILE), "--prefix", str(self.prefix / "absent")],
                              capture_output=True, text=True, timeout=10, check=False)
        self.assertEqual(proc.returncode, 78)
        self.assertEqual(json.loads(proc.stdout)["status"], "ENV")

    def test_caller_compiler_and_wrapper_overrides_are_not_used(self):
        with patch.dict(os.environ, {"RUSTC": "/wrong/rustc", "RUSTFMT": "/wrong/rustfmt",
                                     "RUSTC_WRAPPER": "rustup", "RUSTC_WORKSPACE_WRAPPER": "rustup"}):
            self.assertEqual(self.inspect()["status"], "PASS_VERSIONS_ONLY")


if __name__ == "__main__":
    unittest.main(verbosity=2)
