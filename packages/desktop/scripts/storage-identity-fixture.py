"""Prepare isolated, distinct S3 identities for packaged desktop qualification."""
from __future__ import annotations

from contextlib import ExitStack, contextmanager
import hashlib
import http.client
import http.server
import importlib.util
import json
import os
from pathlib import Path
import secrets
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("sdk_fixture", ROOT / "crab/scripts/qualify_sdk_native_rustfs.py")
sdk_fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sdk_fixture)


def stop(server: subprocess.Popen) -> None:
    if server.poll() is not None:
        return
    server.terminate()
    try:
        server.wait(timeout=15)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()


def proxy(port: int) -> http.server.ThreadingHTTPServer:
    counts = {"get": 0, "head": 0, "writes": 0, "blocked": 0}
    gate = threading.Event()
    gate.set()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def reject(self):
            counts["writes"] += 1
            self.send_error(403, "Qualification permits only reads")

        do_PUT = do_POST = do_DELETE = do_PATCH = reject

        def forward(self):
            if self.path in ("/__fixture_stats", "/__fixture_pause", "/__fixture_release"):
                if self.path == "/__fixture_pause":
                    gate.clear()
                elif self.path == "/__fixture_release":
                    gate.set()
                body = json.dumps({**counts, "paused": not gate.is_set()}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            counts["get" if self.command == "GET" else "head"] += 1
            if not gate.is_set():
                counts["blocked"] += 1
                try:
                    deadline = time.monotonic() + 30
                    while not gate.wait(0.05):
                        # Cancellation must finish while storage is still held.
                        # Stop the owned handler when its native client leaves.
                        readable, _, _ = select.select([self.connection], [], [], 0)
                        if readable and not self.connection.recv(1, socket.MSG_PEEK):
                            self.close_connection = True
                            return
                        if time.monotonic() >= deadline:
                            self.send_error(504, "Qualification storage gate expired")
                            return
                finally:
                    counts["blocked"] -= 1
            backend = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            try:
                # Preserve Host: SigV4 authenticates the endpoint selected by
                # the app, even though this read-only proxy forwards locally.
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ("connection", "proxy-connection")}
                backend.request(self.command, self.path, headers=headers)
                response = backend.getresponse()
                body = response.read(16 * 1024 * 1024 + 1)
                if len(body) > 16 * 1024 * 1024:
                    raise RuntimeError("Fixture response exceeded its bound")
                self.send_response(response.status)
                for key, value in response.getheaders():
                    if key.lower() not in ("transfer-encoding", "connection", "server", "date", "content-length"):
                        self.send_header(key, value)
                length = response.getheader("Content-Length") if self.command == "HEAD" else str(len(body))
                if length is not None:
                    self.send_header("Content-Length", length)
                self.send_header("Connection", "close")
                self.end_headers()
                if self.command != "HEAD":
                    self.wfile.write(body)
                self.close_connection = True
            finally:
                backend.close()

        do_GET = do_HEAD = forward

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


