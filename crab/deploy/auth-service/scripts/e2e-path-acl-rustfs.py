#!/usr/bin/env python3
"""RustFS-backed path-ACL smoke test using Crab Auth and the crab CLI."""

from __future__ import annotations

import importlib.util
import json
import os
import secrets
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import boto3
import jwt
import requests
import yaml


ROOT = Path(__file__).resolve().parents[3]
AUTH_ROOT = ROOT / "deploy" / "auth-service"
WORKSPACE = ROOT.parent
LOCAL_SCRIPT = Path(__file__).with_name("e2e-path-acl-local.py")
ISSUER = "https://login.corp.example.com"
AUDIENCE = "crab-cli"


def main() -> None:
    local = load_local_smoke()
    local.ISSUER = ISSUER
    local.AUDIENCE = AUDIENCE
    local.require_binaries()
    sys.path.insert(0, str(AUTH_ROOT))

    endpoint = os.environ.get("CRAB_AUTH_RUSTFS_ENDPOINT", "http://127.0.0.1:9000")
    bucket = os.environ.get("CRAB_AUTH_RUSTFS_BUCKET", "crab")
    region = _env("CRAB_AUTH_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION", default="us-east-1")
    access_key_id = _required_env("CRAB_AUTH_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID")
    secret_access_key = _required_env("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY")
    work_root = Path(
        os.environ.get(
            "CRAB_AUTH_RUSTFS_WORKSPACE",
            str(WORKSPACE / "target" / "crab-auth-rustfs-e2e"),
        )
    )
    run_id = f"{time.strftime('%Y%m%d-%H%M%S')}-{secrets.token_hex(3)}"
    run_dir = work_root / "auth-acl-rustfs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    os.chdir(run_dir)

    auth_port = local.free_port()
    jwks_port = local.free_port()
    auth_url = f"http://127.0.0.1:{auth_port}"
    jwks_url = f"http://127.0.0.1:{jwks_port}/jwks.json"
    repo_prefix = f"auth-acl-rustfs/{run_id}"
    repo_url = f"crab://{bucket}/{repo_prefix}"

    ensure_bucket(endpoint, bucket, region, access_key_id, secret_access_key)
    policy_path = write_policy(run_dir, repo_prefix)
    private_key, jwks = local.make_jwks()
    jwks_server = local.start_jwks_server(jwks_port, jwks)
    auth_server = None
    success = False

    try:
        configure_auth_env(
            local=local,
            policy_path=policy_path,
            endpoint=endpoint,
            jwks_url=jwks_url,
            region=region,
            access_key_id=access_key_id,
            secret_access_key=secret_access_key,
        )
        auth_server = local.start_auth_server(auth_port)
        local.wait_http(f"{auth_url}/health")
        local.assert_ready(auth_url)

        bin_dir = run_dir / "bin"
        bin_dir.mkdir()
        (bin_dir / "git-remote-crab").symlink_to(local.CRAB)

        source_home = local.make_home(run_dir, "source")
        alice_home = local.make_home(
            run_dir,
            "alice",
            auth_url=auth_url,
            token=local.make_token(private_key, "alice@corp.example.com"),
        )
        bob_home = local.make_home(
            run_dir,
            "bob",
            auth_url=auth_url,
            token=local.make_token(private_key, "bob@corp.example.com"),
        )

        assert_invalid_tokens_denied(local, auth_url, repo_url, private_key)
        assert_malformed_requests_denied(auth_url, repo_url, private_key)
        assert_unmatched_identity_denied(auth_url, repo_url, private_key)
        assert_issued_credential_expires(auth_url, repo_url, private_key)

        source_repo = run_dir / "source"
        local.create_source_repo(
            source_repo,
            repo_url,
            cli_env(
                local,
                source_home,
                endpoint,
                bin_dir,
                region,
                access_key_id,
                secret_access_key,
            ),
        )

        alice_scope = local.issue_credentials(auth_url, repo_url, "clone", alice_home)
        if alice_scope.get("provider") != "s3":
            raise AssertionError(f"alice credentials used unexpected provider: {alice_scope}")
        if "storage_scope" not in alice_scope:
            raise AssertionError("alice path-scoped read did not return storage_scope")
        if "session_token" in alice_scope["credentials"]:
            raise AssertionError("RustFS S3 credentials unexpectedly included a session token")
        assert_credential_request_can_be_retried(
            local,
            auth_url,
            repo_url,
            alice_home,
            alice_scope,
        )

        alice_clone = run_dir / "alice-clone"
        local.run_crab(
            ["clone", repo_url, str(alice_clone), "--branch", "main", "--eager"],
            cli_env(
                local,
                alice_home,
                endpoint,
                bin_dir,
                region,
                access_key_id,
                secret_access_key,
            ),
            run_dir,
        )
        local.run_crab(
            ["hydrate", "--all"],
            cli_env(
                local,
                alice_home,
                endpoint,
                bin_dir,
                region,
                access_key_id,
                secret_access_key,
            ),
            alice_clone,
        )
        local.assert_file(alice_clone / "src" / "allowed.bin", "allowed v1")
        local.assert_absent(alice_clone / "secret")
        local.assert_not_in_git_log(alice_clone, "secret/hidden.bin")

        bob_scope = local.issue_credentials(auth_url, repo_url, "clone", bob_home)
        if bob_scope.get("provider") != "s3":
            raise AssertionError(f"bob credentials used unexpected provider: {bob_scope}")
        if "storage_scope" in bob_scope:
            raise AssertionError("bob repo-wide read unexpectedly returned storage_scope")

        bob_env = cli_env(
            local,
            bob_home,
            endpoint,
            bin_dir,
            region,
            access_key_id,
            secret_access_key,
        )
        bob_clone = run_dir / "bob-clone"
        local.run_crab(
            ["clone", repo_url, str(bob_clone), "--branch", "main", "--eager"],
            bob_env,
            run_dir,
        )
        local.run_crab(
            ["hydrate", "--all"],
            bob_env,
            bob_clone,
        )
        local.assert_file(bob_clone / "src" / "allowed.bin", "allowed v1")
        local.assert_file(bob_clone / "secret" / "hidden.bin", "secret v1")

        local.write_text(alice_clone / "src" / "allowed.bin", "allowed v2")
        alice_env = cli_env(
            local,
            alice_home,
            endpoint,
            bin_dir,
            region,
            access_key_id,
            secret_access_key,
        )
        local.run_crab(["add", "src/allowed.bin"], alice_env, alice_clone)
        local.git_commit(alice_clone, "alice allowed update", alice_env)
        local.run_crab(["push"], alice_env, alice_clone)

        local.run(["git", "fetch", "origin", "main"], bob_env, bob_clone)
        local.run(["git", "reset", "--hard", "FETCH_HEAD"], bob_env, bob_clone)
        local.run_crab(["hydrate", "--all"], bob_env, bob_clone)
        local.assert_file(bob_clone / "src" / "allowed.bin", "allowed v2")
        local.assert_file(bob_clone / "secret" / "hidden.bin", "secret v1")

        bob_after_allowed = run_dir / "bob-after-allowed"
        local.run_crab(
            ["clone", repo_url, str(bob_after_allowed), "--branch", "main", "--eager"],
            bob_env,
            run_dir,
        )
        local.run_crab(["hydrate", "--all"], bob_env, bob_after_allowed)
        local.assert_file(bob_after_allowed / "src" / "allowed.bin", "allowed v2")
        local.assert_file(bob_after_allowed / "secret" / "hidden.bin", "secret v1")

        (alice_clone / "secret").mkdir(exist_ok=True)
        local.write_text(alice_clone / "secret" / "hidden.bin", "secret v2")
        local.run_crab(["add", "secret/hidden.bin"], alice_env, alice_clone)
        local.git_commit(alice_clone, "alice denied update", alice_env)
        denied = local.run_crab(["push"], alice_env, alice_clone, check=False)
        if denied.returncode == 0:
            raise AssertionError("alice denied secret push unexpectedly succeeded")
        denied_output = f"{denied.stdout}\n{denied.stderr}".lower()
        if not any(marker in denied_output for marker in ("forbidden", "explicitly denied", "403")):
            raise AssertionError(
                "alice denied secret push did not fail with policy denial:\n"
                f"{denied.stdout}\n{denied.stderr}"
            )
        if "non-fast-forward" in denied_output or "invalid_bundle" in denied_output:
            raise AssertionError(
                "alice denied secret push failed before write ACL evaluation:\n"
                f"{denied.stdout}\n{denied.stderr}"
            )

        bob_after_denied = run_dir / "bob-after-denied"
        local.run_crab(
            ["clone", repo_url, str(bob_after_denied), "--branch", "main", "--eager"],
            bob_env,
            run_dir,
        )
        local.run_crab(["hydrate", "--all"], bob_env, bob_after_denied)
        local.assert_file(bob_after_denied / "src" / "allowed.bin", "allowed v2")
        local.assert_file(bob_after_denied / "secret" / "hidden.bin", "secret v1")

        write_contract_evidence(run_dir, repo_url)

        print(
            "RustFS path ACL E2E passed: clone/hydrate/fetch, allowed push, "
            "path denial, malformed input, retry, and expired credential"
        )
        print(f"repo_url={repo_url}")
        print(f"run_dir={run_dir}")
        print(f"alice_config={alice_home / '.config' / 'crab' / 'config.toml'}")
        print(f"bob_config={bob_home / '.config' / 'crab' / 'config.toml'}")
        success = True
    finally:
        if auth_server is not None:
            auth_server.should_exit = True
        jwks_server.shutdown()
        if not success:
            print(f"RustFS path ACL E2E failed; kept run dir: {run_dir}", file=sys.stderr)


