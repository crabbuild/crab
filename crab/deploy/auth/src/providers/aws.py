"""AWS credential provider — STS AssumeRole with scoped inline policy.

Generates short-lived AWS credentials restricted to the specific S3
prefix (repository path) requested. Uses an inline session policy to
scope down the assumed role's permissions to only the requested repo.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
from datetime import datetime, timedelta, timezone

import boto3
import structlog

from src.providers import (
    CredentialResult,
    normalize_permissions,
    validate_protected_push_permissions,
)
from src.repo_url import RepoUrlError, normalize_upload_prefix, parse_repo_url

logger = structlog.get_logger()


class AwsProvider:
    """Generate scoped AWS STS credentials via AssumeRole."""

    def __init__(self) -> None:
        self._role_arn = os.environ.get("CRAB_AUTH_AWS_ROLE_ARN", "")
        self._region = os.environ.get("CRAB_AUTH_AWS_REGION", "us-east-1")
        self._session_duration = int(
            os.environ.get("CRAB_AUTH_SESSION_DURATION", "3600")
        )
        self._external_id = os.environ.get("CRAB_AUTH_AWS_EXTERNAL_ID", "").strip()
        self._dry_run = os.environ.get("CRAB_AUTH_DRY_RUN", "").lower() in (
            "1", "true", "yes",
        )
        if not self._dry_run:
            self._sts = boto3.client("sts", region_name=self._region)
            self._s3 = boto3.client("s3", region_name=self._region)

    async def generate(
        self,
        identity: str,
        repo_url: str,
        operation: str,
        permissions: list[str],
        upload_prefix: str | None = None,
    ) -> CredentialResult:
        """Generate scoped AWS credentials for the requested repo."""
        bucket, prefix = _parse_repo_url(repo_url)
        permissions = normalize_permissions(permissions)
        validate_protected_push_permissions(operation, permissions, upload_prefix)
        session_name = _session_name(identity)
        policy = _build_session_policy(bucket, prefix, permissions, upload_prefix)

        logger.debug(
            "assuming_role",
            role_arn=self._role_arn,
            session_name=session_name,
            bucket=bucket,
            prefix=prefix,
            dry_run=self._dry_run,
        )

        # Dry-run mode: return synthetic credentials for local testing.
        # Verifies the full auth + policy pipeline without real AWS access.
        if self._dry_run:
            expiry = datetime.now(timezone.utc) + timedelta(
                seconds=self._session_duration
            )
            return CredentialResult(
                credentials={
                    "access_key_id": f"ASIADRYRUN{session_name[-6:].upper()}",
                    "secret_access_key": "dry-run-secret-key-not-real",
                    "session_token": "dry-run-session-token-not-real",
                    "region": self._region,
                },
                expires_at=expiry.strftime("%Y-%m-%dT%H:%M:%SZ"),
            )

        response = await asyncio.to_thread(
            self._assume_role,
            session_name=session_name,
            policy=policy,
        )

        creds = response["Credentials"]
        expiration: datetime = creds["Expiration"]

        # Ensure timezone-aware UTC.
        if expiration.tzinfo is None:
            expiration = expiration.replace(tzinfo=timezone.utc)

        return CredentialResult(
            credentials={
                "access_key_id": creds["AccessKeyId"],
                "secret_access_key": creds["SecretAccessKey"],
                "session_token": creds["SessionToken"],
                "region": self._region,
            },
            expires_at=expiration.strftime("%Y-%m-%dT%H:%M:%SZ"),
        )

    def _assume_role(self, *, session_name: str, policy: dict) -> dict:
        request = {
            "RoleArn": self._role_arn,
            "RoleSessionName": session_name,
            "DurationSeconds": self._session_duration,
            "Policy": json.dumps(policy),
        }
        if self._external_id:
            request["ExternalId"] = self._external_id
        return self._sts.assume_role(**request)

    def cleanup_staging(self, *, repo_url: str, older_than_seconds: int) -> int:
        """Delete abandoned staging objects older than the configured TTL."""
        if self._dry_run or older_than_seconds <= 0:
            return 0

        bucket, prefix = _parse_repo_url(repo_url)
        staging_prefix = f"{_normalize_repo_prefix(prefix)}/staging/"
        cutoff = datetime.now(timezone.utc) - timedelta(seconds=older_than_seconds)
        deleted = 0
        continuation_token = None

        while True:
            request = {"Bucket": bucket, "Prefix": staging_prefix}
            if continuation_token:
                request["ContinuationToken"] = continuation_token
            response = self._s3.list_objects_v2(**request)
            batch = []
            for item in response.get("Contents", []):
                last_modified = item.get("LastModified")
                key = item.get("Key")
                if not key or last_modified is None:
                    continue
                if last_modified < cutoff:
                    batch.append({"Key": key})
            if batch:
                self._s3.delete_objects(
                    Bucket=bucket,
                    Delete={"Objects": batch, "Quiet": True},
                )
                deleted += len(batch)
            if not response.get("IsTruncated"):
                break
            continuation_token = response.get("NextContinuationToken")
            if not continuation_token:
                break
        return deleted


def _parse_repo_url(repo_url: str) -> tuple[str, str]:
    """Extract bucket and prefix from a crab:// URL.

    crab://my-bucket/team/repo → ("my-bucket", "team/repo")
    """
    parsed = parse_repo_url(repo_url, allowed_schemes={"crab", "s3"})
    return parsed.bucket, parsed.prefix


def _session_name(identity: str) -> str:
    """Derive a CloudTrail-friendly session name from the user's identity.

    Format: crab-{sha256(identity)[:12]}
    STS session names must be 2-64 chars, [a-zA-Z0-9+=,.@_-].
    """
    h = hashlib.sha256(identity.encode()).hexdigest()[:12]
    return f"crab-{h}"


def _build_session_policy(
    bucket: str,
    prefix: str,
    permissions: list[str],
    upload_prefix: str | None = None,
) -> dict:
    """Build an IAM inline session policy scoped to the repo prefix.

    This policy is intersected with the assumed role's permissions,
    so it can only restrict — never grant more than the role allows.
    """
    permissions = normalize_permissions(permissions)
    if "immutable-write" in permissions and "read" in permissions:
        raise ValueError("immutable-write credentials cannot include read permission")
    read_actions = []
    canonical_write_actions = []
    staging_write_actions = []
    needs_list_bucket = False

    if "read" in permissions:
        read_actions.extend([
            "s3:GetObject",
        ])
        needs_list_bucket = True

    if "write" in permissions:
        canonical_write_actions.extend([
            "s3:PutObject",
            "s3:DeleteObject",
            "s3:AbortMultipartUpload",
            "s3:ListMultipartUploadParts",
        ])

    if "immutable-write" in permissions:
        staging_write_actions.extend([
            "s3:PutObject",
            "s3:AbortMultipartUpload",
            "s3:ListMultipartUploadParts",
        ])

    # Normalize prefix — ensure no trailing slash for the resource ARN,
    # but add /* for object-level access.
    prefix = _normalize_repo_prefix(prefix)
    upload_prefix = _normalize_upload_prefix(upload_prefix, repo_prefix=prefix)
    if "immutable-write" in permissions and not upload_prefix:
        raise ValueError("immutable-write credentials require an upload_prefix")

    statements = []

    # Object-level actions. Protected repositories receive write access only
    # under the staging upload prefix; mutable refs and manifests stay service-owned.
    if read_actions or canonical_write_actions or staging_write_actions:
        if read_actions:
            statements.append({
                "Effect": "Allow",
                "Action": read_actions,
                "Resource": _repo_object_resource(bucket, prefix),
            })
        if canonical_write_actions:
            statements.append({
                "Effect": "Allow",
                "Action": canonical_write_actions,
                "Resource": _repo_object_resource(bucket, prefix),
            })
        if staging_write_actions:
            statements.append({
                "Effect": "Allow",
                "Action": staging_write_actions,
                "Resource": _object_resource(bucket, upload_prefix),
            })

    # ListBucket needs a bucket-level resource with a prefix condition.
    if needs_list_bucket:
        statement = {
            "Effect": "Allow",
            "Action": "s3:ListBucket",
            "Resource": f"arn:aws:s3:::{bucket}",
        }
        statement["Condition"] = {
            "StringLike": {
                "s3:prefix": [f"{prefix}/*", prefix],
            }
        }
        statements.append(statement)

    return {
        "Version": "2012-10-17",
        "Statement": statements,
    }


def _repo_object_resource(bucket: str, prefix: str) -> str:
    return f"arn:aws:s3:::{bucket}/{prefix}/*"


def _object_resource(bucket: str, prefix: str) -> str:
    return f"arn:aws:s3:::{bucket}/{prefix}/*"


def _normalize_repo_prefix(prefix: str) -> str:
    try:
        parsed = parse_repo_url(f"crab://bucket/{prefix}", allowed_schemes={"crab"})
        return parsed.prefix
    except RepoUrlError as e:
        raise ValueError(str(e)) from e


def _normalize_upload_prefix(
    upload_prefix: str | None,
    *,
    repo_prefix: str,
) -> str | None:
    try:
        return normalize_upload_prefix(upload_prefix, repo_prefix=repo_prefix)
    except RepoUrlError as e:
        raise ValueError(str(e)) from e
