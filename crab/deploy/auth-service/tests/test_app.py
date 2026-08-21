"""Tests for the FastAPI credential endpoint."""

from __future__ import annotations

from typing import Any

import pytest
from fastapi.testclient import TestClient
from structlog.testing import capture_logs

import src.app as app_module
from src.auth import TokenClaims
from src.providers import CredentialResult
from src.receive_helper import (
    ReceiveCommitResult,
    ReceiveConflictError,
    ReceiveInvalidBundleError,
    ReceivePrepareResult,
    ReceiveRuntimeStatus,
    ReceiveVerifyResult,
)
from src.view_helper import ViewMaterializationResult, ViewRuntimeStatus


class FakeVerifier:
    def __init__(
        self,
        *,
        error: str | None = None,
        email: str = "alice@corp.example.com",
        groups: list[str] | None = None,
    ) -> None:
        self._error = error
        self._email = email
        self._groups = ["platform-admins"] if groups is None else groups

    async def verify(self, token: str) -> TokenClaims:
        if self._error:
            raise ValueError(self._error)
        return TokenClaims(
            subject="user-123",
            email=self._email,
            name=None,
            groups=self._groups,
        )

    async def check_runtime(self) -> dict[str, int]:
        if self._error:
            raise ValueError(self._error)
        return {"key_count": 1}


class FakeProvider:
    def __init__(
        self,
        *,
        error: Exception | None = None,
        cleanup_error: Exception | None = None,
        events: list[str] | None = None,
    ) -> None:
        self._error = error
        self._cleanup_error = cleanup_error
        self._events = events
        self.calls = []
        self.cleanup_calls = []

    async def generate(
        self,
        identity: str,
        repo_url: str,
        operation: str,
        permissions: list[str],
        upload_prefix: str | None = None,
    ) -> CredentialResult:
        if self._error:
            raise self._error
        self.calls.append({
            "identity": identity,
            "repo_url": repo_url,
            "operation": operation,
            "permissions": permissions,
            "upload_prefix": upload_prefix,
        })
        return CredentialResult(
            credentials={"access_key_id": "AKIA", "secret_access_key": "secret"},
            expires_at="2026-06-06T00:00:00Z",
        )

    def cleanup_staging(self, *, repo_url: str, older_than_seconds: int) -> int:
        if self._events is not None:
            self._events.append("cleanup")
        self.cleanup_calls.append({
            "repo_url": repo_url,
            "older_than_seconds": older_than_seconds,
        })
        if self._cleanup_error:
            raise self._cleanup_error
        return 2


class FakeReceiveHelper:
    def __init__(
        self,
        *,
        error: Exception | None = None,
        commit_error: Exception | None = None,
        events: list[str] | None = None,
    ) -> None:
        self._error = error
        self._commit_error = commit_error
        self._events = events
        self.prepare_calls = []
        self.verify_calls = []
        self.commit_calls = []
        self.ref_updates = ref_updates()
        self.commit_ref_updates = self.ref_updates
        self.commit_metadata: dict[str, object] = {}
        self.verified_paths = ["src/lib.rs"]

    def prepare(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
        ref_updates: list[dict[str, str | None]],
        view_scope: dict[str, str] | None = None,
    ) -> ReceivePrepareResult:
        if self._error:
            raise self._error
        if self._events is not None:
            self._events.append("prepare")
        self.prepare_calls.append({
            "repo_url": repo_url,
            "push_id": push_id,
            "provider": provider,
            "ref_updates": ref_updates,
            "view_scope": view_scope,
        })
        return ReceivePrepareResult(status="prepared", source_generation=4)

    def verify(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
    ) -> ReceiveVerifyResult:
        if self._error:
            raise self._error
        if self._events is not None:
            self._events.append("verify")
        self.verify_calls.append({
            "repo_url": repo_url,
            "push_id": push_id,
            "provider": provider,
        })
        return ReceiveVerifyResult(
            ref_updates=self.ref_updates,
            verified_changed_paths=self.verified_paths,
            plan_digest="abc123",
        )

    def commit(
        self,
        *,
        repo_url: str,
        push_id: str,
        plan_digest: str,
        provider: str,
        active_active: dict[str, Any] | None = None,
    ) -> ReceiveCommitResult:
        if self._events is not None:
            self._events.append("commit")
        call = {
            "repo_url": repo_url,
            "push_id": push_id,
            "plan_digest": plan_digest,
            "provider": provider,
        }
        if active_active is not None:
            call["active_active"] = active_active
        self.commit_calls.append(call)
        if self._commit_error:
            raise self._commit_error
        if self._error:
            raise self._error
        return ReceiveCommitResult(
            status="updated",
            ref_updates=self.commit_ref_updates,
            **self.commit_metadata,
        )

    def check_runtime(self) -> ReceiveRuntimeStatus:
        if self._error:
            raise self._error
        return ReceiveRuntimeStatus(
            status="ok",
            git_version="git version 2.50.0",
        )


