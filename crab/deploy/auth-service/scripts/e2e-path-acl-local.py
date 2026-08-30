#!/usr/bin/env python3
"""Local path-ACL smoke test using Moto S3, Crab Auth, and the crab CLI."""

from __future__ import annotations

import base64
import json
import logging
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import boto3
import jwt
import requests
import uvicorn
import yaml
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from jwt.algorithms import RSAAlgorithm
from moto.server import ThreadedMotoServer


ROOT = Path(__file__).resolve().parents[3]
AUTH_ROOT = ROOT / "deploy" / "auth-service"
WORKSPACE = ROOT.parent
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", WORKSPACE / "target")) / "debug"
CRAB = TARGET / "crab"
VIEW_HELPER = TARGET / "crab-auth-view"
RECEIVE_HELPER = TARGET / "crab-auth-receive"
ISSUER = "https://idp.local.example"
AUDIENCE = "crab-cli"


def main() -> None:
    require_binaries()
    logging.getLogger("werkzeug").setLevel(logging.ERROR)
    sys.path.insert(0, str(AUTH_ROOT))

    tmp_path = Path(tempfile.mkdtemp(prefix="crab-acl-e2e-"))
    success = False
    try:
        os.chdir(tmp_path)
        moto_port = free_port()
        auth_port = free_port()
        jwks_port = free_port()
        moto_url = f"http://127.0.0.1:{moto_port}"
        auth_url = f"http://127.0.0.1:{auth_port}"
        jwks_url = f"http://127.0.0.1:{jwks_port}/jwks.json"
        bucket = f"crab-acl-e2e-{secrets.token_hex(4)}"
        repo_prefix = f"acl-e2e/{secrets.token_hex(4)}"
        repo_url = f"crab://{bucket}/{repo_prefix}"

        moto = start_moto(moto_port)
        auth_server = None
        jwks_server = None
        try:
            wait_http(moto_url, expect_any=True)
            create_bucket(moto_url, bucket)

            policy_path = write_policy(tmp_path, repo_prefix)
            private_key, jwks = make_jwks()
            jwks_server = start_jwks_server(jwks_port, jwks)
            configure_auth_env(
                policy_path=policy_path,
                moto_url=moto_url,
                auth_url=auth_url,
                jwks_url=jwks_url,
            )
            auth_server = start_auth_server(auth_port)
            wait_http(f"{auth_url}/health")
            assert_ready(auth_url)

            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            (bin_dir / "git-remote-crab").symlink_to(CRAB)

            source_home = make_home(tmp_path, "source")
            alice_home = make_home(
                tmp_path,
                "alice",
                auth_url=auth_url,
                token=make_token(private_key, "alice@corp.example.com"),
            )
            bob_home = make_home(
                tmp_path,
                "bob",
                auth_url=auth_url,
                token=make_token(private_key, "bob@corp.example.com"),
            )

            source_repo = tmp_path / "source"
            create_source_repo(
                source_repo,
                repo_url,
                cli_env(source_home, moto_url, bin_dir),
            )

            alice_scope = issue_credentials(auth_url, repo_url, "clone", alice_home)
            if "storage_scope" not in alice_scope:
                raise AssertionError("alice path-scoped read did not return storage_scope")

            alice_clone = tmp_path / "alice-clone"
            run_crab(
                ["clone", repo_url, str(alice_clone), "--branch", "main", "--eager"],
                cli_env(alice_home, moto_url, bin_dir),
                tmp_path,
            )
            run_crab(["hydrate", "--all"], cli_env(alice_home, moto_url, bin_dir), alice_clone)
            assert_file(alice_clone / "src" / "allowed.bin", "allowed v1")
            assert_absent(alice_clone / "secret")
            assert_not_in_git_log(alice_clone, "secret/hidden.bin")

            bob_scope = issue_credentials(auth_url, repo_url, "clone", bob_home)
            if "storage_scope" in bob_scope:
                raise AssertionError("bob repo-wide read unexpectedly returned storage_scope")

            bob_clone = tmp_path / "bob-clone"
            run_crab(
                ["clone", repo_url, str(bob_clone), "--branch", "main", "--eager"],
                cli_env(bob_home, moto_url, bin_dir),
                tmp_path,
            )
            run_crab(["hydrate", "--all"], cli_env(bob_home, moto_url, bin_dir), bob_clone)
            assert_file(bob_clone / "src" / "allowed.bin", "allowed v1")
            assert_file(bob_clone / "secret" / "hidden.bin", "secret v1")

            write_text(alice_clone / "src" / "allowed.bin", "allowed v2")
            run_crab(["add", "src/allowed.bin"], cli_env(alice_home, moto_url, bin_dir), alice_clone)
            git_commit(alice_clone, "alice allowed update", cli_env(alice_home, moto_url, bin_dir))
            run_crab(["push"], cli_env(alice_home, moto_url, bin_dir), alice_clone)

            bob_after_allowed = tmp_path / "bob-after-allowed"
            run_crab(
                ["clone", repo_url, str(bob_after_allowed), "--branch", "main", "--eager"],
                cli_env(bob_home, moto_url, bin_dir),
                tmp_path,
            )
            run_crab(
                ["hydrate", "--all"],
                cli_env(bob_home, moto_url, bin_dir),
                bob_after_allowed,
            )
            assert_file(bob_after_allowed / "src" / "allowed.bin", "allowed v2")
            assert_file(bob_after_allowed / "secret" / "hidden.bin", "secret v1")

            (alice_clone / "secret").mkdir(exist_ok=True)
            write_text(alice_clone / "secret" / "hidden.bin", "secret v2")
            run_crab(
                ["add", "secret/hidden.bin"],
                cli_env(alice_home, moto_url, bin_dir),
                alice_clone,
            )
            git_commit(alice_clone, "alice denied update", cli_env(alice_home, moto_url, bin_dir))
            denied = run_crab(
                ["push"],
                cli_env(alice_home, moto_url, bin_dir),
                alice_clone,
                check=False,
            )
            if denied.returncode == 0:
                raise AssertionError("alice denied secret push unexpectedly succeeded")
            denied_output = f"{denied.stdout}\n{denied.stderr}".lower()
            if not any(
                marker in denied_output
                for marker in ("forbidden", "explicitly denied", "403")
            ):
                raise AssertionError(
                    "alice denied secret push did not fail with policy denial:\n"
                    f"{denied.stdout}\n{denied.stderr}"
                )
            if "non-fast-forward" in denied_output or "invalid_bundle" in denied_output:
                raise AssertionError(
                    "alice denied secret push failed before write ACL evaluation:\n"
                    f"{denied.stdout}\n{denied.stderr}"
                )

            bob_after_denied = tmp_path / "bob-after-denied"
            run_crab(
                ["clone", repo_url, str(bob_after_denied), "--branch", "main", "--eager"],
                cli_env(bob_home, moto_url, bin_dir),
                tmp_path,
            )
            run_crab(
                ["hydrate", "--all"],
                cli_env(bob_home, moto_url, bin_dir),
                bob_after_denied,
            )
            assert_file(bob_after_denied / "src" / "allowed.bin", "allowed v2")
            assert_file(bob_after_denied / "secret" / "hidden.bin", "secret v1")

            print(
                "path ACL E2E passed: alice filtered clone/hydrate, "
                "bob repo-wide clone, alice allowed push, alice denied push"
            )
            success = True
        finally:
            if auth_server is not None:
                auth_server.should_exit = True
            if jwks_server is not None:
                jwks_server.shutdown()
            moto.stop()
    finally:
        if success:
            shutil.rmtree(tmp_path)
        else:
            print(f"path ACL E2E failed; kept temp dir: {tmp_path}", file=sys.stderr)


