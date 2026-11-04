#!/usr/bin/env python3
"""Black-box smoke test of a packaged app; no account or external network needed."""
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import uuid


def wait_for(check, process, seconds=25):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f"App exited unexpectedly: {process.returncode}")
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError):
            pass
        time.sleep(0.1)
    raise AssertionError("Timed out waiting for packaged app")


def smoke(executable, unavailable_home):
    with tempfile.TemporaryDirectory(prefix="versadown-macos-smoke-") as temporary:
        root = Path(temporary)
        data = root / "data"
        data.mkdir()
        downloads = root / "downloads"
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        token = "smoke-" + uuid.uuid4().hex
        settings = {"aria2": {"rpcPort": port, "rpcToken": token},
                    "privacyPasswordConfigured": unavailable_home}
        (data / "settings.json").write_text(json.dumps(settings))
        environment = dict(os.environ, VERSADOWN_BACKGROUND_UI_TEST="1",
                           VERSADOWN_TEST_DATA_DIR=str(data),
                           VERSADOWN_TEST_DOWNLOAD_DIR=str(downloads))
        if unavailable_home:
            home = root / "empty-home"
            home.mkdir()
            environment["HOME"] = str(home)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def rpc(method, *arguments):
            body = json.dumps({"jsonrpc": "2.0", "id": "smoke", "method": method,
                               "params": ["token:" + token, *arguments]}).encode()
            request = urllib.request.Request(f"http://127.0.0.1:{port}/jsonrpc", body,
                                             {"Content-Type": "application/json"})
            with opener.open(request, timeout=2) as response:
                result = json.load(response)
            if "error" in result:
                raise AssertionError(result["error"].get("code"))
            return result["result"]

        payload = bytes(range(256)) * 16384

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        with (root / "startup.log").open("w") as log:
            process = subprocess.Popen([str(executable)], env=environment,
                                       stdout=log, stderr=subprocess.STDOUT)
            try:
                version = wait_for(lambda: rpc("aria2.getVersion"), process)
                assert version["version"] == "3.0.9"
                url = f"http://127.0.0.1:{server.server_port}/smoke.bin"
                # The host starts saved sessions paused. Match the real
                # DownloadService's explicit opt-in for a new submission.
                gid = rpc("aria2.addUri", [url], {"dir": str(downloads), "out": "smoke.bin", "pause": "false"})
                last_status = {}

                def completed():
                    nonlocal last_status
                    last_status = rpc("aria2.tellStatus", gid)
                    if last_status["status"] == "error":
                        raise AssertionError(json.dumps({key: last_status.get(key) for key in
                                                         ("status", "errorCode", "errorMessage")}))
                    return last_status["status"] == "complete"

                try:
                    wait_for(completed, process)
                except AssertionError:
                    print("Download diagnostics:", json.dumps({key: last_status.get(key) for key in
                          ("status", "completedLength", "totalLength", "errorCode", "errorMessage")}), flush=True)
                    print((root / "startup.log").read_text()[-8000:], flush=True)
                    raise
                assert (downloads / "smoke.bin").read_bytes() == payload
                saved = json.loads((data / "settings.json").read_text())
                assert saved["downloadDirectory"] == str(downloads)
                credential_failed = "Unable to restore" in (root / "startup.log").read_text()
                if unavailable_home and credential_failed:
                    assert saved["privacyPasswordConfigured"] is True
                print(json.dumps({"isolatedHome": unavailable_home,
                                  "credentialStoreUnavailable": credential_failed,
                                  "appAlive": process.poll() is None,
                                  "engineVersion": version["version"],
                                  "downloadSha256": hashlib.sha256(payload).hexdigest()}), flush=True)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                server.shutdown()
                server.server_close()
                deadline = time.monotonic() + 12
                while time.monotonic() < deadline:
                    try:
                        rpc("aria2.getVersion")
                    except OSError:
                        break
                    time.sleep(0.2)
                else:
                    raise AssertionError("Download engine remained alive after the parent exited")


if __name__ == "__main__":
    app = Path(sys.argv[1]).resolve()
    executable = app / "Contents/MacOS/versadown"
    assert executable.is_file(), "Pass the path to VersaDown.app"
    for unavailable in (False, True):
        smoke(executable, unavailable)
