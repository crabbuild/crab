"""Azure credential provider — scoped SAS tokens for Blob Storage.

Generates a user-delegation SAS token scoped to the specific container
and prefix (repository path). The SAS token grants only the permissions
needed for the requested operation.
"""

from __future__ import annotations

import asyncio
import os
from datetime import datetime, timedelta, timezone

import structlog

from src.providers import (
    CredentialResult,
    normalize_permissions,
    validate_protected_push_permissions,
)
from src.repo_url import RepoUrlError, normalize_upload_prefix, parse_repo_url

logger = structlog.get_logger()


class AzureProvider:
    """Generate scoped Azure SAS tokens for Blob Storage."""

    def __init__(self) -> None:
        self._tenant_id = os.environ.get("CRAB_AUTH_AZURE_TENANT_ID", "")
        self._subscription_id = os.environ.get(
            "CRAB_AUTH_AZURE_SUBSCRIPTION_ID", ""
        )
        self._storage_account = os.environ.get(
            "CRAB_AUTH_AZURE_STORAGE_ACCOUNT", ""
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
        """Generate a scoped SAS token for Azure Blob Storage."""
        container, prefix = _parse_repo_url(repo_url)
        permissions = normalize_permissions(permissions)
        validate_protected_push_permissions(operation, permissions, upload_prefix)
        upload_prefix = _normalize_upload_prefix(upload_prefix, repo_prefix=prefix)
        if "immutable-write" in permissions and not upload_prefix:
            raise ValueError("immutable-write credentials require an upload_prefix")
        if not self._storage_account:
            raise ValueError("Azure storage account is required")

        return await asyncio.to_thread(
            self._generate_sync,
            identity=identity,
            container=container,
            prefix=prefix,
            permissions=permissions,
            upload_prefix=upload_prefix,
        )

    def _generate_sync(
        self,
        *,
        identity: str,
        container: str,
        prefix: str,
        permissions: list[str],
        upload_prefix: str | None,
    ) -> CredentialResult:
        from azure.identity import DefaultAzureCredential
        from azure.storage.blob import (
            BlobServiceClient,
            generate_blob_sas,
            UserDelegationKey,
        )

        logger.debug(
            "generating_azure_sas",
            storage_account=self._storage_account,
            container=container,
            prefix=prefix,
            identity=identity,
        )

        # Authenticate as the auth service itself.
        credential = DefaultAzureCredential()
        account_url = f"https://{self._storage_account}.blob.core.windows.net"
        blob_service = BlobServiceClient(account_url, credential=credential)

        # Get a user delegation key (valid for the SAS lifetime).
        now = datetime.now(timezone.utc)
        expiry = now + timedelta(seconds=self._session_duration)

        delegation_key: UserDelegationKey = (
            blob_service.get_user_delegation_key(
                key_start_time=now,
                key_expiry_time=expiry,
            )
        )

        if "immutable-write" in permissions:
            write_token = _generate_directory_sas(
                account_name=self._storage_account,
                container=container,
                prefix=upload_prefix or "",
                delegation_key=delegation_key,
                permissions=["immutable-write"],
                expiry=expiry,
                start=now,
                generate_blob_sas=generate_blob_sas,
            )
            credentials = {
                "storage_account": self._storage_account,
                "write_sas_token": write_token,
                "write_prefix": upload_prefix,
            }
        else:
            # Directory-scoped SAS uses Blob SAS with sr=d/sdd under the hood.
            sas_token = generate_blob_sas(
                account_name=self._storage_account,
                container_name=container,
                blob_name=prefix,
                user_delegation_key=delegation_key,
                permission=_sas_permission_string(permissions),
                expiry=expiry,
                start=now,
                is_directory=True,
                sdd=str(_directory_depth(prefix)),
            )
            credentials = {
                "storage_account": self._storage_account,
                "sas_token": sas_token,
            }
        return CredentialResult(
            credentials=credentials,
            expires_at=expiry.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )

    def cleanup_staging(self, *, repo_url: str, older_than_seconds: int) -> int:
        """Delete abandoned staging blobs older than the configured TTL."""
        if older_than_seconds <= 0:
            return 0

        container, prefix = _parse_repo_url(repo_url)
        staging_prefix = f"{prefix}/staging/"
        cutoff = datetime.now(timezone.utc) - timedelta(seconds=older_than_seconds)

        from azure.identity import DefaultAzureCredential
        from azure.storage.blob import BlobServiceClient

        credential = DefaultAzureCredential()
        account_url = f"https://{self._storage_account}.blob.core.windows.net"
        blob_service = BlobServiceClient(account_url, credential=credential)
        container_client = blob_service.get_container_client(container)

        deleted = 0
        for blob in container_client.list_blobs(name_starts_with=staging_prefix):
            last_modified = getattr(blob, "last_modified", None)
            name = getattr(blob, "name", None)
            if not name or last_modified is None:
                continue
            if last_modified >= cutoff:
                continue
            container_client.delete_blob(name)
            deleted += 1
        return deleted


def _parse_repo_url(repo_url: str) -> tuple[str, str]:
    """Extract container and prefix from a crab:// URL.

    crab://my-container/team/repo → ("my-container", "team/repo")
    """
    parsed = parse_repo_url(repo_url, allowed_schemes={"crab", "az", "azure"})
    return parsed.bucket, parsed.prefix


def _sas_permission_string(permissions: list[str]) -> str:
    """Build a SAS permission string in Azure's required order."""
    permissions = normalize_permissions(permissions)
    requested: set[str] = set()
    if "read" in permissions:
        requested.update({"r", "l"})
    if "write" in permissions:
        requested.update({"a", "c", "w", "d"})
    if "immutable-write" in permissions:
        requested.update({"a", "c", "w"})
    return "".join(ch for ch in "racwdl" if ch in requested)


def _directory_depth(prefix: str) -> int:
    """Return Azure's signedDirectoryDepth for a directory SAS."""
    normalized = prefix.strip("/")
    if not normalized:
        return 0
    return len([part for part in normalized.split("/") if part])


def _generate_directory_sas(
    *,
    account_name: str,
    container: str,
    prefix: str,
    delegation_key,
    permissions: list[str],
    expiry: datetime,
    start: datetime,
    generate_blob_sas,
) -> str:
    return _generate_scoped_sas(
        account_name=account_name,
        container=container,
        prefix=prefix,
        delegation_key=delegation_key,
        permissions=permissions,
        expiry=expiry,
        start=start,
        generate_blob_sas=generate_blob_sas,
        is_directory=True,
    )


def _generate_scoped_sas(
    *,
    account_name: str,
    container: str,
    prefix: str,
    delegation_key,
    permissions: list[str],
    expiry: datetime,
    start: datetime,
    generate_blob_sas,
    is_directory: bool,
) -> str:
    if not prefix:
        raise ValueError("scoped Azure SAS requires a non-empty prefix")
    kwargs = dict(
        account_name=account_name,
        container_name=container,
        blob_name=prefix,
        user_delegation_key=delegation_key,
        permission=_sas_permission_string_for_scope(permissions, is_directory),
        expiry=expiry,
        start=start,
    )
    if is_directory:
        kwargs["is_directory"] = True
        kwargs["sdd"] = str(_directory_depth(prefix))
    return generate_blob_sas(**kwargs)


def _sas_permission_string_for_scope(
    permissions: list[str],
    is_directory: bool,
) -> str:
    normalized = normalize_permissions(permissions)
    if normalized == ["read"] and not is_directory:
        return "r"
    return _sas_permission_string(normalized)


def _normalize_upload_prefix(
    upload_prefix: str | None,
    *,
    repo_prefix: str,
) -> str | None:
    try:
        return normalize_upload_prefix(upload_prefix, repo_prefix=repo_prefix)
    except RepoUrlError as e:
        raise ValueError(str(e)) from e
