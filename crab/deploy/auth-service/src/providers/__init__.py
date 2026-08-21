"""Cloud credential providers for the auth endpoint."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol


@dataclass
class CredentialResult:
    """Result from a credential provider."""

    credentials: dict[str, Any]
    expires_at: str  # ISO 8601


class CredentialProvider(Protocol):
    """Protocol for cloud credential providers."""

    async def generate(
        self,
        identity: str,
        repo_url: str,
        operation: str,
        permissions: list[str],
        upload_prefix: str | None = None,
    ) -> CredentialResult: ...


def normalize_permissions(permissions: list[str]) -> list[str]:
    """Normalize permission tokens before provider-specific policy generation."""
    return [permission.strip().lower() for permission in permissions if permission.strip()]


def validate_protected_push_permissions(
    operation: str,
    permissions: list[str],
    upload_prefix: str | None,
) -> None:
    """Reject direct mutable credentials for protected push."""
    permissions = normalize_permissions(permissions)
    if operation.strip().lower() != "push":
        return
    unknown = [
        permission
        for permission in permissions
        if permission not in {"read", "write", "immutable-write"}
    ]
    if unknown:
        raise ValueError("push credentials include unsupported permission")
    if "write" in permissions:
        raise ValueError("push credentials cannot include canonical write permission")
    if "read" in permissions:
        raise ValueError("push credentials cannot include read permission")
    if "immutable-write" not in permissions:
        raise ValueError("push credentials require immutable-write permission")
    if not upload_prefix:
        raise ValueError("push credentials require an upload_prefix")


_providers: dict[str, CredentialProvider] = {}


def get_provider(name: str) -> CredentialProvider:
    """Get or create a credential provider by name."""
    if name not in _providers:
        if name == "aws":
            from .aws import AwsProvider

            _providers[name] = AwsProvider()
        elif name == "s3":
            from .s3 import S3Provider

            _providers[name] = S3Provider()
        elif name == "gcp":
            from .gcp import GcpProvider

            _providers[name] = GcpProvider()
        elif name == "azure":
            from .azure import AzureProvider

            _providers[name] = AzureProvider()
        else:
            raise ValueError(f"Unknown provider: {name}")
    return _providers[name]
