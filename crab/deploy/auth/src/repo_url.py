"""Strict repo URL parsing for auth and provider policy scoping."""

from __future__ import annotations

import re
from dataclasses import dataclass


SUPPORTED_SCHEMES = frozenset({"crab", "s3", "gs", "gcs", "az", "azure"})
_SAFE_BUCKET_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_SAFE_SEGMENT_RE = re.compile(r"^[A-Za-z0-9._@+=,-]+$")
_PUSH_ID_RE = re.compile(r"^[0-9a-f]{32}$")


class RepoUrlError(ValueError):
    """Raised when a repo URL cannot safely scope cloud credentials."""


@dataclass(frozen=True)
class ParsedRepoUrl:
    scheme: str
    bucket: str
    prefix: str


def parse_repo_url(
    repo_url: str,
    *,
    allowed_schemes: set[str] | frozenset[str] | None = None,
    require_prefix: bool = True,
) -> ParsedRepoUrl:
    raw = repo_url.strip()
    if not raw:
        raise RepoUrlError("repo_url is required")
    if "://" not in raw:
        raise RepoUrlError("repo_url must include a supported scheme")

    scheme, rest = raw.split("://", 1)
    scheme = scheme.lower()
    if scheme not in SUPPORTED_SCHEMES:
        raise RepoUrlError(f"unsupported repo_url scheme: {scheme}")
    if allowed_schemes is not None and scheme not in allowed_schemes:
        allowed = ", ".join(sorted(allowed_schemes))
        raise RepoUrlError(
            f"repo_url scheme {scheme} is not valid here; expected {allowed}"
        )

    bucket, separator, raw_prefix = rest.partition("/")
    bucket = _validate_bucket(bucket)
    if not separator:
        if require_prefix:
            raise RepoUrlError("repo_url must include a repo prefix")
        return ParsedRepoUrl(scheme=scheme, bucket=bucket, prefix="")

    prefix = normalize_repo_prefix(raw_prefix, require_prefix=require_prefix)
    return ParsedRepoUrl(scheme=scheme, bucket=bucket, prefix=prefix)


def normalize_repo_prefix(prefix: str, *, require_prefix: bool = True) -> str:
    normalized = prefix.strip().strip("/")
    if not normalized:
        if require_prefix:
            raise RepoUrlError("repo_url must include a repo prefix")
        return ""

    if len(normalized) > 1024:
        raise RepoUrlError("repo prefix is too long")

    for segment in normalized.split("/"):
        if segment in {"", ".", ".."}:
            raise RepoUrlError("repo prefix contains an unsafe path component")
        if not _SAFE_SEGMENT_RE.fullmatch(segment):
            raise RepoUrlError(
                "repo prefix contains unsupported characters; "
                "use letters, numbers, '/', '.', '_', '-', '@', '+', '=', or ','"
            )
    return normalized


def normalize_upload_prefix(upload_prefix: str | None, *, repo_prefix: str) -> str | None:
    if upload_prefix is None:
        return None

    normalized_repo = normalize_repo_prefix(repo_prefix)
    normalized_upload = normalize_repo_prefix(upload_prefix)
    expected = f"{normalized_repo}/staging/"
    if not normalized_upload.startswith(expected):
        raise RepoUrlError("upload_prefix must be under the repo staging prefix")

    push_id = normalized_upload.removeprefix(expected)
    if not push_id or "/" in push_id:
        raise RepoUrlError("upload_prefix must name exactly one push staging directory")
    validate_push_id(push_id)
    return normalized_upload


def validate_push_id(push_id: str) -> str:
    normalized = push_id.strip()
    if not _PUSH_ID_RE.fullmatch(normalized):
        raise RepoUrlError("push_id must be a 32-character lowercase hex token")
    return normalized


def _validate_bucket(bucket: str) -> str:
    normalized = bucket.strip()
    if not normalized:
        raise RepoUrlError("repo_url must include a bucket or container")
    if "/" in normalized or normalized in {".", ".."}:
        raise RepoUrlError("bucket or container name is invalid")
    if not _SAFE_BUCKET_RE.fullmatch(normalized):
        raise RepoUrlError("bucket or container name contains unsupported characters")
    return normalized
