#!/usr/bin/env python3
"""Capture the ignored GTK layout test through Broadway; requires Playwright."""

import argparse
import time
from pathlib import Path
from playwright.sync_api import sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("directory", type=Path, help="Same fresh directory as BIGNETSCREEN_LAYOUT_DIR")
parser.add_argument("--url", default="http://127.0.0.1:19091")
parser.add_argument("--browser", help="Chromium executable; defaults to Playwright's browser")
args = parser.parse_args()
args.directory.mkdir(parents=True, exist_ok=True)

with sync_playwright() as playwright:
    browser = playwright.chromium.launch(executable_path=args.browser, headless=True)
    page = browser.new_page(viewport={"width": 1280, "height": 960})
    page.goto(args.url)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        page.wait_for_timeout(100)
        marker = args.directory / "ready"
        if not marker.exists():
            continue
        name = marker.read_text().strip()
        if not name or Path(name).name != name:
            continue
        target = args.directory / f"{name}.png"
        if target.exists():
            continue
        page.wait_for_timeout(500)
        page.screenshot(path=str(target))
        if name == "settings-compact":
            break
    else:
        raise TimeoutError("Layout test did not finish within 90 seconds")
    browser.close()
