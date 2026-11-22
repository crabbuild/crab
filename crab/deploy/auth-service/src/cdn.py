"""Issue viewer capabilities for operator-approved repository exports."""

from __future__ import annotations

import base64
import hmac
import json
import re
from datetime import datetime
from pathlib import Path
from typing import Literal
from urllib.parse import urlsplit

import jwt
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from src.cdn_approval import CdnApproval
from src.repo_url import normalize_repo_prefix

CAPABILITY_HEADER = "X-Crab-CDN-Grant-Version"
CAPABILITY_VERSION = "1"
APPROVAL_HEADER = "X-Crab-CDN-Approval-Sha256"
_APPROVAL_DIGEST = re.compile(r"[0-9a-f]{64}")
_READ_OPERATIONS = frozenset({"clone", "fetch", "hydrate", "pull", "mount", "smudge"})
_REPOSITORY_OBJECTS = (
    "v2/capsules",
    "v2/checkpoints",
    "v2/catalogs",
    "v2/pack-layers",
    "v2/history",
    "lfs/objects",
)
_VIEW_CONTENT = ("xorbs", "shards", "capsule-chunk-pages")
_VIEW_SUFFIX = re.compile(r"/acl-views/v2/[0-9a-f]{64}/(?:0|[1-9][0-9]{0,19})-[0-9a-f]{64}")
_PATH = re.compile(r"[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*")
_HOST_LABEL = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?")
Provider = Literal["cloudfront", "cloud_cdn", "front_door"]


class ApprovalConflict(ValueError):
    """The selected issuer approval differs from the caller's pinned approval."""


def requested_approvals(values: list[str]) -> frozenset[str]:
    if not values:
        return frozenset()
    if len(values) != 1 or len(values[0]) > 16 * 65 - 1:
        raise ValueError("invalid CDN approval list")
    digests = values[0].split(",")
    unique = frozenset(digests)
    if len(unique) != len(digests) or not all(_APPROVAL_DIGEST.fullmatch(digest) for digest in digests):
        raise ValueError("invalid CDN approval list")
    return unique


def canonical_prefix(prefix: str) -> str:
    if not _PATH.fullmatch(prefix) or normalize_repo_prefix(prefix) != prefix:
        raise ValueError("CDN export requires an unambiguous storage prefix")
    return prefix


class CdnExport(BaseModel):
    """An exact repository approved by the operator for CDN delivery."""

    model_config = ConfigDict(extra="forbid", frozen=True)

    provider: Provider
    origin: str
    repo_prefix: str
    base_url: str
    allow_unfiltered: bool = False
    protected_view_suffix: str = ""
    approval: CdnApproval | None = None
    key_id: str = Field(pattern=r"^[A-Za-z0-9_-]{1,63}$")
    key_file: Path = Field(repr=False)
    ttl_seconds: int = Field(default=300, gt=0, le=900)

    _prefix = field_validator("repo_prefix")(canonical_prefix)

    @field_validator("protected_view_suffix")
    @classmethod
    def validate_protected_view_suffix(cls, value: str) -> str:
        if value and not _VIEW_SUFFIX.fullmatch(value):
            raise ValueError("CDN export requires an exact protected v2 view suffix")
        return value

    @field_validator("base_url")
    @classmethod
    def validate_base_url(cls, value: str) -> str:
        url = urlsplit(value)
        path = url.path.lstrip("/")
        if (
            url.scheme != "https"
            or "." not in url.netloc
            or not all(_HOST_LABEL.fullmatch(label) for label in url.netloc.split("."))
            or (path and canonical_prefix(path) != path)
            or value != f"https://{url.netloc}{url.path}"
            or url.path.startswith("//")
        ):
            raise ValueError("CDN base URL must use a canonical HTTPS hostname and path")
        return value.rstrip("/")

    @model_validator(mode="after")
    def validate_export(self) -> CdnExport:
        if self.allow_unfiltered and self.protected_view_suffix:
            raise ValueError("CDN export cannot approve both source and protected view reads")
        patterns = {
            "cloudfront": r"s3://[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]",
            "cloud_cdn": r"gs://[a-z0-9][a-z0-9._-]{1,220}[a-z0-9]",
            "front_door": (
                r"https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/"
                r"[a-z0-9][a-z0-9-]{1,61}[a-z0-9]"
            ),
        }
        if not re.fullmatch(patterns[self.provider], self.origin):
            raise ValueError("CDN origin must match the provider's native cloud target")
        return self


class CdnPrefixGrant(BaseModel):
    """A viewer credential for one slash-delimited physical object prefix."""

    prefix: str
    credential: str = Field(repr=False)


class CdnGrant(BaseModel):
    """Bound, short-lived viewer authority returned alongside origin credentials."""

    provider: Provider
    origin: str
    base_url: str
    repo_prefix: str
    global_prefix: str | None
    expires_at: int
    grants: list[CdnPrefixGrant] = Field(repr=False)


def resolve_export(
    exports: tuple[CdnExport, ...],
    cloud_provider: str,
    bucket: str,
    source_prefix: str,
    credentials: dict[str, object],
) -> CdnExport | None:
    if cloud_provider == "aws":
        origin = f"s3://{bucket}"
    elif cloud_provider == "gcp":
        origin = f"gs://{bucket}"
    elif cloud_provider == "azure":
        account = credentials.get("storage_account")
        if not isinstance(account, str):
            return None
        origin = f"https://{account}.blob.core.windows.net/{bucket}"
    else:
        # The reference "s3" provider can issue for custom S3-compatible endpoints.
        return None
    return next(
        (entry for entry in exports if entry.origin == origin and entry.repo_prefix == source_prefix),
        None,
    )