class FakeViewHelper:
    def __init__(self, *, error: Exception | None = None) -> None:
        self._error = error
        self.calls = []

    def materialize(
        self,
        *,
        repo_url: str,
        provider: str,
        scope_hash: str,
        read_paths: list[str],
        denied_read_paths: list[str],
    ) -> ViewMaterializationResult:
        if self._error:
            raise self._error
        self.calls.append({
            "repo_url": repo_url,
            "provider": provider,
            "scope_hash": scope_hash,
            "read_paths": read_paths,
            "denied_read_paths": denied_read_paths,
        })
        return ViewMaterializationResult(
            repo_prefix=f"restricted/repo/acl-views/v1/{scope_hash}/7-deadbeef",
            global_prefix=f"restricted/repo/acl-views/v1/{scope_hash}/7-deadbeef/.crab",
            source_repo="restricted/repo",
            scope_hash=scope_hash,
            source_generation=7,
            source_manifest_hash="deadbeef",
            cache_hit=True,
        )

    def check_runtime(self) -> ViewRuntimeStatus:
        if self._error:
            raise self._error
        return ViewRuntimeStatus(
            status="ok",
            git_version="git version 2.50.0",
        )


def ref_updates(
    *,
    ref_name: str = "refs/heads/main",
    old_oid: str | None = "0" * 40,
    new_oid: str = "1" * 40,
) -> list[dict[str, str | None]]:
    return [{
        "ref_name": ref_name,
        "old_oid": old_oid,
        "new_oid": new_oid,
    }]


def active_active_payload() -> dict[str, object]:
    return {
        "replication": {
            "mode": "active-active",
            "coordinator": {
                "kind": "managed",
                "url": "dynamodb://crab-coordinator",
                "region": "us-west-2",
                "failover_regions": ["us-east-1"],
                "consistency": "linearizable",
            },
            "writers": [
                {
                    "name": "west",
                    "url": "crab://bucket/restricted/repo",
                    "region": "us-west-2",
                    "enabled": True,
                },
                {
                    "name": "east",
                    "url": "crab://bucket-east/restricted/repo",
                    "region": "us-east-1",
                    "enabled": True,
                },
            ],
        },
        "writer": "west",
    }


@pytest.fixture(autouse=True)
def reset_app(monkeypatch):
    app_module._verifier = FakeVerifier()
    app_module._policy = None
    app_module._rate_limiter = app_module.RateLimiter(rate_per_minute=0, burst=1)
    app_module._receive_helper = FakeReceiveHelper()
    app_module._view_helper = None
    monkeypatch.setenv("CRAB_AUTH_JWKS_URL", "https://idp.example.com/jwks.json")
    monkeypatch.setenv("CRAB_AUTH_ISSUER", "https://idp.example.com")
    monkeypatch.setenv("CRAB_AUTH_AUDIENCE", "crab-cli")
    monkeypatch.setenv(
        "CRAB_AUTH_AWS_ROLE_ARN",
        "arn:aws:iam::123456789012:role/crab-auth",
    )
    monkeypatch.delenv("CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON", raising=False)
    monkeypatch.delenv("CRAB_AUTH_ACTIVE_ACTIVE_ALLOW_CLIENT_CONFIG", raising=False)
    monkeypatch.delenv("CRAB_AUTH_POLICY_PATH", raising=False)
    monkeypatch.delenv("CRAB_AUTH_DRY_RUN", raising=False)
    monkeypatch.delenv("CRAB_AUTH_RATE_LIMIT_MAX_KEYS", raising=False)
    monkeypatch.setattr(app_module, "get_provider", lambda name: FakeProvider())
    yield
    app_module._verifier = None
    app_module._policy = None
    app_module._rate_limiter = None
    app_module._receive_helper = None
    app_module._view_helper = None