def require_binaries() -> None:
    missing = [
        str(path)
        for path in [CRAB, VIEW_HELPER, RECEIVE_HELPER]
        if not path.exists()
    ]
    if missing:
        raise SystemExit(f"missing built binary: {', '.join(missing)}")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def start_moto(port: int) -> ThreadedMotoServer:
    server = ThreadedMotoServer(ip_address="127.0.0.1", port=port, verbose=False)
    server.start()
    return server


def wait_http(url: str, *, expect_any: bool = False) -> None:
    deadline = time.time() + 30
    last_error: Exception | None = None
    while time.time() < deadline:
        try:
            response = requests.get(url, timeout=1)
            if expect_any or response.status_code < 500:
                return
        except Exception as e:  # server not listening yet
            last_error = e
        time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {url}: {last_error}")


def create_bucket(endpoint: str, bucket: str) -> None:
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id="test",
        aws_secret_access_key="test",
        region_name="us-east-1",
    )
    client.create_bucket(Bucket=bucket)


def write_policy(tmp_path: Path, repo_prefix: str) -> Path:
    policy = {
        "version": "1",
        "default_provider": "aws",
        "protected_repos": [repo_prefix],
        "rules": [
            {
                "identity": "alice@corp.example.com",
                "repos": [repo_prefix],
                "operations": ["clone", "fetch", "hydrate", "pull", "push"],
                "read_paths": ["src/**"],
                "write_paths": ["src/**"],
            },
            {
                "identity": "bob@corp.example.com",
                "repos": [repo_prefix],
                "operations": ["clone", "fetch", "hydrate", "pull", "push"],
            },
        ],
        "deny": [
            {
                "identity": "alice@corp.example.com",
                "repos": [repo_prefix],
                "operations": ["clone", "fetch", "hydrate", "pull"],
                "read_paths": ["secret/**"],
            },
            {
                "identity": "alice@corp.example.com",
                "repos": [repo_prefix],
                "operations": ["push"],
                "write_paths": ["secret/**"],
            },
        ],
    }
    path = tmp_path / "policy.yaml"
    path.write_text(yaml.safe_dump(policy), encoding="utf-8")
    return path


