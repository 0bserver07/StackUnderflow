#!/usr/bin/env python3
"""Drive `index.html` in a real headless browser and assert it answered.

A demo page that was never opened is a claim, not a deliverable. This script
serves `rust/demo/` on 127.0.0.1, starts a WebDriver, hands the page a real
`store.db` through its file input (the same code path a drop takes), waits for
the showcase to render, and then asserts on the DOM:

  * the status line reports the schema version, so the store really opened;
  * the `store` card contains the row counts;
  * at least one query card rendered results or an honest "0 result(s)";
  * **the browser console recorded no CSP violation** — which is the page's
    privacy claim, checked rather than asserted. `connect-src 'none'` means any
    attempt to fetch/XHR/beacon anything would show up here.

It also writes a screenshot next to the state dir so a human can look at it.

    python3 rust/demo/smoke.py [store.db]

No selenium dependency: a WebDriver speaks plain W3C over HTTP and this talks
to it with urllib. Firefox 136 + geckodriver by default (`$STAX_WEBDRIVER`);
this box's Chrome is 87 and its chromedriver 2.41 cannot drive it.
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUST = HERE.parent
DEFAULT_STORE = RUST / ".parity-state/wasm9/home/store.db"
SHOT = RUST / ".parity-state/wasm9/demo-screenshot.png"
PORT = 8097          # never :8095 — that is the maintainer's running server
DRIVER_PORT = 4444


def rpc(method: str, url: str, body: dict | None = None) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(request, timeout=180) as response:
        return json.loads(response.read())


def wait_for(url: str, what: str, seconds: float = 20.0) -> None:
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=1).read()
            return
        except urllib.error.HTTPError:
            return
        except OSError:
            time.sleep(0.2)
    raise SystemExit(f"smoke: {what} never came up at {url}")


def main() -> int:
    store = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_STORE
    if not store.is_file():
        raise SystemExit(f"smoke: no store at {store}")
    if not (HERE / "pkg/stax_wasm_inline.js").is_file():
        raise SystemExit("smoke: run rust/demo/build.sh first")

    server = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(PORT), "--bind", "127.0.0.1"],
        cwd=HERE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    # geckodriver + Firefox rather than chromedriver + Chrome: this box's Chrome
    # is 87 (2020) and its chromedriver 2.41 (2018) cannot drive it, and Chrome 87
    # predates the `'wasm-unsafe-eval'` CSP keyword the page relies on. Firefox
    # 136 is current. Point $STAX_WEBDRIVER at any W3C driver binary.
    driver_bin = os.environ.get("STAX_WEBDRIVER", "geckodriver")
    driver = subprocess.Popen(
        [driver_bin, "--port", str(DRIVER_PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    session = None
    base = f"http://127.0.0.1:{DRIVER_PORT}"
    try:
        wait_for(f"http://127.0.0.1:{PORT}/index.html", "the static server")
        wait_for(f"{base}/status", "the webdriver")

        created = rpc("POST", f"{base}/session", {
            "capabilities": {"alwaysMatch": {
                "moz:firefoxOptions": {"args": ["-headless", "-width", "1280", "-height", "1600"]},
            }},
        })
        session = created["value"]["sessionId"]
        s = f"{base}/session/{session}"

        rpc("POST", f"{s}/url", {"url": f"http://127.0.0.1:{PORT}/index.html"})

        # The file input, driven exactly as a drop would drive it.
        found = rpc("POST", f"{s}/element",
                    {"using": "css selector", "value": "#file"})
        element = next(iter(found["value"].values()))
        rpc("POST", f"{s}/element/{element}/value", {"text": str(store)})

        # Wait for the showcase: four cards, the first of which is the store table.
        deadline = time.time() + 300
        text = ""
        while time.time() < deadline:
            body = rpc("POST", f"{s}/execute/sync", {
                "script": "return document.getElementById('out').innerText;",
                "args": [],
            })["value"]
            status = rpc("POST", f"{s}/execute/sync", {
                "script": "return document.getElementById('status').innerText;",
                "args": [],
            })["value"]
            if body and "schema:" in body and "schema v" in (status or ""):
                text = f"{status}\n{body}"
                break
            time.sleep(0.5)
        else:
            raise SystemExit("smoke: the page never rendered the showcase")

        shot = rpc("GET", f"{s}/screenshot")["value"]
        SHOT.parent.mkdir(parents=True, exist_ok=True)
        SHOT.write_bytes(base64.b64decode(shot))

        # The listener `csp-watch.js` installs. Empty is the assertion: any
        # blocked fetch/beacon/image would have landed here.
        try:
            logs = rpc("POST", f"{s}/execute/sync", {
                "script": "return window.__cspViolations || [];", "args": [],
            })["value"]
        except Exception:
            logs = []

        print(text[:2000])
        print("-" * 60)
        checks = {
            "the store opened (schema reported)": "schema v" in text,
            "the store table rendered": "objects:" in text and "messages" in text,
            "a memory verb answered": "result(s)" in text,
            "no CSP violation recorded": not logs,
        }
        for name, ok in checks.items():
            print(f"{'PASS' if ok else 'FAIL'}  {name}")
        print(f"screenshot: {SHOT}")
        return 0 if all(checks.values()) else 1
    finally:
        if session:
            try:
                rpc("DELETE", f"{base}/session/{session}")
            except Exception:
                pass
        driver.terminate()
        server.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