@pytest.fixture
def client(sample_policy):
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    return TestClient(app_module.app)


def test_issue_credentials_success(client):
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
            "client_version": "1.0.0",
        },
    )
    assert response.status_code == 200
    body = response.json()
    assert body["provider"] == "aws"
    assert body["permissions"] == ["read"]
    assert "credentials" in body


def test_issue_credentials_invalid_token_returns_401(client):
    app_module._verifier = FakeVerifier(error="bad token")
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "bad",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
        },
    )
    assert response.status_code == 401
    assert response.json()["detail"]["error"] == "unauthorized"


def test_issue_credentials_rejects_mismatched_authorization_header(client):
    response = client.post(
        "/v1/credentials",
        headers={"Authorization": "Bearer other-token"},
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
        },
    )
    assert response.status_code == 401
    assert response.json()["detail"]["error"] == "unauthorized"


def test_push_prepare_rejects_mismatched_authorization_header(client):
    response = client.post(
        "/v1/push/prepare",
        headers={"Authorization": "Bearer other-token"},
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "ref_updates": ref_updates(),
        },
    )
    assert response.status_code == 401
    assert response.json()["detail"]["error"] == "unauthorized"


def test_issue_credentials_push_requires_protected_flow(client):
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "push",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "push_requires_protected_flow"


def test_issue_credentials_push_cutoff_normalizes_operation(client):
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": " Push ",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "push_requires_protected_flow"


def test_issue_credentials_unknown_field_returns_400(client):
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
            "audience": "unexpected",
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"


def test_issue_credentials_policy_denial_returns_403(client):
    app_module._verifier = FakeVerifier(email="stranger@corp.example.com", groups=[])
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/private/repo",
            "operation": "fetch",
        },
    )
    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "forbidden"


def test_issue_credentials_rejects_protected_repo_write_without_service_flow(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "protected_repos": ["restricted/*"],
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["gc"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "operation": "gc",
        },
    )

    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "protected_repo_requires_service_flow"
    assert provider.calls == []


def test_issue_credentials_allows_protected_repo_read(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "protected_repos": ["restricted/*"],
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["fetch"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "operation": "fetch",
        },
    )

    assert response.status_code == 200
    body = response.json()
    assert body["permissions"] == ["read"]
    assert "storage_scope" not in body
    assert provider.calls[0]["permissions"] == ["read"]


def test_issue_credentials_path_scoped_read_uses_filtered_view(monkeypatch):
    provider = FakeProvider()
    view = FakeViewHelper()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["clone"],
                "read_paths": ["src/**", "README.md"],
            },
        ],
        "deny": [
            {
                "identity": "*",
                "repos": ["restricted/*"],
                "operations": ["clone"],
                "read_paths": ["src/secrets/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "operation": "clone",
        },
    )

    assert response.status_code == 200
    body = response.json()
    assert body["permissions"] == ["read"]
    scope = body["storage_scope"]
    assert scope["source_repo"] == "restricted/repo"
    assert scope["repo_prefix"].startswith("restricted/repo/acl-views/v1/")
    assert scope["global_prefix"] == f"{scope['repo_prefix']}/.crab"
    assert view.calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "provider": "aws",
        "scope_hash": scope["scope_hash"],
        "read_paths": ["src/**", "README.md"],
        "denied_read_paths": ["src/secrets/**"],
    }]
    assert provider.calls[0]["repo_url"] == f"crab://bucket/{scope['repo_prefix']}"
    assert provider.calls[0]["permissions"] == ["read"]


