"""Run packaged v2 storage qualification against an owned native RustFS."""
from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time

DESKTOP = Path(__file__).resolve().parents[1]
ROOT = DESKTOP.parents[1]


def main() -> None:
    target = Path(os.environ["CARGO_TARGET_DIR"]).resolve(strict=True)
    spec = importlib.util.spec_from_file_location("sdk_fixture", ROOT / "crab/scripts/qualify_sdk_native_rustfs.py")
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    server_binary, _, _ = fixture.install_rustfs(target / "desktop-fixture-tools")
    git, aws, node = (shutil.which(name) for name in ("git", "aws", "node"))
    if not all((git, aws, node)):
        raise RuntimeError("Git, AWS CLI, and Node 22 are required")
    if os.name == "nt":
        executable = DESKTOP / "release/win-unpacked/Crab.exe"
        crab = executable.parent / "resources/agent/crab.exe"
    else:
        executable = DESKTOP / "release/mac-arm64/Crab.app/Contents/MacOS/Crab"
        crab = executable.parents[1] / "Resources/agent/crab"
    parent, prefix, _ = fixture.rustfs_work_location(target / "desktop-proof")
    with tempfile.TemporaryDirectory(prefix=prefix, dir=parent) as temporary:
        work = Path(temporary)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        env = fixture.fixture_environment(work, work, endpoint, "desktop", Path(git), crab, target)
        env["CRAB_PACKAGED_APP"] = str(executable)
        server_env = {**env, "RUSTFS_ACCESS_KEY": env["AWS_ACCESS_KEY_ID"],
                      "RUSTFS_SECRET_KEY": env["AWS_SECRET_ACCESS_KEY"],
                      "RUSTFS_CONSOLE_ENABLE": "false", "RUST_LOG": "warn"}
        data = work / "data"
        data.mkdir()
        with (work / "server.log").open("w") as log:
            server = subprocess.Popen([str(server_binary), "server", "--address", f"127.0.0.1:{port}", str(data)],
                                      cwd=work, env=server_env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 60
                while True:
                    result = subprocess.run([aws, "--endpoint-url", endpoint, "s3api", "list-buckets"],
                                            env=env, capture_output=True, timeout=10)
                    if result.returncode == 0:
                        break
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("Isolated RustFS did not become ready")
                    time.sleep(0.5)
                subprocess.run([aws, "--endpoint-url", endpoint, "s3api", "create-bucket", "--bucket", "desktop"],
                               env=env, check=True, capture_output=True)
                subprocess.run(["cargo", "test", "--release", "--locked", "-p", "crab-desktop-agent",
                                "--", "--include-ignored"],
                               cwd=ROOT, env=env, check=True)
                subprocess.run([node, "scripts/smoke.mjs"], cwd=DESKTOP, env=env, check=True)
            finally:
                server.terminate()
                try:
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
