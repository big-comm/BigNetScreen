#!/usr/bin/env python3
"""Independent Python models and static checks; DOES NOT execute Rust code."""
from __future__ import annotations
import collections
import json
from pathlib import Path
import subprocess
import sys
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]

class AckModel:
    def __init__(self, last: int = -1, checkpoint: int = -1, reference: int | None = None):
        self.last, self.checkpoint, self.reference = last, checkpoint, reference
    @property
    def pending(self) -> int:
        return self.last - self.checkpoint
    def sent(self, frame: int) -> None:
        assert self.pending < 120 and frame == self.last + 1
        self.last = frame
    def ack(self, wire: int, reference: int | None = None) -> bool:
        if reference is not None and self.reference is not None and reference < self.reference:
            return False
        expanded = (self.last & ~255) | wire
        if expanded > self.last:
            expanded -= 256
        if expanded < self.checkpoint:
            return False
        self.checkpoint = expanded
        if reference is not None:
            self.reference = reference
        return True

class WindowModel:
    def __init__(self, delay_ms: int):
        self.limit = max(66, delay_ms)
        self.frames: collections.deque[tuple[int, int]] = collections.deque()
    def room(self, pts: int, acknowledged: int) -> bool:
        while self.frames and self.frames[0][0] <= acknowledged:
            self.frames.popleft()
        return not self.frames or 0 <= pts - self.frames[0][1] <= self.limit

class ModelTests(unittest.TestCase):
    def test_startup_and_first_ack(self):
        w = AckModel()
        self.assertTrue(w.ack(255))
        self.assertFalse(w.ack(0))
        for i in range(120):
            w.sent(i)
        self.assertEqual(w.pending, 120)
        with self.assertRaises(AssertionError):
            w.sent(120)
        self.assertTrue(w.ack(0))
        w.sent(120)
        self.assertEqual(w.pending, 120)
    def test_477120_wrapping_windows(self):
        checked = 0
        for last in range(120, 4096):
            for distance in range(1, 121):
                w = AckModel(last, last - distance)
                self.assertTrue(w.ack((last - 1) & 255))
                self.assertEqual(w.pending, 1)
                self.assertFalse(w.ack((last - 2) & 255))
                checked += 1
        self.assertEqual(checked, 477120)
    def test_xr_rejects_previous_wire_cycle(self):
        w = AckModel(1000, 990, 500)
        self.assertFalse(w.ack(999 & 255, 499))
        self.assertEqual(w.pending, 10)
        self.assertTrue(w.ack(999 & 255, 501))
        self.assertEqual(w.pending, 1)
    def test_media_time_and_release(self):
        w = WindowModel(150)
        w.frames.append((0, 0))
        self.assertTrue(w.room(150, -1))
        self.assertFalse(w.room(151, -1))
        self.assertTrue(w.room(10000, 0))
        w.frames.append((1, 10000))
        self.assertFalse(w.room(9999, 0))
        self.assertEqual(WindowModel(20).limit, 66)
    def test_resync_never_advances_dropped_ids(self):
        frame_id, resync, requested = 0, True, False
        emitted = []
        # (has_room, keyframe): startup, congestion, P-frame rejection, recovery.
        sequence = [(True, False), (True, True), (True, False),
                    (False, True), (False, False), (True, False), (True, True)]
        for room, key in sequence:
            if not room:
                resync, requested = True, False
                continue
            if resync and not key:
                requested = True
                continue
            if resync:
                resync, requested = False, False
            emitted.append((frame_id, None if key else frame_id - 1))
            frame_id += 1
        self.assertEqual(emitted, [(0, None), (1, 0), (2, None)])
        self.assertFalse(resync or requested)
    def test_burst_units_and_packet_ceilings(self):
        self.assertEqual((24 << 20) * 10 // 1000 // 8, 31457)
        self.assertEqual(31457 // 1200, 26)
        self.assertEqual((908116 + 1180) // 1181, 769)
        self.assertEqual((66105 + 1180) // 1181, 56)


def static_checks() -> dict:
    files = subprocess.check_output(['git', 'ls-files', '-z'], cwd=ROOT).decode().split('\0')
    paths = [ROOT / f for f in files if f]
    tomls = [p for p in paths if p.suffix == '.toml' or p.name == 'Cargo.lock']
    for p in tomls:
        tomllib.loads(p.read_text())
    result = {'toml_files_parsed': len(tomls), 'rust_execution': 'NOT_RUN_NO_TOOLCHAIN'}
    try:
        from pygments import lex
        from pygments.lexers import RustLexer
        from pygments.token import Comment, String
    except ImportError:
        result['rust_delimiters'] = 'SKIPPED_NO_PYGMENTS'
    else:
        rust = [p for p in paths if p.suffix == '.rs']
        pairs = {')': '(', ']': '[', '}': '{'}
        for p in rust:
            stack = []
            for token, text in lex(p.read_text(), RustLexer()):
                if token in Comment.Single or token in Comment.Multiline or token in String:
                    continue
                for c in text:
                    if c in '([{':
                        stack.append(c)
                    elif c in pairs:
                        assert stack and stack.pop() == pairs[c], str(p)
            assert not stack, str(p)
        result['rust_delimiters'] = {'files': len(rust), 'status': 'PASS_LEXICAL_ONLY_NOT_TYPECHECK'}
    return result

if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(ModelTests))
    checks = static_checks()
    checks['python_model_tests'] = result.testsRun
    checks['python_model_failures'] = len(result.failures) + len(result.errors)
    checks['warning'] = 'Python models/static checks are not Rust build or regression-test execution.'
    print(json.dumps(checks, indent=2))
    sys.exit(0 if result.wasSuccessful() else 1)