def test_issue_credentials_path_scoped_read_audits_view_scope(monkeypatch):
    provider = FakeProvider()
    view = FakeViewHelper()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["clone"],
                "read_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    with capture_logs() as logs:
        response = client.post(
            "/v1/credentials",
            json={
                "id_token": "token",
                "repo_url": "crab://bucket/restricted/repo",
                "operation": "clone",
            },
        )

    assert response.status_code == 200
    scope = response.json()["storage_scope"]
    view_events = [event for event in logs if event["event"] == "view_materialized"]
    assert len(view_events) == 1
    assert view_events[0]["identity"] == "alice@corp.example.com"
    assert view_events[0]["groups_hash"] == app_module._hash_values(["platform-admins"])
    assert view_events[0]["provider"] == "aws"
    assert view_events[0]["scope_hash"] == scope["scope_hash"]
    assert view_events[0]["source_generation"] == 7
    assert view_events[0]["view_cache_hit"] is True
    assert view_events[0]["read_path_hash"] == app_module._hash_values(["src/**"])
    credential_events = [
        event for event in logs if event["event"] == "credentials_issued"
    ]
    assert len(credential_events) == 1
    assert credential_events[0]["identity"] == "alice@corp.example.com"
    assert credential_events[0]["groups_hash"] == app_module._hash_values([
        "platform-admins"
    ])
    assert credential_events[0]["scope_hash"] == scope["scope_hash"]


def test_issue_credentials_path_scoped_read_does_not_mint_on_view_failure(monkeypatch):
    provider = FakeProvider()
    view = FakeViewHelper(error=RuntimeError("view contains crab pointers"))
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["clone"],
                "read_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "operation": "clone",
        },
    )

    assert response.status_code == 500
    assert response.json()["detail"]["error"] == "internal"
    assert provider.calls == []


def test_issue_credentials_repo_wide_read_skips_view_helper(monkeypatch):
    provider = FakeProvider()
    view = FakeViewHelper()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["fetch"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "operation": "fetch",
        },
    )

    assert response.status_code == 200
    assert "storage_scope" not in response.json()
    assert view.calls == []
    assert provider.calls[0]["repo_url"] == "crab://bucket/restricted/repo"


def test_issue_credentials_provider_failure_returns_500(client, monkeypatch):
    monkeypatch.setattr(
        app_module,
        "get_provider",
        lambda name: FakeProvider(error=RuntimeError("boom")),
    )
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
        },
    )
    assert response.status_code == 500
    assert response.json()["detail"]["error"] == "internal"