def make_jwks() -> tuple[Any, dict[str, Any]]:
    private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    public_jwk = json.loads(RSAAlgorithm.to_jwk(private_key.public_key()))
    public_jwk.update({"kid": "acl-e2e-key", "use": "sig", "alg": "RS256"})
    return private_key, {"keys": [public_jwk]}


def configure_auth_env(
    *,
    policy_path: Path,
    moto_url: str,
    auth_url: str,
    jwks_url: str,
) -> None:
    os.environ.update({
        "CRAB_AUTH_POLICY_PATH": str(policy_path),
        "CRAB_AUTH_JWKS_URL": jwks_url,
        "CRAB_AUTH_ISSUER": ISSUER,
        "CRAB_AUTH_AUDIENCE": AUDIENCE,
        "CRAB_AUTH_DRY_RUN": "true",
        "CRAB_AUTH_AWS_REGION": "us-east-1",
        "CRAB_AUTH_VIEW_HELPER": str(VIEW_HELPER),
        "CRAB_AUTH_RECEIVE_HELPER": str(RECEIVE_HELPER),
        "AWS_ACCESS_KEY_ID": "test",
        "AWS_SECRET_ACCESS_KEY": "test",
        "AWS_SESSION_TOKEN": "test",
        "AWS_REGION": "us-east-1",
        "AWS_DEFAULT_REGION": "us-east-1",
        "AWS_ENDPOINT_URL": moto_url,
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
    })


def start_jwks_server(port: int, jwks: dict[str, Any]) -> ThreadingHTTPServer:
    body = json.dumps(jwks).encode("utf-8")

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            if self.path != "/jwks.json":
                self.send_response(404)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: Any) -> None:
            return

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def start_auth_server(port: int) -> uvicorn.Server:
    from src import app as app_module
    from src.providers import _providers

    app_module._verifier = None
    app_module._policy = None
    app_module._receive_helper = None
    app_module._view_helper = None
    _providers.clear()

    server = uvicorn.Server(
        uvicorn.Config(
            app_module.app,
            host="127.0.0.1",
            port=port,
            log_level="warning",
        )
    )
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    return server


def assert_ready(auth_url: str) -> None:
    response = requests.get(f"{auth_url}/ready", timeout=10)
    if response.status_code != 200:
        raise AssertionError(f"/ready failed: {response.status_code} {response.text}")
    body = response.json()
    if body.get("receive_helper") != "ok" or body.get("view_helper") != "ok":
        raise AssertionError(f"/ready missing helper status: {body}")


def make_home(
    tmp_path: Path,
    name: str,
    *,
    auth_url: str | None = None,
    token: str | None = None,
) -> Path:
    home = tmp_path / f"home-{name}"
    config_dir = home / ".config" / "crab"
    config_dir.mkdir(parents=True)
    key = secrets.token_bytes(32)
    key_path = config_dir / ".token-key"
    key_path.write_bytes(key)
    key_path.chmod(0o600)

    if auth_url and token:
        (config_dir / "config.toml").write_text(
            "\n".join([
                "[auth]",
                'provider = "crab-auth"',
                f'issuer_url = "{ISSUER}"',
                f'client_id = "{AUDIENCE}"',
                f'auth_endpoint = "{auth_url}/v1/credentials"',
                'token_cache_path = "~/.config/crab/tokens/"',
                "",
            ]),
            encoding="utf-8",
        )
        write_token_cache(config_dir / "tokens", key, token)

    return home


def make_token(private_key: Any, email: str) -> str:
    now = int(time.time())
    payload = {
        "sub": email,
        "email": email,
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now,
        "exp": now + 3600,
    }
    return jwt.encode(
        payload,
        private_key,
        algorithm="RS256",
        headers={"kid": "acl-e2e-key"},
    )


def write_token_cache(token_dir: Path, key: bytes, token: str) -> None:
    token_dir.mkdir(parents=True)
    payload_part = token.split(".", 2)[1]
    padded = payload_part + "=" * (-len(payload_part) % 4)
    claims = json.loads(base64.urlsafe_b64decode(padded.encode("ascii")))
    plaintext = json.dumps(
        {
            "id_token": token,
            "refresh_token": None,
            "identity": {
                "subject": claims["sub"],
                "email": claims.get("email"),
                "name": claims.get("name"),
            },
            "issued_at": int(time.time()),
        },
        separators=(",", ":"),
    ).encode("utf-8")
    nonce = secrets.token_bytes(12)
    ciphertext = ChaCha20Poly1305(key).encrypt(nonce, plaintext, None)
    (token_dir / "crab-auth.json.enc").write_bytes(nonce + ciphertext)


