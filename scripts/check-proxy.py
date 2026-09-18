#!/usr/bin/env python3
"""Local-only nginx template smoke test. Requires Docker, openssl and Python 3.
No production host or account is contacted. Uses an ephemeral self-signed cert.
"""
import http.server
import json
import pathlib
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
IMAGE = "nginx:1.28-alpine"


class Echo(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.reply()

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.reply()

    def reply(self):
        self.server.requests += 1
        body = json.dumps({"headers": dict(self.headers), "count": self.server.requests}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


with tempfile.TemporaryDirectory(prefix="gameboxd_proxy_") as tmp:
    directory = pathlib.Path(tmp)
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=gameboxd.example", "-keyout", str(directory / "privkey.pem"),
                    "-out", str(directory / "fullchain.pem")], check=True, capture_output=True)
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Echo)
    upstream.requests = 0
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    config = (ROOT / "ops/nginx.conf.example").read_text()
    config = config.replace("listen 443 ssl;", f"listen 127.0.0.1:{port} ssl;")
    config = config.replace("http://127.0.0.1:3000", f"http://127.0.0.1:{upstream.server_port}")
    (directory / "default.conf").write_text(config)
    mounts = ["--mount", f"type=bind,src={directory / 'default.conf'},dst=/etc/nginx/conf.d/default.conf,readonly",
              "--mount", f"type=bind,src={directory},dst=/etc/gameboxd/tls,readonly"]
    subprocess.run(["docker", "run", "--rm", "--network", "none", *mounts,
                    "--entrypoint", "nginx", IMAGE, "-t"], check=True)
    name = "gameboxd-proxy-check-" + uuid.uuid4().hex
    try:
        subprocess.run(["docker", "run", "--rm", "--detach", "--name", name, "--network", "host",
                        *mounts, "--entrypoint", "nginx", IMAGE, "-g", "daemon off;"], check=True, capture_output=True)
        # Only the throwaway local certificate is trusted for this test.
        context = ssl.create_default_context(cafile=str(directory / "fullchain.pem"))
        context.check_hostname = False  # local URL is 127.0.0.1; cert is gameboxd.example
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context))
        def request(path, data=None):
            req = urllib.request.Request(f"https://127.0.0.1:{port}{path}", data=data,
                headers={"Host": "gameboxd.example", "X-Forwarded-For": "203.0.113.99, 198.51.100.99",
                         "Forwarded": "for=203.0.113.99", "X-Real-IP": "203.0.113.99"})
            return opener.open(req, timeout=3)
        for attempt in range(50):
            try:
                with request("/health/live") as response:
                    response.read()
                break
            except urllib.error.URLError:
                if attempt == 49:
                    raise
                time.sleep(0.1)
        prior = 0
        for _ in range(2):
            with request("/api/public/profiles/test?secret=must-not-be-logged") as response:
                assert response.headers["Cache-Control"] == "no-store"
                assert "max-age" in response.headers["Strict-Transport-Security"]
                body = json.load(response)
                assert body["headers"]["X-Forwarded-For"] == "127.0.0.1"
                assert "Forwarded" not in body["headers"] and "X-Real-IP" not in body["headers"]
                assert body["count"] > prior
                prior = body["count"]
        for path in ["/api/me/import", "/api/me/import/preview"]:
            with request(path, b"x" * 70_000) as response:
                assert response.status == 200
                response.read()
        try:
            request("/api/me/lists", b"x" * 70_000)
            raise AssertionError("ordinary request exceeded body limit")
        except urllib.error.HTTPError as response:
            assert response.code == 413
            assert response.headers["Cache-Control"] == "no-store"
        log = subprocess.run(["docker", "exec", name, "cat", "/var/log/nginx/gameboxd.log"], check=True, capture_output=True).stdout
        assert b"must-not-be-logged" not in log
        print("Proxy template passed: syntax, TLS, header overwrite, no caching, token-safe access logs and body limits.")
    finally:
        subprocess.run(["docker", "rm", "--force", name], capture_output=True)
        upstream.shutdown()
        upstream.server_close()