def test_issue_credentials_invalid_repo_url_returns_400(client, monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/team/*",
            "operation": "fetch",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_repo_url"
    assert provider.calls == []


def test_issue_credentials_oversized_operation_returns_400(client, monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    response = client.post(
        "/v1/credentials",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/team/repo",
            "operation": "x" * (app_module.MAX_OPERATION_LEN + 1),
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert provider.calls == []


def test_issue_credentials_rate_limited(client):
    app_module._rate_limiter = app_module.RateLimiter(rate_per_minute=60, burst=1)
    payload = {
        "id_token": "token",
        "repo_url": "crab://bucket/any/repo",
        "operation": "fetch",
    }

    assert client.post("/v1/credentials", json=payload).status_code == 200
    response = client.post("/v1/credentials", json=payload)
    assert response.status_code == 429
    assert response.headers["retry-after"] == "1"
    assert response.json()["detail"]["error"] == "rate_limited"


@pytest.mark.asyncio
async def test_rate_limiter_evicts_oldest_key_when_capacity_is_reached():
    limiter = app_module.RateLimiter(rate_per_minute=60, burst=1, max_keys=2)

    assert await limiter.allow("198.51.100.1") == (True, 0)
    assert await limiter.allow("198.51.100.2") == (True, 0)
    assert await limiter.allow("198.51.100.3") == (True, 0)

    assert set(limiter._buckets) == {"198.51.100.2", "198.51.100.3"}


def test_client_rate_limit_key_ignores_invalid_trusted_proxy_header(client, monkeypatch):
    monkeypatch.setenv("CRAB_AUTH_TRUST_PROXY_HEADERS", "true")
    app_module._rate_limiter = app_module.RateLimiter(rate_per_minute=60, burst=10)

    response = client.post(
        "/v1/credentials",
        headers={"X-Forwarded-For": "not-an-ip"},
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/any/repo",
            "operation": "fetch",
        },
    )

    assert response.status_code == 200
    assert set(app_module._rate_limiter._buckets) == {"unknown"}


def test_push_prepare_returns_immutable_credentials_for_protected_repo(monkeypatch):
    provider = FakeProvider()
    provider_names = []

    def capture_provider(name):
        provider_names.append(name)
        return provider

    monkeypatch.setattr(app_module, "get_provider", capture_provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "gcp",
        "protected_repos": ["restricted/*"],
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**", "README.md"],
                "provider": "gcp",
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "client_version": "1.0.0",
        },
    )
    assert response.status_code == 200
    body = response.json()
    assert body["permissions"] == ["immutable-write"]
    assert body["provider"] == "gcp"
    assert body["upload_prefix"].startswith("restricted/repo/staging/")
    assert provider_names == ["gcp"]
    assert provider.calls[0]["permissions"] == ["immutable-write"]
    assert provider.calls[0]["upload_prefix"] == body["upload_prefix"]
    assert provider.cleanup_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "older_than_seconds": app_module.DEFAULT_STAGING_TTL_SECONDS,
    }]


def test_push_prepare_always_returns_immutable_write(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["open/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/open/repo",
            "ref_updates": ref_updates(),
        },
    )
    assert response.status_code == 200
    assert response.json()["permissions"] == ["immutable-write"]
    assert provider.calls[0]["permissions"] == ["immutable-write"]
    assert provider.calls[0]["upload_prefix"] == response.json()["upload_prefix"]


def test_push_prepare_uses_configured_staging_cleanup_ttl(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setenv("CRAB_AUTH_STAGING_TTL_SECONDS", "60")
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["open/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/open/repo",
            "ref_updates": ref_updates(),
        },
    )

    assert response.status_code == 200
    assert provider.cleanup_calls == [{
        "repo_url": "crab://bucket/open/repo",
        "older_than_seconds": 60,
    }]


def test_push_prepare_is_repo_level_for_path_scoped_rules(monkeypatch):
    provider = FakeProvider()
    receive = FakeReceiveHelper()
    app_module._receive_helper = receive
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
        },
    )
    assert response.status_code == 200
    assert response.json()["permissions"] == ["immutable-write"]
    assert provider.calls[0]["permissions"] == ["immutable-write"]
    assert receive.prepare_calls[0]["view_scope"] is None


def test_push_prepare_passes_filtered_read_scope_to_receive_helper(monkeypatch):
    provider = FakeProvider()
    receive = FakeReceiveHelper()
    view = FakeViewHelper()
    app_module._receive_helper = receive
    app_module._view_helper = view
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["fetch", "push"],
                "read_paths": ["src/**"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
        },
    )

    assert response.status_code == 200
    assert view.calls[0]["read_paths"] == ["src/**"]
    view_scope = receive.prepare_calls[0]["view_scope"]
    assert view_scope == {
        "repo_prefix": view.calls[0]["repo_url"].replace(
            "crab://bucket/", ""
        )
        + f"/acl-views/v1/{view.calls[0]['scope_hash']}/7-deadbeef",
        "global_prefix": (
            "restricted/repo/acl-views/v1/"
            f"{view.calls[0]['scope_hash']}/7-deadbeef/.crab"
        ),
        "source_repo": "restricted/repo",
        "scope_hash": view.calls[0]["scope_hash"],
    }


def test_push_prepare_rejects_client_changed_paths_before_provider(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "changed_paths": ["src/lib.rs"],
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert provider.calls == []


def test_push_prepare_invalid_ref_returns_400():
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(ref_name="heads/main"),
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_ref"


def test_push_prepare_duplicate_ref_returns_400():
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates() + ref_updates(),
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_ref"


def test_push_prepare_unknown_ref_update_field_returns_400(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": "0" * 40,
                    "new_oid": "1" * 40,
                    "force": True,
                }
            ],
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert provider.calls == []


def test_push_prepare_too_many_ref_updates_returns_400(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": [
                {
                    "ref_name": f"refs/heads/branch-{idx}",
                    "old_oid": "0" * 40,
                    "new_oid": f"{idx % 16:x}" * 40,
                }
                for idx in range(app_module.MAX_REF_UPDATES + 1)
            ],
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert provider.calls == []


def test_push_prepare_ref_delete_returns_400():
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(new_oid="0" * 40),
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_ref"


def test_push_prepare_noop_ref_update_returns_400(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(old_oid="a" * 40, new_oid="A" * 40),
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_ref"
    assert provider.calls == []


def test_push_prepare_invalid_repo_url_returns_400(monkeypatch):
    provider = FakeProvider()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/prepare",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket",
            "ref_updates": ref_updates(),
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_repo_url"
    assert provider.calls == []


def test_push_finalize_verifies_paths_before_commit(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )
    assert response.status_code == 200
    assert response.json() == {
        "status": "updated",
        "ref_updates": ref_updates(),
    }
    assert receive.verify_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "provider": "aws",
    }]
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "aws",
    }]


def test_push_finalize_rejects_active_active_without_service_config(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "active_active": active_active_payload(),
        },
    )

    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "active_active_config_required"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_passes_approved_active_active_context(monkeypatch):
    active_active = active_active_payload()
    monkeypatch.setenv(
        "CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON",
        app_module._canonical_json(active_active),
    )
    receive = FakeReceiveHelper()
    receive.commit_metadata = {
        "operation_id": "op-123",
        "coordinator_epoch": 7,
        "writer_region": "us-west-2",
        "manifest_generation": 42,
        "commit_state": "materialized",
    }
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "active_active": active_active,
        },
    )

    assert response.status_code == 200
    assert response.json() == {
        "status": "updated",
        "ref_updates": ref_updates(),
        "operation_id": "op-123",
        "coordinator_epoch": 7,
        "writer_region": "us-west-2",
        "manifest_generation": 42,
        "commit_state": "materialized",
    }
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "aws",
        "active_active": active_active,
    }]


def test_push_finalize_cleanup_failure_does_not_block_commit(monkeypatch):
    provider = FakeProvider(cleanup_error=RuntimeError("cleanup failed"))
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 200
    assert provider.cleanup_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "older_than_seconds": app_module.DEFAULT_STAGING_TTL_SECONDS,
    }]
    assert receive.commit_calls


def test_push_finalize_cleans_staging_after_commit(monkeypatch):
    events = []
    provider = FakeProvider(events=events)
    receive = FakeReceiveHelper(events=events)
    monkeypatch.setattr(app_module, "get_provider", lambda name: provider)
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 200
    assert events == ["verify", "commit", "cleanup"]


def test_push_finalize_success_audit_event_has_enterprise_fields(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    with capture_logs() as logs:
        response = client.post(
            "/v1/push/finalize",
            json={
                "id_token": "token",
                "repo_url": "crab://bucket/restricted/repo",
                "ref_updates": ref_updates(),
                "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            },
        )

    assert response.status_code == 200
    events = [event for event in logs if event["event"] == "push_finalized"]
    assert len(events) == 1
    event = events[0]
    assert event["repo_url"] == "crab://bucket/restricted/repo"
    assert event["refs"] == ["refs/heads/main"]
    assert event["push_id"] == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    assert event["identity"] == "alice@corp.example.com"
    assert event["groups_hash"] == app_module._hash_values(["platform-admins"])
    assert event["provider"] == "aws"
    assert event["policy_decision"] == "allowed"
    assert event["cas_result"] == "committed"
    assert event["verified_path_count"] == 1
    assert event["verified_path_hash"] == app_module._hash_values(["src/lib.rs"])
    assert "id_token" not in event
    assert "credentials" not in event
    assert "changed_paths" not in event
    assert "verified_changed_paths" not in event


def test_push_finalize_passes_policy_provider_to_receive_helper(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "azure",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "provider": "azure",
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 200
    assert receive.verify_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "provider": "azure",
    }]
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "azure",
    }]


def test_push_finalize_invalid_repo_url_returns_400_before_verify(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_repo_url"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_unknown_field_returns_400_before_verify(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "plan_digest": "a" * 64,
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_invalid_push_id_returns_400_before_verify(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "../aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_push_id"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_denies_ambiguous_provider_before_verify(monkeypatch):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
                "provider": "aws",
            },
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["docs/**"],
                "provider": "gcp",
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "forbidden"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_conflict_returns_409(monkeypatch):
    monkeypatch.setattr(
        app_module,
        "get_receive_helper",
        lambda: FakeReceiveHelper(error=ReceiveConflictError("changed")),
    )
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )
    assert response.status_code == 409
    assert response.json()["detail"]["error"] == "manifest_conflict"


def test_push_finalize_commit_conflict_returns_409(monkeypatch):
    receive = FakeReceiveHelper(
        commit_error=ReceiveConflictError("manifest changed during commit")
    )
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 409
    assert response.json()["detail"]["error"] == "manifest_conflict"
    assert receive.verify_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "provider": "aws",
    }]
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "aws",
    }]


def test_push_finalize_rejects_mismatched_staged_ref_updates(monkeypatch):
    receive = FakeReceiveHelper()
    receive.ref_updates = ref_updates(new_oid="2" * 40)
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_bundle"
    assert receive.commit_calls == []


@pytest.mark.parametrize(
    "changed_paths",
    [
        [" src/lib.rs"],
        ["/src/lib.rs"],
        ["src//lib.rs"],
        ["src/../secret.env"],
        ["src/bad\nname.rs"],
        ["src/lib.rs", "src/lib.rs"],
    ],
)
def test_push_finalize_invalid_client_changed_paths_return_400_before_verify(
    monkeypatch, changed_paths
):
    receive = FakeReceiveHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "changed_paths": changed_paths,
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_request"
    assert receive.verify_calls == []
    assert receive.commit_calls == []


def test_push_finalize_rejects_invalid_verified_changed_path(monkeypatch):
    receive = FakeReceiveHelper()
    receive.verified_paths = ["src//lib.rs"]
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_bundle"
    assert "invalid changed path" in response.json()["detail"]["message"]
    assert receive.commit_calls == []


def test_push_finalize_accepts_multiple_verified_paths_without_client_echo(monkeypatch):
    receive = FakeReceiveHelper()
    receive.verified_paths = ["README.md", "src/lib.rs"]
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 200
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "aws",
    }]


def test_push_finalize_compares_ref_updates_after_oid_normalization(monkeypatch):
    receive = FakeReceiveHelper()
    receive.ref_updates = ref_updates(old_oid="a" * 40, new_oid="b" * 40)
    receive.commit_ref_updates = receive.ref_updates
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(old_oid="A" * 40, new_oid="B" * 40),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 200
    assert receive.commit_calls == [{
        "repo_url": "crab://bucket/restricted/repo",
        "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_digest": "abc123",
        "provider": "aws",
    }]


def test_push_finalize_rejects_mismatched_committed_ref_updates(monkeypatch):
    receive = FakeReceiveHelper()
    receive.commit_ref_updates = ref_updates(
        ref_name="refs/heads/other",
        new_oid="2" * 40,
    )
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    with capture_logs() as logs:
        response = client.post(
            "/v1/push/finalize",
            json={
                "id_token": "token",
                "repo_url": "crab://bucket/restricted/repo",
                "ref_updates": ref_updates(),
                "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            },
        )

    assert response.status_code == 500
    assert response.json()["detail"]["error"] == "internal"
    events = [event for event in logs if event["event"] == "push_finalize_commit_failed"]
    assert len(events) == 1
    assert events[0]["cas_result"] == "commit_mismatch"
    assert events[0]["verified_path_hash"] == app_module._hash_values(["src/lib.rs"])
    assert "ref_updates" not in events[0]
    assert "changed_paths" not in events[0]


def test_push_finalize_invalid_ref_returns_400(monkeypatch):
    monkeypatch.setattr(
        app_module,
        "get_receive_helper",
        lambda: FakeReceiveHelper(error=ReceiveInvalidBundleError("bad bundle")),
    )
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(ref_name="../main"),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )
    assert response.status_code == 400
    assert response.json()["detail"]["error"] == "invalid_ref"


def test_push_finalize_denies_verified_disallowed_path(monkeypatch):
    receive = FakeReceiveHelper()
    receive.verified_paths = ["infra/prod.tf"]
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    with capture_logs() as logs:
        response = client.post(
            "/v1/push/finalize",
            json={
                "id_token": "token",
                "repo_url": "crab://bucket/restricted/repo",
                "ref_updates": ref_updates(),
                "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            },
        )
    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "forbidden"
    assert receive.commit_calls == []
    events = [event for event in logs if event["event"] == "push_finalize_policy_denied"]
    assert len(events) == 1
    event = events[0]
    assert event["policy_decision"] == "denied"
    assert event["cas_result"] == "not_attempted"
    assert event["verified_path_count"] == 1
    assert event["verified_path_hash"] == app_module._hash_values(["infra/prod.tf"])
    assert "changed_paths" not in event
    assert "verified_changed_paths" not in event


def test_push_finalize_denies_empty_verified_paths_for_path_scoped_rule(monkeypatch):
    receive = FakeReceiveHelper()
    receive.verified_paths = []
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["restricted/*"],
                "operations": ["push"],
                "write_paths": ["src/**"],
            },
        ],
    })
    client = TestClient(app_module.app)

    response = client.post(
        "/v1/push/finalize",
        json={
            "id_token": "token",
            "repo_url": "crab://bucket/restricted/repo",
            "ref_updates": ref_updates(),
            "push_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
    )

    assert response.status_code == 403
    assert response.json()["detail"]["error"] == "forbidden"
    assert receive.commit_calls == []


def test_health_is_not_rate_limited():
    app_module._rate_limiter = app_module.RateLimiter(rate_per_minute=1, burst=1)
    client = TestClient(app_module.app)
    assert client.get("/health").status_code == 200
    assert client.get("/health").status_code == 200


def test_ready_reports_runtime_dependencies(monkeypatch, sample_policy):
    receive = FakeReceiveHelper()
    view = FakeViewHelper()
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 200
    assert response.json() == {
        "status": "ok",
        "auth_config": "ok",
        "policy": "ok",
        "provider_config": "ok",
        "jwks": "ok",
        "jwks_key_count": 1,
        "receive_helper": "ok",
        "view_helper": "ok",
        "receive_git_version": "git version 2.50.0",
        "view_git_version": "git version 2.50.0",
    }


def test_ready_returns_503_without_leaking_receive_helper_error(
    monkeypatch,
    sample_policy,
):
    receive = FakeReceiveHelper(error=RuntimeError("git missing at /opt/bin/git"))
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 503
    assert response.json() == {
        "status": "not_ready",
        "component": "receive_helper",
        "message": "Crab Auth dependencies are unavailable",
    }
    assert "git missing" not in response.text


def test_ready_checks_view_helper_without_leaking_error(monkeypatch, sample_policy):
    receive = FakeReceiveHelper()
    view = FakeViewHelper(error=RuntimeError("missing /usr/local/bin/crab-auth-view"))
    monkeypatch.setattr(app_module, "get_receive_helper", lambda: receive)
    monkeypatch.setattr(app_module, "get_view_helper", lambda: view)
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 503
    assert response.json() == {
        "status": "not_ready",
        "component": "view_helper",
        "message": "Crab Auth dependencies are unavailable",
    }
    assert "crab-auth-view" not in response.text


def test_ready_requires_auth_configuration(monkeypatch, sample_policy):
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    monkeypatch.delenv("CRAB_AUTH_JWKS_URL")
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 503
    assert response.json() == {
        "status": "not_ready",
        "component": "auth_config",
        "message": "Crab Auth dependencies are unavailable",
    }
    assert "CRAB_AUTH_JWKS_URL" not in response.text


def test_ready_requires_policy_file_when_policy_not_preloaded(monkeypatch):
    monkeypatch.setenv("CRAB_AUTH_POLICY_PATH", "/tmp/missing-crab-auth-policy.yaml")
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 503
    assert response.json() == {
        "status": "not_ready",
        "component": "policy",
        "message": "Crab Auth dependencies are unavailable",
    }
    assert "missing-crab-auth-policy" not in response.text


def test_ready_requires_aws_role_for_aws_policy(sample_policy, monkeypatch):
    app_module._policy = app_module.PolicyEngine.from_dict(sample_policy)
    monkeypatch.delenv("CRAB_AUTH_AWS_ROLE_ARN")
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 503
    assert response.json() == {
        "status": "not_ready",
        "component": "provider_config",
        "message": "Crab Auth dependencies are unavailable",
    }
    assert "CRAB_AUTH_AWS_ROLE_ARN" not in response.text


def test_ready_does_not_require_unused_default_provider(monkeypatch):
    app_module._policy = app_module.PolicyEngine.from_dict({
        "version": "1",
        "default_provider": "aws",
        "rules": [
            {
                "group": "platform-admins",
                "repos": ["*"],
                "operations": ["clone"],
                "provider": "s3",
            },
        ],
    })
    monkeypatch.delenv("CRAB_AUTH_AWS_ROLE_ARN")
    monkeypatch.setenv("CRAB_AUTH_S3_ACCESS_KEY_ID", "crab")
    monkeypatch.setenv("CRAB_AUTH_S3_SECRET_ACCESS_KEY", "secret")
    app_module._view_helper = FakeViewHelper()
    client = TestClient(app_module.app)

    response = client.get("/ready")

    assert response.status_code == 200
    assert response.json()["provider_config"] == "ok"