@contextmanager
def prepare(target: Path, crab: Path, rustfs: Path):
    target = target.resolve(strict=True)
    parent, prefix, _data = sdk_fixture.rustfs_work_location(target / "storage-identities")
    work = Path(tempfile.mkdtemp(prefix=prefix, dir=parent))
    bucket = "desktop-identities-" + secrets.token_hex(5)
    profiles = []
    payloads = []
    cold_payloads = []
    git, aws = shutil.which("git"), shutil.which("aws")
    if not git or not aws:
        raise RuntimeError("Git and AWS CLI are required for storage identity qualification")
    with ExitStack() as cleanup:
        for index, name in enumerate(("Alpha", "Beta")):
            directory = work / name.lower()
            directory.mkdir(mode=0o700)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            endpoint = f"http://127.0.0.1:{port}"
            env = sdk_fixture.fixture_environment(directory, directory, endpoint, bucket, Path(git), crab, target)
            env["PATH"] = str(crab.parent) + os.pathsep + env["PATH"]
            env["CRAB_BIN"] = str(crab)
            log = cleanup.enter_context((directory / "commands.log").open("w"))

            def run(arguments, cwd=directory, environment=env):
                result = subprocess.run([str(value) for value in arguments], cwd=cwd, env=environment, capture_output=True, timeout=120)
                output = result.stdout + result.stderr
                for key in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"):
                    output = output.replace(environment[key].encode(), b"[redacted]")
                log.write(output.decode(errors="replace"))
                log.flush()
                if result.returncode:
                    raise RuntimeError(f"Fixture command {Path(str(arguments[0])).name} failed; see its private log")
                return result.stdout

            data = directory / "data"
            data.mkdir()
            server_log = cleanup.enter_context((directory / "server.log").open("w"))
            server_env = {**env, "RUSTFS_ACCESS_KEY": env["AWS_ACCESS_KEY_ID"], "RUSTFS_SECRET_KEY": env["AWS_SECRET_ACCESS_KEY"], "RUSTFS_CONSOLE_ENABLE": "false", "RUST_LOG": "warn"}
            server = subprocess.Popen([str(rustfs), "server", "--address", f"127.0.0.1:{port}", str(data)], cwd=directory, env=server_env, stdout=server_log, stderr=server_log)
            cleanup.callback(stop, server)
            deadline = time.monotonic() + 60
            while True:
                ready = subprocess.run([aws, "--endpoint-url", endpoint, "s3api", "list-buckets"], env=env, capture_output=True, timeout=10)
                if ready.returncode == 0:
                    break
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Owned RustFS did not become ready")
                time.sleep(0.2)
            run([aws, "--endpoint-url", endpoint, "s3api", "create-bucket", "--bucket", bucket])
            readonly = proxy(port)
            cleanup.callback(readonly.server_close)
            cleanup.callback(readonly.shutdown)
            profile = {"name": name, "provider": "s3", "bucket": bucket, "region": "us-east-1", "endpoint": f"http://127.0.0.1:{readonly.server_address[1]}", "credentials": {"provider": "s3", "access_key_id": env["AWS_ACCESS_KEY_ID"], "secret_access_key": env["AWS_SECRET_ACCESS_KEY"]}}
            profiles.append(profile)
            source = directory / "source"
            source.mkdir()
            run([git, "init", "--initial-branch=main"], source)
            for key, value in (("user.name", "Desktop test"), ("user.email", "test@example.invalid"), ("commit.gpgsign", "false")):
                run([git, "config", key, value], source)
            url = f"crab://{bucket}/proof"
            run([crab, "init", url], source)
            run([crab, "lfs", "install", "--local", "--skip-repo"], source)
            (source / ".gitattributes").write_text("weights.safetensors filter=crab -text\nlfs.safetensors filter=lfs -text\ncold.safetensors filter=lfs -text\n")
            size = 4 * 1024 * 1024
            marker = name.upper() + "_STORAGE_IDENTITY_CONTENT"
            header = json.dumps({marker: {"dtype": "U8", "shape": [size], "data_offsets": [0, size]}}).encode()
            payload = len(header).to_bytes(8, "little") + header + bytes([0x31 + index]) * size
            for filename in ("weights.safetensors", "lfs.safetensors"):
                (source / filename).write_bytes(payload)
            cold_marker = name.upper() + "_COLD_STORAGE_IDENTITY_CONTENT"
            cold_header = json.dumps({cold_marker: {"dtype": "U8", "shape": [size], "data_offsets": [0, size]}}).encode()
            cold = len(cold_header).to_bytes(8, "little") + cold_header + bytes([0x51 + index]) * size
            (source / "cold.safetensors").write_bytes(cold)
            samples = bytes([0x10 + index]) * 2048
            (source / "sample.wav").write_bytes(struct.pack("<4sI4s4sIHHIIHH4sI", b"RIFF", 36 + len(samples), b"WAVE", b"fmt ", 16, 1, 1, 8000, 16000, 2, 16, b"data", len(samples)) + samples)
            run([git, "add", ".gitattributes", "crab.toml", "weights.safetensors", "lfs.safetensors", "cold.safetensors", "sample.wav"], source)
            run([git, "commit", "-m", "Storage identity fixture"], source)
            run([crab, "push", url, "main", "--json"], source)
            payloads.append({"marker": marker, "sha256": hashlib.sha256(payload).hexdigest(), "size": len(payload)})
            cold_payloads.append({"marker": cold_marker, "sha256": hashlib.sha256(cold).hexdigest(), "size": len(cold)})
            if index == 0:
                clone = work / "local"
                run([crab, "clone", str(source), str(clone), "--lazy"])
                if any((clone / filename).stat().st_size >= 1024 for filename in ("weights.safetensors", "lfs.safetensors", "cold.safetensors")):
                    raise RuntimeError("Lazy fixture unexpectedly materialized its pointers")
        descriptor = work / "fixture.json"
        with open(descriptor, "x", opener=lambda name, flags: os.open(name, flags, 0o600)) as output:
            json.dump({"url": url, "local": str(clone), "profiles": profiles, "payloads": payloads, "cold_payloads": cold_payloads}, output)
        yield descriptor


if __name__ == "__main__":
    target = Path(os.environ["CARGO_TARGET_DIR"])
    binary, _asset, _digest = sdk_fixture.install_rustfs(target / "large-file-fixture-tools")
    with prepare(target, Path(os.environ["CRAB_BIN"]), binary) as descriptor:
        print(json.dumps({"ready": True, "fixture": str(descriptor)}), flush=True)
        for line in sys.stdin:
            if line.strip() == "stop":
                break