def issue_viewer_grant(
    export: CdnExport,
    *,
    operation: str,
    permissions: list[str],
    parent_expiry: str,
    view_repo_prefix: str | None,
    view_global_prefix: str | None,
    credentials: dict[str, object],
    approvals: frozenset[str],
    now: datetime,
) -> CdnGrant | None:
    """Mint only for an authorized physical export and read-only operation."""
    operation = operation.strip().lower()
    if not approvals or export.approval is None or operation not in _READ_OPERATIONS or set(permissions) != {"read"}:
        return None
    if view_repo_prefix is None:
        if view_global_prefix is not None or not export.allow_unfiltered:
            return None
        repo = export.repo_prefix
        global_prefix = None
    else:
        repo = canonical_prefix(view_repo_prefix)
        if not repo.startswith(f"{export.repo_prefix}/acl-views/"):
            raise ValueError("CDN view is outside its approved repository export")
        if view_global_prefix != f"{repo}/.crab":
            raise ValueError("CDN view global prefix does not match its repository")
        # A newer generation still has its own authorized origin credentials.
        if (
            not export.protected_view_suffix
            or repo != export.repo_prefix + export.protected_view_suffix
        ):
            return None
        global_prefix = view_global_prefix

    parent = datetime.fromisoformat(parent_expiry)
    if parent.tzinfo is None or now.tzinfo is None:
        raise ValueError("CDN grant expiration requires a timezone")
    now_epoch = int(now.timestamp())
    expires = min(int(parent.timestamp()), now_epoch + export.ttl_seconds)
    if expires <= now_epoch:
        return None

    if export.approval.sha256 not in approvals:
        raise ApprovalConflict("CDN deployment approval differs from the client pin")
    export.approval.verify(
        provider=export.provider,
        origin=export.origin,
        repo_prefix=export.repo_prefix,
        view_suffix=export.protected_view_suffix,
        base_url=export.base_url,
        credentials=credentials,
    )
    prefixes = [f"{repo}/{kind}/" for kind in _REPOSITORY_OBJECTS]
    if global_prefix is not None:
        prefixes.extend(f"{global_prefix}/{kind}/" for kind in _VIEW_CONTENT)

    return CdnGrant(
        provider=export.provider,
        origin=export.origin,
        base_url=export.base_url,
        repo_prefix=repo,
        global_prefix=global_prefix,
        expires_at=expires,
        grants=sign_viewer_prefixes(export, repo, prefixes, now_epoch, expires),
    )


def sign_viewer_prefixes(
    export: CdnExport, repo: str, prefixes: list[str], now_epoch: int, expires: int,
) -> list[CdnPrefixGrant]:
    """Sign prefixes already validated by the issuer or operator's probe."""
    # Mounted secret files keep signing material out of policy and out of
    # responses. Read once per issuance so rotating a mount is atomic per grant.
    with export.key_file.open("rb") as source:
        key = source.read(8193)
    if len(key) > 8192:
        raise ValueError("CDN signing key is too large")
    if export.provider == "cloud_cdn":
        signing_key = base64.b64decode(key.strip(), altchars=b"-_", validate=True)
        if len(signing_key) != 16:
            raise ValueError("Cloud CDN signing key must decode to 16 bytes")

        def sign(prefix: str) -> str:
            url_prefix = _b64url(f"{export.base_url}/{prefix}".encode())
            policy = f"URLPrefix={url_prefix}:Expires={expires}:KeyName={export.key_id}"
            signature = _b64url(hmac.digest(signing_key, policy.encode(), "sha1"))
            return f"Cloud-CDN-Cookie={policy}:Signature={signature}"
    else:
        private_key = serialization.load_pem_private_key(key, password=None)
        if not isinstance(private_key, rsa.RSAPrivateKey) or private_key.key_size != 2048:
            raise ValueError("CDN signing key must be RSA 2048 PEM")
        if export.provider == "cloudfront":
            def sign(prefix: str) -> str:
                return _cloudfront_cookie(export, prefix, expires, private_key)
        else:
            def sign(prefix: str) -> str:
                return jwt.encode(
                    {
                        "iss": export.base_url,
                        "aud": export.base_url,
                        "sub": repo,
                        "iat": now_epoch,
                        "exp": expires,
                        "prefix": prefix,
                        "origin": export.origin,
                    },
                    private_key,
                    algorithm="RS256",
                    headers={"kid": export.key_id},
                )

    return [CdnPrefixGrant(prefix=prefix, credential=sign(prefix)) for prefix in prefixes]


def _b64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).decode("ascii")


def _cloudfront_cookie(
    export: CdnExport, prefix: str, expires: int, key: rsa.RSAPrivateKey
) -> str:
    policy = json.dumps(
        {"Statement": [{
            "Resource": f"{export.base_url}/{prefix}*",
            "Condition": {"DateLessThan": {"AWS:EpochTime": expires}},
        }]},
        separators=(",", ":"),
    ).encode()
    signature = key.sign(policy, padding.PKCS1v15(), hashes.SHA256())
    alphabet = str.maketrans("+=/", "-_~")

    def encode(value: bytes) -> str:
        return base64.b64encode(value).decode("ascii").translate(alphabet)

    return (
        f"CloudFront-Policy={encode(policy)}; CloudFront-Signature={encode(signature)}; "
        f"CloudFront-Key-Pair-Id={export.key_id}; CloudFront-Hash-Algorithm=SHA256"
    )