def load_local_smoke() -> Any:
    spec = importlib.util.spec_from_file_location("e2e_path_acl_local", LOCAL_SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {LOCAL_SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["e2e_path_acl_local"] = module
    spec.loader.exec_module(module)
    return module


def ensure_bucket(
    endpoint: str,
    bucket: str,
    region: str,
    access_key_id: str,
    secret_access_key: str,
) -> None:
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=access_key_id,
        aws_secret_access_key=secret_access_key,
        region_name=region,
    )
    try:
        client.head_bucket(Bucket=bucket)
        return
    except Exception:
        client.create_bucket(Bucket=bucket)


def write_policy(run_dir: Path, repo_prefix: str) -> Path:
    policy = {
        "version": "1",
        "default_provider": "s3",
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
    path = run_dir / "policy.yaml"
    path.write_text(yaml.safe_dump(policy), encoding="utf-8")
    return path


def assert_invalid_tokens_denied(
    local: Any,
    auth_url: str,
    repo_url: str,
    private_key: Any,
) -> None:
    for label, token in [
        ("malformed", "not-a-jwt"),
        (
            "expired",
            make_token(private_key, "alice@corp.example.com", expires_in_seconds=-60),
        ),
        (
            "wrong-audience",
            make_token(private_key, "alice@corp.example.com", audience="other-client"),
        ),
    ]:
        response = requests.post(
            f"{auth_url}/v1/credentials",
            json={
                "id_token": token,
                "repo_url": repo_url,
                "operation": "clone",
                "client_version": f"acl-e2e-local-{label}",
            },
            timeout=30,
        )
        if response.status_code != 401:
            raise AssertionError(
                f"{label} credential request returned {response.status_code}: "
                f"{response.text}"
            )

    response = requests.post(
        f"{auth_url}/v1/push/prepare",
        json={
            "id_token": "not-a-jwt",
            "repo_url": repo_url,
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1111111111111111111111111111111111111111",
                }
            ],
            "client_version": "acl-e2e-local-malformed-push",
        },
        timeout=30,
    )
    if response.status_code != 401:
        raise AssertionError(
            "malformed push prepare returned "
            f"{response.status_code}: {response.text}"
        )