def cli_env(home: Path, moto_url: str, bin_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update({
        "HOME": str(home),
        "CRAB_NO_KEYCHAIN": "1",
        "AWS_ACCESS_KEY_ID": "test",
        "AWS_SECRET_ACCESS_KEY": "test",
        "AWS_SESSION_TOKEN": "test",
        "AWS_REGION": "us-east-1",
        "AWS_DEFAULT_REGION": "us-east-1",
        "AWS_ENDPOINT_URL": moto_url,
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "PATH": f"{bin_dir}:{TARGET}:{env.get('PATH', '')}",
        "GIT_CONFIG_NOSYSTEM": "1",
    })
    return env


def create_source_repo(path: Path, repo_url: str, env: dict[str, str]) -> None:
    path.mkdir()
    run(["git", "init", "-b", "main"], env, path)
    run(["git", "config", "user.email", "source@corp.example.com"], env, path)
    run(["git", "config", "user.name", "Source User"], env, path)
    run_crab(["init", repo_url], env, path)
    run_crab(["track", "*.bin"], env, path)
    (path / "src").mkdir()
    (path / "secret").mkdir()
    write_text(path / "src" / "allowed.bin", "allowed v1")
    write_text(path / "secret" / "hidden.bin", "secret v1")
    run_crab(["add", "src/allowed.bin", "secret/hidden.bin"], env, path)
    for candidate in [".gitattributes", "crab.toml"]:
        if (path / candidate).exists():
            run(["git", "add", candidate], env, path)
    git_commit(path, "initial", env)
    run_crab(["push"], env, path)


def run_crab(
    args: list[str],
    env: dict[str, str],
    cwd: Path,
    *,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    return run([str(CRAB), *args], env, cwd, check=check)


def run(
    args: list[str],
    env: dict[str, str],
    cwd: Path,
    *,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        args,
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=300,
    )
    if check and completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(args)}\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}"
        )
    return completed


def git_commit(repo: Path, message: str, env: dict[str, str]) -> None:
    run(["git", "add", "-u"], env, repo)
    run(
        [
            "git",
            "-c",
            "user.email=e2e@corp.example.com",
            "-c",
            "user.name=ACL E2E",
            "commit",
            "-m",
            message,
        ],
        env,
        repo,
    )


def write_text(path: Path, value: str) -> None:
    path.write_text(value, encoding="utf-8")


def assert_file(path: Path, expected: str) -> None:
    if not path.exists():
        root = path.parents[1] if len(path.parents) > 1 else path.parent
        entries = sorted(str(p.relative_to(root)) for p in root.rglob("*")) if root.exists() else []
        raise AssertionError(f"{path} is missing; entries under {root}: {entries[:80]}")
    actual = path.read_text(encoding="utf-8")
    if actual != expected:
        raise AssertionError(f"{path} content mismatch: {actual!r}")


def assert_absent(path: Path) -> None:
    if path.exists():
        raise AssertionError(f"{path} should be absent")


def assert_not_in_git_log(repo: Path, forbidden: str) -> None:
    result = run(["git", "log", "--name-only", "--format="], os.environ.copy(), repo)
    if forbidden in result.stdout:
        raise AssertionError(f"{forbidden} leaked through git log")


def issue_credentials(
    auth_url: str,
    repo_url: str,
    operation: str,
    home: Path,
) -> dict[str, Any]:
    token_file = home / ".config" / "crab" / "tokens" / "crab-auth.json.enc"
    if not token_file.exists():
        raise AssertionError(f"missing token cache for {home}")
    return direct_credential_request(auth_url, repo_url, operation, home)


def direct_credential_request(
    auth_url: str,
    repo_url: str,
    operation: str,
    home: Path,
) -> dict[str, Any]:
    key = (home / ".config" / "crab" / ".token-key").read_bytes()
    token_file = home / ".config" / "crab" / "tokens" / "crab-auth.json.enc"
    data = token_file.read_bytes()
    plaintext = ChaCha20Poly1305(key).decrypt(data[:12], data[12:], None)
    token = json.loads(plaintext)["id_token"]
    response = requests.post(
        f"{auth_url}/v1/credentials",
        json={
            "id_token": token,
            "repo_url": repo_url,
            "operation": operation,
            "client_version": "acl-e2e-local",
        },
        timeout=600,
    )
    if response.status_code != 200:
        raise AssertionError(f"credential request failed: {response.status_code} {response.text}")
    return response.json()


if __name__ == "__main__":
    main()
