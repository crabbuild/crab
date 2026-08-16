"""S3-compatible static credential provider.

This provider is intended for local and self-hosted S3-compatible deployments
where the object store does not issue scoped STS credentials. The Crab Auth
policy still controls which users receive credentials and protected push still
keeps canonical refs service-owned, but the returned static key can access
whatever the object store grants to that key.
"""

from __future__ import annotations

import os
from datetime import datetime, timedelta, timezone

from src.providers import (
    CredentialResult,
    normalize_permissions,
    validate_protected_push_permissions,
)
from src.repo_url import normalize_upload_prefix, parse_repo_url


class S3Provider:
    """Return configured static credentials for S3-compatible object stores."""

    def __init__(self) -> None:
        self._access_key_id = _env("CRAB_AUTH_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID")
        self._secret_access_key = _env(
            "CRAB_AUTH_S3_SECRET_ACCESS_KEY",
            "AWS_SECRET_ACCESS_KEY",
        )
        self._session_token = os.environ.get("CRAB_AUTH_S3_SESSION_TOKEN", "")
        self._region = _env(
            "CRAB_AUTH_S3_REGION",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            default="us-east-1",
        )
        self._session_duration = int(
            os.environ.get("CRAB_AUTH_SESSION_DURATION", "3600")
        )

    async def generate(
        self,
        identity: str,
        repo_url: str,
        operation: str,
        permissions: list[str],
        upload_prefix: str | None = None,
    ) -> CredentialResult:
        """Return S3-compatible credentials after validating policy scope."""
        if not self._access_key_id:
            raise ValueError("CRAB_AUTH_S3_ACCESS_KEY_ID or AWS_ACCESS_KEY_ID is required")
        if not self._secret_access_key:
            raise ValueError(
                "CRAB_AUTH_S3_SECRET_ACCESS_KEY or AWS_SECRET_ACCESS_KEY is required"
            )

        parsed = parse_repo_url(repo_url, allowed_schemes={"crab", "s3"})
        permissions = normalize_permissions(permissions)
        validate_protected_push_permissions(operation, permissions, upload_prefix)
        if "immutable-write" in permissions:
            normalize_upload_prefix(upload_prefix, repo_prefix=parsed.prefix)

        expiry = datetime.now(timezone.utc) + timedelta(seconds=self._session_duration)
        credentials = {
            "access_key_id": self._access_key_id,
            "secret_access_key": self._secret_access_key,
            "region": self._region,
        }
        if self._session_token:
            credentials["session_token"] = self._session_token

        return CredentialResult(
            credentials=credentials,
            expires_at=expiry.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )

    def cleanup_staging(self, *, repo_url: str, older_than_seconds: int) -> int:
        """No-op cleanup for static S3-compatible credentials."""
        return 0


def _env(*names: str, default: str = "") -> str:
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return default