def assert_malformed_requests_denied(
    auth_url: str,
    repo_url: str,
    private_key: Any,
) -> None:
    token = make_token(private_key, "alice@corp.example.com")
    invalid_repo = requests.post(
        f"{auth_url}/v1/credentials",
        json={
            "id_token": token,
            "repo_url": "crab://missing-prefix",
            "operation": "clone",
            "client_version": "acl-e2e-local-invalid-repo",
        },
        timeout=30,
    )
    if invalid_repo.status_code != 400:
        raise AssertionError(
            "malformed repository request returned "
            f"{invalid_repo.status_code}: {invalid_repo.text}"
        )

    extra_field = requests.post(
        f"{auth_url}/v1/credentials",
        json={
            "id_token": token,
            "repo_url": repo_url,
            "operation": "clone",
            "client_version": "acl-e2e-local-extra-field",
            "unexpected": True,
        },
        timeout=30,
    )
    if extra_field.status_code != 400:
        raise AssertionError(
            "unknown credential request field returned "
            f"{extra_field.status_code}: {extra_field.text}"
        )
    if extra_field.json().get("detail", {}).get("error") != "invalid_request":
        raise AssertionError(
            "unknown credential request field returned unexpected contract: "
            f"{extra_field.text}"
        )


def assert_credential_request_can_be_retried(
    local: Any,
    auth_url: str,
    repo_url: str,
    home: Path,
    first_response: dict[str, Any],
) -> None:
    second_response = local.issue_credentials(
        auth_url,
        repo_url,
        "clone",
        home,
    )
    for field in ("provider", "permissions", "storage_scope"):
        if second_response.get(field) != first_response.get(field):
            raise AssertionError(
                f"retried credential request changed authorized {field}"
            )
    if set(second_response.get("credentials", {})) != set(
        first_response.get("credentials", {})
    ):
        raise AssertionError("retried credential request changed credential shape")

    expires_at = second_response.get("expires_at")
    if not isinstance(expires_at, str):
        raise AssertionError("retried credential request omitted expires_at")
    expiry = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
    if expiry <= datetime.now(timezone.utc):
        raise AssertionError("retried credential request returned expired credentials")


