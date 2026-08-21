"""GCP credential provider — short-lived downscoped access tokens.

Generates short-lived OAuth2 access tokens constrained by Cloud Storage
Credential Access Boundaries. The token can only exercise permissions
already held by the auth service principal and only for the repository
prefixes included in the boundary.
"""

from __future__ import annotations

import asyncio
import os
from datetime import datetime, timedelta, timezone
from urllib.parse import quote

import structlog

from src.providers import (
    CredentialResult,
    normalize_permissions,
    validate_protected_push_permissions,
)
from src.repo_url import (
    RepoUrlError,
    normalize_repo_prefix,
    normalize_upload_prefix,
    parse_repo_url,
)

logger = structlog.get_logger()


class GcpProvider:
    """Generate short-lived GCP access tokens via service account impersonation."""

    def __init__(self) -> None:
        self._project_id = os.environ.get("CRAB_AUTH_GCP_PROJECT", "")
        self._sa_email = os.environ.get("CRAB_AUTH_GCP_SA_EMAIL", "")
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
        """Generate a short-lived GCP access token."""
        bucket, prefix = _parse_repo_url(repo_url)
        permissions = normalize_permissions(permissions)
        validate_protected_push_permissions(operation, permissions, upload_prefix)
        upload_prefix = _normalize_upload_prefix(upload_prefix, repo_prefix=prefix)
        if "immutable-write" in permissions and not upload_prefix:
            raise ValueError("immutable-write credentials require an upload_prefix")

        return await asyncio.to_thread(
            self._generate_sync,
            identity=identity,
            repo_url=repo_url,
            operation=operation,
            bucket=bucket,
            prefix=prefix,
            permissions=permissions,
            upload_prefix=upload_prefix,
        )

    def _generate_sync(
        self,
        *,
        identity: str,
        repo_url: str,
        operation: str,
        bucket: str,
        prefix: str,
        permissions: list[str],
        upload_prefix: str | None,
    ) -> CredentialResult:
        import google.auth
        from google.auth import downscoped
        from google.auth.transport import requests

        logger.debug(
            "generating_gcp_downscoped_token",
            sa_email=self._sa_email,
            identity=identity,
            repo_url=repo_url,
            operation=operation,
            permissions=permissions,
        )

        source_credentials, _project = google.auth.default(
            scopes=["https://www.googleapis.com/auth/cloud-platform"]
        )
        boundary = downscoped.CredentialAccessBoundary(
            _build_access_boundary_rules(
                downscoped,
                bucket=bucket,
                repo_prefix=prefix,
                upload_prefix=upload_prefix,
                permissions=permissions,
            )
        )
        credentials = downscoped.Credentials(source_credentials, boundary)
        credentials.refresh(requests.Request())
        expires_at = credentials.expiry.strftime("%Y-%m-%dT%H:%M:%SZ")

        return CredentialResult(
            credentials={
                "access_token": credentials.token,
            },
            expires_at=expires_at,
        )

    def cleanup_staging(self, *, repo_url: str, older_than_seconds: int) -> int:
        """Delete abandoned staging objects older than the configured TTL."""
        if older_than_seconds <= 0:
            return 0

        bucket, prefix = _parse_repo_url(repo_url)
        staging_prefix = f"{normalize_repo_prefix(prefix)}/staging/"
        cutoff = datetime.now(timezone.utc) - timedelta(seconds=older_than_seconds)

        import google.auth
        from google.auth.transport.requests import AuthorizedSession

        credentials, _project = google.auth.default(
            scopes=["https://www.googleapis.com/auth/cloud-platform"]
        )
        session = AuthorizedSession(credentials)
        deleted = 0
        page_token = None
        base_url = f"https://storage.googleapis.com/storage/v1/b/{bucket}/o"

        while True:
            params = {
                "prefix": staging_prefix,
                "fields": "nextPageToken,items(name,updated)",
            }
            if page_token:
                params["pageToken"] = page_token
            response = session.get(base_url, params=params)
            response.raise_for_status()
            payload = response.json()
            for item in payload.get("items", []):
                name = item.get("name")
                updated = item.get("updated")
                if not name or not updated:
                    continue
                updated_at = datetime.fromisoformat(
                    updated.replace("Z", "+00:00")
                )
                if updated_at >= cutoff:
                    continue
                delete_response = session.delete(
                    f"{base_url}/{quote(name, safe='')}"
                )
                if delete_response.status_code not in {404}:
                    delete_response.raise_for_status()
                deleted += 1
            page_token = payload.get("nextPageToken")
            if not page_token:
                break
        return deleted


def _parse_repo_url(repo_url: str) -> tuple[str, str]:
    parsed = parse_repo_url(repo_url, allowed_schemes={"crab", "gs", "gcs"})
    return parsed.bucket, parsed.prefix


def _build_access_boundary_rules(
    downscoped,
    *,
    bucket: str,
    repo_prefix: str,
    upload_prefix: str | None,
    permissions: list[str],
) -> list:
    permissions = normalize_permissions(permissions)
    if "immutable-write" in permissions and "read" in permissions:
        raise ValueError("immutable-write credentials cannot include read permission")
    try:
        repo_prefix = normalize_repo_prefix(repo_prefix)
        if upload_prefix is not None:
            upload_prefix = normalize_upload_prefix(
                upload_prefix,
                repo_prefix=repo_prefix,
            )
    except RepoUrlError as e:
        raise ValueError(str(e)) from e

    resource = f"//storage.googleapis.com/projects/_/buckets/{bucket}"
    rules = []
    if "read" in permissions:
        rules.append(
            downscoped.AccessBoundaryRule(
                available_resource=resource,
                available_permissions=["inRole:roles/storage.objectViewer"],
                availability_condition={
                    "expression": _gcs_prefix_expression(bucket, repo_prefix),
                },
            )
        )
    if "write" in permissions:
        rules.append(
            downscoped.AccessBoundaryRule(
                available_resource=resource,
                available_permissions=["inRole:roles/storage.objectAdmin"],
                availability_condition={
                    "expression": _gcs_prefix_expression(bucket, repo_prefix),
                },
            )
        )
    if "immutable-write" in permissions:
        rules.append(
            downscoped.AccessBoundaryRule(
                available_resource=resource,
                available_permissions=["inRole:roles/storage.objectCreator"],
                availability_condition={
                    "expression": _gcs_prefix_expression(bucket, upload_prefix or ""),
                },
            )
        )
    return rules


def _gcs_prefix_expression(bucket: str, prefix: str) -> str:
    try:
        parsed = parse_repo_url(f"crab://{bucket}/{prefix}", allowed_schemes={"crab"})
        normalized = parsed.prefix
    except RepoUrlError as e:
        raise ValueError(str(e)) from e
    return (
        f"resource.name.startsWith('projects/_/buckets/{parsed.bucket}/objects/{normalized}/')"
        " || "
        "api.getAttribute('storage.googleapis.com/objectListPrefix', '')"
        f".startsWith('{normalized}/')"
    )


def _normalize_upload_prefix(
    upload_prefix: str | None,
    *,
    repo_prefix: str,
) -> str | None:
    try:
        return normalize_upload_prefix(upload_prefix, repo_prefix=repo_prefix)
    except RepoUrlError as e:
        raise ValueError(str(e)) from e