def assert_issued_credential_expires(
    auth_url: str,
    repo_url: str,
    private_key: Any,
) -> None:
    from src.providers import _providers

    previous_duration = os.environ.get("CRAB_AUTH_SESSION_DURATION")
    os.environ["CRAB_AUTH_SESSION_DURATION"] = "1"
    _providers.clear()
    try:
        token = make_token(private_key, "bob@corp.example.com")
        response = requests.post(
            f"{auth_url}/v1/credentials",
            json={
                "id_token": token,
                "repo_url": repo_url,
                "operation": "clone",
                "client_version": "acl-e2e-local-expiring-credential",
            },
            timeout=30,
        )
        if response.status_code != 200:
            raise AssertionError(
                "short-lived credential request returned "
                f"{response.status_code}: {response.text}"
            )
        expires_at = response.json().get("expires_at")
        if not isinstance(expires_at, str):
            raise AssertionError("short-lived credential response omitted expires_at")
        time.sleep(1.1)
        expiry = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
        if expiry > datetime.now(timezone.utc):
            raise AssertionError("short-lived credential did not reach its advertised expiry")
    finally:
        if previous_duration is None:
            os.environ.pop("CRAB_AUTH_SESSION_DURATION", None)
        else:
            os.environ["CRAB_AUTH_SESSION_DURATION"] = previous_duration
        _providers.clear()


def assert_unmatched_identity_denied(
    auth_url: str,
    repo_url: str,
    private_key: Any,
) -> None:
    response = requests.post(
        f"{auth_url}/v1/credentials",
        json={
            "id_token": make_token(private_key, "stranger@corp.example.com"),
            "repo_url": repo_url,
            "operation": "clone",
            "client_version": "acl-e2e-local-stranger",
        },
        timeout=30,
    )
    if response.status_code != 403:
        raise AssertionError(
            "unmatched identity credential request returned "
            f"{response.status_code}: {response.text}"
        )


def make_token(
    private_key: Any,
    email: str,
    *,
    issuer: str = ISSUER,
    audience: str = AUDIENCE,
    expires_in_seconds: int = 3600,
) -> str:
    now = int(time.time())
    payload = {
        "sub": email,
        "email": email,
        "iss": issuer,
        "aud": audience,
        "iat": now,
        "exp": now + expires_in_seconds,
    }
    return jwt.encode(
        payload,
        private_key,
        algorithm="RS256",
        headers={"kid": "acl-e2e-key"},
    )


def write_contract_evidence(run_dir: Path, repo_url: str) -> None:
    evidence = {
        "repo_url": repo_url,
        "successful_flows": ["clone", "hydrate", "fetch", "protected_push"],
        "denied_flows": [
            "path_write",
            "malformed_token",
            "expired_token",
            "wrong_audience",
            "unmatched_identity",
            "invalid_repo_url",
            "unknown_request_field",
        ],
        "retry_flows": ["credential_request"],
    }
    (run_dir / "contract-evidence.json").write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def configure_auth_env(
    *,
    local: Any,
    policy_path: Path,
    endpoint: str,
    jwks_url: str,
    region: str,
    access_key_id: str,
    secret_access_key: str,
) -> None:
    for key in ("AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN", "CRAB_AUTH_S3_SESSION_TOKEN"):
        os.environ.pop(key, None)
    os.environ.update({
        "CRAB_AUTH_POLICY_PATH": str(policy_path),
        "CRAB_AUTH_JWKS_URL": jwks_url,
        "CRAB_AUTH_ISSUER": ISSUER,
        "CRAB_AUTH_AUDIENCE": AUDIENCE,
        "CRAB_AUTH_S3_ACCESS_KEY_ID": access_key_id,
        "CRAB_AUTH_S3_SECRET_ACCESS_KEY": secret_access_key,
        "CRAB_AUTH_S3_REGION": region,
        "CRAB_AUTH_VIEW_HELPER": str(local.VIEW_HELPER),
        "CRAB_AUTH_RECEIVE_HELPER": str(local.RECEIVE_HELPER),
        "AWS_ACCESS_KEY_ID": access_key_id,
        "AWS_SECRET_ACCESS_KEY": secret_access_key,
        "AWS_REGION": region,
        "AWS_DEFAULT_REGION": region,
        "AWS_ENDPOINT_URL": endpoint,
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "AWS_EC2_METADATA_DISABLED": "true",
        "CRAB_AUTH_DRY_RUN": "false",
    })


def cli_env(
    local: Any,
    home: Path,
    endpoint: str,
    bin_dir: Path,
    region: str,
    access_key_id: str,
    secret_access_key: str,
) -> dict[str, str]:
    env = os.environ.copy()
    env.update({
        "HOME": str(home),
        "CRAB_NO_KEYCHAIN": "1",
        "AWS_ACCESS_KEY_ID": access_key_id,
        "AWS_SECRET_ACCESS_KEY": secret_access_key,
        "AWS_REGION": region,
        "AWS_DEFAULT_REGION": region,
        "AWS_ENDPOINT_URL": endpoint,
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "AWS_EC2_METADATA_DISABLED": "true",
        "PATH": f"{bin_dir}:{local.TARGET}:{env.get('PATH', '')}",
        "GIT_CONFIG_NOSYSTEM": "1",
    })
    env.pop("AWS_SESSION_TOKEN", None)
    env.pop("AWS_SECURITY_TOKEN", None)
    return env


def _required_env(*names: str) -> str:
    value = _env(*names)
    if not value:
        joined = " or ".join(names)
        raise SystemExit(f"set {joined} before running this smoke test")
    return value


def _env(*names: str, default: str = "") -> str:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return default


if __name__ == "__main__":
    main()
