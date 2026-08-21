"""FastAPI application for the crab-auth endpoint."""

from __future__ import annotations

import asyncio
import hashlib
import ipaddress
import json
import math
import os
import secrets
import time
from pathlib import Path
from typing import Annotated, Any

import structlog
from fastapi import FastAPI, HTTPException, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from pydantic import BaseModel, ConfigDict, Field

from src.auth import JWTVerifier, TokenClaims
from src.policy import PolicyEngine, PolicyDecision
from src.providers import get_provider
from src.receive_helper import (
    ReceiveCommitResult,
    ReceiveConflictError,
    ReceiveHelper,
    ReceiveInvalidBundleError,
    ReceivePrepareResult,
    ReceiveRuntimeStatus,
    SubprocessReceiveHelper,
)
from src.ref_store import InvalidRefError, validate_ref_update
from src.repo_url import ParsedRepoUrl, RepoUrlError, parse_repo_url, validate_push_id
from src.view_helper import (
    SubprocessViewHelper,
    ViewHelper,
    ViewMaterializationResult,
    ViewRuntimeStatus,
)

logger = structlog.get_logger()

MAX_ID_TOKEN_LEN = 20000
MAX_REPO_URL_LEN = 2048
MAX_OPERATION_LEN = 64
MAX_CLIENT_VERSION_LEN = 128
MAX_REF_NAME_LEN = 512
MAX_REF_UPDATES = 32
MAX_CHANGED_PATHS = 10000
MAX_CHANGED_PATH_LEN = 4096
DEFAULT_STAGING_TTL_SECONDS = 86400
DEFAULT_RATE_LIMIT_MAX_KEYS = 10000
RATE_LIMIT_IDLE_TTL_SECONDS = 3600
RATE_LIMIT_PRUNE_INTERVAL_SECONDS = 60
REQUIRED_AUTH_ENV = (
    "CRAB_AUTH_JWKS_URL",
    "CRAB_AUTH_ISSUER",
    "CRAB_AUTH_AUDIENCE",
)

IdTokenField = Annotated[str, Field(min_length=1, max_length=MAX_ID_TOKEN_LEN)]
RepoUrlField = Annotated[str, Field(min_length=1, max_length=MAX_REPO_URL_LEN)]
OperationField = Annotated[str, Field(min_length=1, max_length=MAX_OPERATION_LEN)]
ClientVersionField = Annotated[str, Field(max_length=MAX_CLIENT_VERSION_LEN)]
RefNameField = Annotated[str, Field(min_length=1, max_length=MAX_REF_NAME_LEN)]
OidField = Annotated[str, Field(max_length=40)]
NewOidField = Annotated[str, Field(min_length=1, max_length=40)]
PushIdField = Annotated[str, Field(min_length=1, max_length=128)]

app = FastAPI(
    title="Crab Auth",
    version="0.1.0",
    docs_url=None,
    redoc_url=None,
)


@app.exception_handler(RequestValidationError)
async def validation_exception_handler(
    request: Request,
    exc: RequestValidationError,
) -> JSONResponse:
    logger.warning(
        "invalid_request_body",
        path=request.url.path,
        errors=len(exc.errors()),
    )
    return JSONResponse(
        status_code=400,
        content={
            "detail": {
                "error": "invalid_request",
                "message": "Request body failed validation",
            },
        },
    )

# ---------------------------------------------------------------------------
# Configuration (loaded once at startup)
# ---------------------------------------------------------------------------

_verifier: JWTVerifier | None = None
_policy: PolicyEngine | None = None
_rate_limiter: RateLimiter | None = None
_receive_helper: ReceiveHelper | None = None
_view_helper: ViewHelper | None = None


def get_verifier() -> JWTVerifier:
    global _verifier
    if _verifier is None:
        _verifier = JWTVerifier(
            jwks_url=_required_env("CRAB_AUTH_JWKS_URL"),
            issuer=_required_env("CRAB_AUTH_ISSUER"),
            audience=_required_env("CRAB_AUTH_AUDIENCE"),
        )
    return _verifier


def _required_env(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"missing required environment variable: {name}")
    return value


def _policy_path() -> str:
    return os.environ.get("CRAB_AUTH_POLICY_PATH", "/etc/crab-auth/policy.yaml")


def get_policy() -> PolicyEngine:
    global _policy
    if _policy is None:
        _policy = PolicyEngine.from_file(_policy_path())
    return _policy


def get_rate_limiter() -> RateLimiter:
    global _rate_limiter
    if _rate_limiter is None:
        _rate_limiter = RateLimiter(
            rate_per_minute=int(
                os.environ.get("CRAB_AUTH_RATE_LIMIT_PER_MINUTE", "120")
            ),
            burst=int(os.environ.get("CRAB_AUTH_RATE_LIMIT_BURST", "30")),
            max_keys=int(
                os.environ.get(
                    "CRAB_AUTH_RATE_LIMIT_MAX_KEYS",
                    str(DEFAULT_RATE_LIMIT_MAX_KEYS),
                )
            ),
        )
    return _rate_limiter


def get_receive_helper() -> ReceiveHelper:
    global _receive_helper
    if _receive_helper is None:
        _receive_helper = SubprocessReceiveHelper()
    return _receive_helper


def get_view_helper() -> ViewHelper:
    global _view_helper
    if _view_helper is None:
        _view_helper = SubprocessViewHelper()
    return _view_helper


def active_active_client_config_allowed() -> bool:
    return os.environ.get(
        "CRAB_AUTH_ACTIVE_ACTIVE_ALLOW_CLIENT_CONFIG", ""
    ).strip().lower() in {"1", "true", "yes"}


def approved_active_active_payload(
    active_active: PushFinalizeActiveActive | None,
    log: Any,
) -> dict[str, Any] | None:
    if active_active is None:
        return None

    payload = active_active.model_dump()
    expected_json = os.environ.get("CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON")
    if expected_json:
        try:
            expected = json.loads(expected_json)
        except json.JSONDecodeError as e:
            log.error(
                "active_active_config_invalid",
                error=str(e),
            )
            raise HTTPException(
                status_code=500,
                detail={
                    "error": "internal",
                    "message": "Active-active CrabAuth configuration is invalid",
                },
            ) from e
        if _canonical_json(payload) != _canonical_json(expected):
            log.warning("active_active_config_denied")
            raise HTTPException(
                status_code=403,
                detail={
                    "error": "active_active_config_denied",
                    "message": "Active-active coordinator config is not approved by this CrabAuth service",
                },
            )
        return payload

    if active_active_client_config_allowed():
        return payload

    log.warning("active_active_config_required")
    raise HTTPException(
        status_code=403,
        detail={
            "error": "active_active_config_required",
            "message": "Active-active CrabAuth requires CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON on the service",
        },
    )


def _canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


class ReadinessFailure(Exception):
    """Internal readiness failure with a sanitized component label."""

    def __init__(self, component: str, message: str) -> None:
        super().__init__(message)
        self.component = component


class RateLimiter:
    """Small per-instance token bucket keyed by client address."""

    def __init__(
        self,
        rate_per_minute: int,
        burst: int,
        max_keys: int = DEFAULT_RATE_LIMIT_MAX_KEYS,
    ) -> None:
        self._rate_per_second = max(rate_per_minute, 0) / 60.0
        self._burst = max(burst, 1)
        self._max_keys = max(max_keys, 1)
        self._buckets: dict[str, tuple[float, float]] = {}
        self._lock = asyncio.Lock()
        self._last_prune_at = 0.0

    async def allow(self, key: str) -> tuple[bool, int]:
        if self._rate_per_second <= 0:
            return True, 0

        now = time.monotonic()
        async with self._lock:
            self._prune(now)
            tokens, updated_at = self._buckets.get(key, (float(self._burst), now))
            elapsed = max(0.0, now - updated_at)
            tokens = min(float(self._burst), tokens + elapsed * self._rate_per_second)

            if tokens >= 1.0:
                self._ensure_capacity_for(key)
                self._buckets[key] = (tokens - 1.0, now)
                return True, 0

            retry_after = math.ceil((1.0 - tokens) / self._rate_per_second)
            self._ensure_capacity_for(key)
            self._buckets[key] = (tokens, now)
            return False, max(1, retry_after)

    def _prune(self, now: float) -> None:
        if now - self._last_prune_at < RATE_LIMIT_PRUNE_INTERVAL_SECONDS:
            return
        self._last_prune_at = now
        cutoff = now - RATE_LIMIT_IDLE_TTL_SECONDS
        self._buckets = {
            key: bucket
            for key, bucket in self._buckets.items()
            if bucket[1] >= cutoff
        }

    def _ensure_capacity_for(self, key: str) -> None:
        if key in self._buckets or len(self._buckets) < self._max_keys:
            return
        oldest_key = min(self._buckets, key=lambda item: self._buckets[item][1])
        del self._buckets[oldest_key]


def client_rate_limit_key(request: Request) -> str:
    trust_proxy = os.environ.get("CRAB_AUTH_TRUST_PROXY_HEADERS", "").lower() in {
        "1",
        "true",
        "yes",
        "on",
    }
    if trust_proxy:
        forwarded_for = request.headers.get("x-forwarded-for")
        if forwarded_for:
            forwarded_key = _normalized_client_ip(forwarded_for.split(",", 1)[0])
            if forwarded_key:
                return forwarded_key
        real_ip = request.headers.get("x-real-ip")
        if real_ip:
            real_key = _normalized_client_ip(real_ip)
            if real_key:
                return real_key
    if request.client:
        client_key = _normalized_client_ip(request.client.host)
        if client_key:
            return client_key
    return "unknown"


def _normalized_client_ip(value: str) -> str | None:
    try:
        return str(ipaddress.ip_address(value.strip()))
    except ValueError:
        return None


# ---------------------------------------------------------------------------
# Request / Response models
# ---------------------------------------------------------------------------


class RequestModel(BaseModel):
    model_config = ConfigDict(extra="forbid")


def _validate_changed_path_list(paths: list[str]) -> list[str]:
    if len(paths) > MAX_CHANGED_PATHS:
        raise ValueError("too many changed paths")
    seen: set[str] = set()
    for path in paths:
        _validate_changed_path_shape(path)
        if path in seen:
            raise ValueError(f"duplicate changed path: {path}")
        seen.add(path)
    return paths


def _validate_changed_path_shape(path: str) -> None:
    if len(path) > MAX_CHANGED_PATH_LEN:
        raise ValueError("changed path is too long")
    if (
        path != path.strip()
        or path.startswith("/")
        or path.endswith("/")
        or "//" in path
        or any(ord(ch) < 32 or ord(ch) == 127 for ch in path)
    ):
        raise ValueError(f"unsafe changed path: {path}")
    segments = path.split("/")
    if any(segment in {"", ".", ".."} for segment in segments):
        raise ValueError(f"unsafe changed path: {path}")


class AuthRequest(RequestModel):
    id_token: IdTokenField
    repo_url: RepoUrlField
    operation: OperationField
    client_version: ClientVersionField = ""


class StorageScope(BaseModel):
    repo_prefix: str
    global_prefix: str
    source_repo: str
    scope_hash: str


class AuthResponse(BaseModel):
    provider: str
    credentials: dict[str, Any]
    expires_at: str
    permissions: list[str]
    storage_scope: StorageScope | None = None


class PushRefUpdate(RequestModel):
    ref_name: RefNameField
    old_oid: OidField | None = None
    new_oid: NewOidField


class PushPrepareRequest(RequestModel):
    id_token: IdTokenField
    repo_url: RepoUrlField
    ref_updates: list[PushRefUpdate] = Field(
        min_length=1,
        max_length=MAX_REF_UPDATES,
    )
    client_version: ClientVersionField = ""


class PushPrepareResponse(BaseModel):
    provider: str
    credentials: dict[str, Any]
    expires_at: str
    permissions: list[str]
    push_id: str
    upload_prefix: str


class PushFinalizeActiveActive(RequestModel):
    replication: dict[str, Any]
    writer: str = Field(min_length=1, max_length=128)


class PushFinalizeRequest(RequestModel):
    id_token: IdTokenField
    repo_url: RepoUrlField
    ref_updates: list[PushRefUpdate] = Field(
        min_length=1,
        max_length=MAX_REF_UPDATES,
    )
    push_id: PushIdField
    client_version: ClientVersionField = ""
    active_active: PushFinalizeActiveActive | None = None


class PushFinalizeResponse(BaseModel):
    status: str
    ref_updates: list[PushRefUpdate]
    operation_id: str | None = None
    coordinator_epoch: int | None = None
    writer_region: str | None = None
    manifest_generation: int | None = None
    commit_state: str | None = None


async def _materialize_filtered_read_scope(
    *,
    repo_url: str,
    claims: TokenClaims,
    decision: PolicyDecision,
    log: Any,
) -> tuple[str, StorageScope | None, ViewMaterializationResult | None]:
    credential_repo_url = repo_url
    storage_scope: StorageScope | None = None
    view: ViewMaterializationResult | None = None
    if not _requires_filtered_read_view(decision):
        return credential_repo_url, storage_scope, view

    parsed = _parse_repo_url_or_400(repo_url, log)
    scope_hash = _read_scope_hash(
        source_repo=parsed.prefix,
        read_paths=decision.read_paths,
        denied_read_paths=decision.denied_read_paths,
    )
    read_paths = decision.read_paths if decision.read_paths is not None else ["*"]
    try:
        view = await asyncio.to_thread(
            get_view_helper().materialize,
            repo_url=repo_url,
            provider=decision.provider,
            scope_hash=scope_hash,
            read_paths=read_paths,
            denied_read_paths=decision.denied_read_paths,
        )
    except Exception as e:
        log.error(
            "view_materialization_failed",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=decision.provider,
            scope_hash=scope_hash,
            read_path_count=len(read_paths),
            read_path_hash=_hash_values(read_paths),
            denied_path_count=len(decision.denied_read_paths),
            denied_path_hash=_hash_values(decision.denied_read_paths),
            error=str(e),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to materialize ACL-filtered repository view",
            },
        )
    expected_global_prefix = f"{view.repo_prefix}/.crab"
    try:
        parse_repo_url(f"crab://{parsed.bucket}/{view.repo_prefix}")
    except RepoUrlError as e:
        log.error(
            "view_materialization_invalid_prefix",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=decision.provider,
            scope_hash=scope_hash,
            error=str(e),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "ACL-filtered repository view returned an invalid prefix",
            },
        ) from e
    if (
        view.scope_hash != scope_hash
        or view.source_repo != parsed.prefix
        or view.global_prefix != expected_global_prefix
    ):
        log.error(
            "view_materialization_scope_mismatch",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=decision.provider,
            expected_scope_hash=scope_hash,
            returned_scope_hash=view.scope_hash,
            source_repo=parsed.prefix,
            returned_source_repo=view.source_repo,
            expected_global_prefix=expected_global_prefix,
            returned_global_prefix=view.global_prefix,
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "ACL-filtered repository view scope mismatch",
            },
        )
    credential_repo_url = f"crab://{parsed.bucket}/{view.repo_prefix}"
    storage_scope = StorageScope(
        repo_prefix=view.repo_prefix,
        global_prefix=view.global_prefix,
        source_repo=view.source_repo,
        scope_hash=view.scope_hash,
    )
    log.info(
        "view_materialized",
        identity=claims.identity,
        groups_hash=_hash_values(claims.groups),
        provider=decision.provider,
        scope_hash=view.scope_hash,
        source_generation=view.source_generation,
        source_manifest_hash=view.source_manifest_hash,
        view_cache_hit=view.cache_hit,
        read_path_count=len(read_paths),
        read_path_hash=_hash_values(read_paths),
        denied_path_count=len(decision.denied_read_paths),
        denied_path_hash=_hash_values(decision.denied_read_paths),
    )
    return credential_repo_url, storage_scope, view


class ErrorResponse(BaseModel):
    error: str
    message: str


# ---------------------------------------------------------------------------
# Health check
# ---------------------------------------------------------------------------


@app.get("/health")
async def health() -> dict[str, str]:
    return {"status": "ok"}


@app.get("/ready", response_model=None)
async def ready() -> dict[str, Any] | JSONResponse:
    try:
        _check_required_auth_config()
        policy = _check_policy_ready()
        _check_provider_config(policy)
        jwks_status = await _check_jwks_ready()
        receive_status = await _check_receive_helper_ready()
        view_status = await _check_view_helper_ready()
    except ReadinessFailure as e:
        logger.warning(
            "readiness_check_failed",
            component=e.component,
            error=str(e),
        )
        return _readiness_failure_response(e.component)
    except Exception as e:
        logger.error("readiness_check_failed", component="unknown", error=str(e))
        return _readiness_failure_response("unknown")

    return {
        "status": "ok",
        "auth_config": "ok",
        "policy": "ok",
        "provider_config": "ok",
        "jwks": "ok",
        "jwks_key_count": jwks_status["key_count"],
        "receive_helper": receive_status.status,
        "view_helper": view_status.status,
        "receive_git_version": receive_status.git_version,
        "view_git_version": view_status.git_version,
    }


def _readiness_failure_response(component: str) -> JSONResponse:
    return JSONResponse(
        status_code=503,
        content={
            "status": "not_ready",
            "component": component,
            "message": "Crab Auth dependencies are unavailable",
        },
    )


def _check_required_auth_config() -> None:
    missing = [
        name
        for name in REQUIRED_AUTH_ENV
        if not os.environ.get(name, "").strip()
    ]
    if missing:
        raise ReadinessFailure(
            "auth_config",
            f"missing required environment variables: {', '.join(missing)}",
        )


def _check_policy_ready() -> PolicyEngine:
    if _policy is None and not Path(_policy_path()).is_file():
        raise ReadinessFailure("policy", "policy file is not readable")
    try:
        return get_policy()
    except Exception as e:
        raise ReadinessFailure("policy", str(e)) from e


def _check_provider_config(policy: PolicyEngine) -> None:
    dry_run = os.environ.get("CRAB_AUTH_DRY_RUN", "").lower() in {
        "1",
        "true",
        "yes",
    }
    for provider in sorted(policy.providers()):
        if provider == "aws" and not dry_run:
            _require_provider_env(provider, "CRAB_AUTH_AWS_ROLE_ARN")
        elif provider == "s3":
            if not (
                os.environ.get("CRAB_AUTH_S3_ACCESS_KEY_ID")
                or os.environ.get("AWS_ACCESS_KEY_ID")
            ):
                _raise_provider_env(provider, "CRAB_AUTH_S3_ACCESS_KEY_ID")
            if not (
                os.environ.get("CRAB_AUTH_S3_SECRET_ACCESS_KEY")
                or os.environ.get("AWS_SECRET_ACCESS_KEY")
            ):
                _raise_provider_env(provider, "CRAB_AUTH_S3_SECRET_ACCESS_KEY")
        elif provider == "azure":
            _require_provider_env(provider, "CRAB_AUTH_AZURE_STORAGE_ACCOUNT")


async def _check_jwks_ready() -> dict[str, int]:
    try:
        return await get_verifier().check_runtime()
    except Exception as e:
        raise ReadinessFailure("jwks", str(e)) from e


async def _check_receive_helper_ready() -> ReceiveRuntimeStatus:
    try:
        return await asyncio.to_thread(get_receive_helper().check_runtime)
    except Exception as e:
        raise ReadinessFailure("receive_helper", str(e)) from e


async def _check_view_helper_ready() -> ViewRuntimeStatus:
    try:
        return await asyncio.to_thread(get_view_helper().check_runtime)
    except Exception as e:
        raise ReadinessFailure("view_helper", str(e)) from e


def _require_provider_env(provider: str, name: str) -> None:
    if not os.environ.get(name, "").strip():
        _raise_provider_env(provider, name)


def _raise_provider_env(provider: str, name: str) -> None:
    raise ReadinessFailure(
        "provider_config",
        f"{provider} provider is missing required environment variable: {name}",
    )


# ---------------------------------------------------------------------------
# Credential auth endpoint
# ---------------------------------------------------------------------------


@app.post(
    "/v1/credentials",
    response_model=AuthResponse,
    response_model_exclude_none=True,
    responses={
        401: {"model": ErrorResponse},
        403: {"model": ErrorResponse},
        400: {"model": ErrorResponse},
        429: {"model": ErrorResponse},
    },
)
async def issue_credentials(body: AuthRequest, request: Request) -> AuthResponse:
    start = time.time()
    log = logger.bind(
        repo_url=body.repo_url,
        operation=body.operation,
        client_version=body.client_version,
    )

    if body.operation.strip().lower() == "push":
        log.warning("push_credentials_rejected")
        raise HTTPException(
            status_code=400,
            detail={
                "error": "push_requires_protected_flow",
                "message": "Push must use /v1/push/prepare and /v1/push/finalize",
            },
        )

    claims, decision = await _authorize_request(
        id_token=body.id_token,
        repo_url=body.repo_url,
        operation=body.operation,
        request=request,
        log=log,
    )
    if "immutable-write" in decision.permissions:
        log.warning(
            "protected_repo_credentials_write_rejected",
            identity=claims.identity,
            permissions=decision.permissions,
        )
        raise HTTPException(
            status_code=403,
            detail={
                "error": "protected_repo_requires_service_flow",
                "message": (
                    "Protected repository writes must use a service-owned "
                    "operation-specific flow"
                ),
            },
        )

    credential_repo_url, storage_scope, view = await _materialize_filtered_read_scope(
        repo_url=body.repo_url,
        claims=claims,
        decision=decision,
        log=log,
    )

    provider = get_provider(decision.provider)
    try:
        result = await provider.generate(
            identity=claims.identity,
            repo_url=credential_repo_url,
            operation=body.operation,
            permissions=decision.permissions,
        )
    except Exception as e:
        log.error("credential_generation_failed", error=str(e))
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to generate credentials",
            },
        )

    elapsed_ms = (time.time() - start) * 1000
    log.info(
        "credentials_issued",
        identity=claims.identity,
        groups_hash=_hash_values(claims.groups),
        provider=decision.provider,
        permissions=decision.permissions,
        scope_hash=storage_scope.scope_hash if storage_scope else None,
        source_generation=view.source_generation if view else None,
        view_cache_hit=view.cache_hit if view else None,
        elapsed_ms=round(elapsed_ms, 1),
    )

    return AuthResponse(
        provider=decision.provider,
        credentials=result.credentials,
        expires_at=result.expires_at,
        permissions=decision.permissions,
        storage_scope=storage_scope,
    )


@app.post(
    "/v1/push/prepare",
    response_model=PushPrepareResponse,
    responses={
        401: {"model": ErrorResponse},
        403: {"model": ErrorResponse},
        400: {"model": ErrorResponse},
        429: {"model": ErrorResponse},
    },
)
async def prepare_push(
    body: PushPrepareRequest, request: Request
) -> PushPrepareResponse:
    start = time.time()
    log = logger.bind(
        repo_url=body.repo_url,
        ref_count=len(body.ref_updates),
        client_version=body.client_version,
    )

    _parse_repo_url_or_400(body.repo_url, log)
    claims = await _verify_identity(
        id_token=body.id_token,
        request=request,
        log=log,
    )
    try:
        _validate_ref_updates(body.ref_updates)
    except InvalidRefError as e:
        log.warning("push_prepare_invalid_ref", error=str(e))
        raise HTTPException(
            status_code=400,
            detail={"error": "invalid_ref", "message": str(e)},
        )
    log = log.bind(refs=_ref_names(body.ref_updates))

    provider_decision = _resolve_provider_or_403(
        claims=claims,
        repo_url=body.repo_url,
        operation="push",
        log=log,
    )
    read_view_scope: dict[str, str] | None = None
    read_view: ViewMaterializationResult | None = None
    read_decision = get_policy().evaluate(
        identity=claims.identity,
        groups=claims.groups,
        repo_url=body.repo_url,
        operation="fetch",
    )
    if read_decision.allowed and _requires_filtered_read_view(read_decision):
        if read_decision.provider != provider_decision.provider:
            log.warning(
                "push_prepare_provider_mismatch",
                identity=claims.identity,
                groups_hash=_hash_values(claims.groups),
                read_provider=read_decision.provider,
                push_provider=provider_decision.provider,
            )
            raise HTTPException(
                status_code=403,
                detail={
                    "error": "forbidden",
                    "message": "Ambiguous provider policy for protected push request",
                },
            )
        _, storage_scope, read_view = await _materialize_filtered_read_scope(
            repo_url=body.repo_url,
            claims=claims,
            decision=read_decision,
            log=log,
        )
        if storage_scope is not None:
            read_view_scope = storage_scope.model_dump()

    permissions = _push_upload_permissions(provider_decision.permissions)
    push_id = secrets.token_hex(16)
    upload_prefix = _upload_prefix(body.repo_url, push_id)
    try:
        prepare_result: ReceivePrepareResult = await asyncio.to_thread(
            get_receive_helper().prepare,
            repo_url=body.repo_url,
            push_id=push_id,
            provider=provider_decision.provider,
            ref_updates=_ref_updates_payload(body.ref_updates),
            view_scope=read_view_scope,
        )
    except ReceiveInvalidBundleError as e:
        log.warning(
            "push_prepare_invalid_bundle",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=provider_decision.provider,
            push_id=push_id,
            error=str(e),
        )
        raise HTTPException(
            status_code=400,
            detail={"error": "invalid_bundle", "message": str(e)},
        )
    except ReceiveConflictError as e:
        log.warning(
            "push_prepare_conflict",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=provider_decision.provider,
            push_id=push_id,
            error=str(e),
        )
        raise HTTPException(
            status_code=409,
            detail={"error": "manifest_conflict", "message": str(e)},
        )
    except Exception as e:
        log.error(
            "push_prepare_helper_failed",
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            provider=provider_decision.provider,
            push_id=push_id,
            error=str(e),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to prepare protected push",
            },
        )

    provider = get_provider(provider_decision.provider)
    try:
        result = await provider.generate(
            identity=claims.identity,
            repo_url=body.repo_url,
            operation="push",
            permissions=permissions,
            upload_prefix=upload_prefix,
        )
    except Exception as e:
        log.error("credential_generation_failed", error=str(e))
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to generate credentials",
            },
        )

    await _cleanup_staging_best_effort(
        provider,
        body.repo_url,
        log.bind(provider=provider_decision.provider, push_id=push_id),
    )

    elapsed_ms = (time.time() - start) * 1000
    log.info(
        "push_prepare_issued",
        identity=claims.identity,
        groups_hash=_hash_values(claims.groups),
        provider=provider_decision.provider,
        permissions=permissions,
        protected_repo=provider_decision.protected_repo,
        ref_count=len(body.ref_updates),
        push_id=push_id,
        source_generation=prepare_result.source_generation,
        scope_hash=read_view_scope["scope_hash"] if read_view_scope else None,
        view_cache_hit=read_view.cache_hit if read_view else None,
        elapsed_ms=round(elapsed_ms, 1),
    )

    return PushPrepareResponse(
        provider=provider_decision.provider,
        credentials=result.credentials,
        expires_at=result.expires_at,
        permissions=permissions,
        push_id=push_id,
        upload_prefix=upload_prefix,
    )


@app.post(
    "/v1/push/finalize",
    response_model=PushFinalizeResponse,
    response_model_exclude_none=True,
    responses={
        401: {"model": ErrorResponse},
        403: {"model": ErrorResponse},
        400: {"model": ErrorResponse},
        409: {"model": ErrorResponse},
        429: {"model": ErrorResponse},
    },
)
async def finalize_push(
    body: PushFinalizeRequest, request: Request
) -> PushFinalizeResponse:
    log = logger.bind(
        repo_url=body.repo_url,
        ref_count=len(body.ref_updates),
        push_id=body.push_id,
        client_version=body.client_version,
    )
    _parse_repo_url_or_400(body.repo_url, log)
    _validate_push_id_or_400(body.push_id, log)

    claims = await _verify_identity(
        id_token=body.id_token,
        request=request,
        log=log,
    )

    try:
        _validate_ref_updates(body.ref_updates)
    except InvalidRefError as e:
        log.warning(
            "push_finalize_invalid_ref",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider="unresolved",
                policy_decision="not_evaluated",
                cas_result="not_attempted",
            ),
        )
        raise HTTPException(
            status_code=400,
            detail={"error": "invalid_ref", "message": str(e)},
        )
    log = log.bind(refs=_ref_names(body.ref_updates))

    provider_decision = get_policy().resolve_provider(
        identity=claims.identity,
        groups=claims.groups,
        repo_url=body.repo_url,
        operation="push",
    )
    if not provider_decision.allowed:
        log.warning(
            "push_finalize_policy_denied",
            reason=provider_decision.reason,
            **_push_finalize_audit_fields(
                claims=claims,
                provider="unresolved",
                policy_decision="denied",
                cas_result="not_attempted",
            ),
        )
        raise HTTPException(
            status_code=403,
            detail={
                "error": "forbidden",
                "message": f"{claims.identity} does not have push "
                f"access to {body.repo_url}: {provider_decision.reason}",
            },
        )

    active_active = approved_active_active_payload(body.active_active, log)

    verified_paths: list[str] | None = None
    try:
        verified = await asyncio.to_thread(
            get_receive_helper().verify,
            repo_url=body.repo_url,
            push_id=body.push_id,
            provider=provider_decision.provider,
        )
        verified_paths = _normalize_changed_path_payloads(
            verified.verified_changed_paths
        )
        if _normalize_ref_update_payloads(verified.ref_updates) != _ref_updates_payload(
            body.ref_updates
        ):
            raise ReceiveInvalidBundleError(
                "staged ref updates do not match finalize request"
            )
    except ReceiveInvalidBundleError as e:
        log.warning(
            "push_finalize_invalid_bundle",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="not_evaluated",
                cas_result="not_attempted",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=400,
            detail={"error": "invalid_bundle", "message": str(e)},
        )
    except ReceiveConflictError as e:
        log.warning(
            "push_finalize_conflict",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="not_evaluated",
                cas_result="conflict",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=409,
            detail={"error": "manifest_conflict", "message": str(e)},
        )
    except Exception as e:
        log.error(
            "push_finalize_verify_failed",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="not_evaluated",
                cas_result="verify_failed",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to verify staged push",
            },
        )

    decision = get_policy().evaluate(
        identity=claims.identity,
        groups=claims.groups,
        repo_url=body.repo_url,
        operation="push",
        changed_paths=verified_paths,
    )
    if not decision.allowed:
        log.warning(
            "push_finalize_policy_denied",
            reason=decision.reason,
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="denied",
                cas_result="not_attempted",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=403,
            detail={
                "error": "forbidden",
                "message": f"{claims.identity} does not have push access to "
                f"{body.repo_url}: {decision.reason}",
            },
        )

    if decision.provider != provider_decision.provider:
        log.warning(
            "push_finalize_provider_mismatch",
            policy_provider=decision.provider,
            resolved_provider=provider_decision.provider,
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="provider_mismatch",
                cas_result="not_attempted",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=403,
            detail={
                "error": "forbidden",
                "message": "Ambiguous provider policy for verified push request",
            },
        )

    try:
        result: ReceiveCommitResult = await asyncio.to_thread(
            get_receive_helper().commit,
            repo_url=body.repo_url,
            push_id=body.push_id,
            plan_digest=verified.plan_digest,
            provider=provider_decision.provider,
            active_active=active_active,
        )
    except ReceiveInvalidBundleError as e:
        log.warning(
            "push_finalize_invalid_bundle",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="allowed",
                cas_result="invalid_bundle",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=400,
            detail={"error": "invalid_bundle", "message": str(e)},
        )
    except ReceiveConflictError as e:
        log.warning(
            "push_finalize_conflict",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="allowed",
                cas_result="conflict",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=409,
            detail={"error": "manifest_conflict", "message": str(e)},
        )
    except Exception as e:
        log.error(
            "push_finalize_commit_failed",
            error=str(e),
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="allowed",
                cas_result="commit_failed",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to commit staged push",
            },
        )

    if _ref_update_names_payload(result.ref_updates) != _ref_update_names_payload(
        verified.ref_updates
    ):
        log.error(
            "push_finalize_commit_failed",
            error="receive helper returned mismatched committed ref updates",
            **_push_finalize_audit_fields(
                claims=claims,
                provider=provider_decision.provider,
                policy_decision="allowed",
                cas_result="commit_mismatch",
                verified_paths=verified_paths,
            ),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": "internal",
                "message": "Failed to commit staged push",
            },
        )

    await _cleanup_staging_best_effort(
        get_provider(provider_decision.provider),
        body.repo_url,
        log.bind(provider=provider_decision.provider),
    )

    log.info(
        "push_finalized",
        status=result.status,
        ref_count=len(result.ref_updates),
        operation_id=result.operation_id,
        coordinator_epoch=result.coordinator_epoch,
        writer_region=result.writer_region,
        commit_state=result.commit_state,
        **_push_finalize_audit_fields(
            claims=claims,
            provider=provider_decision.provider,
            policy_decision="allowed",
            cas_result="committed",
            verified_paths=verified_paths,
        ),
    )
    return PushFinalizeResponse(
        status=result.status,
        ref_updates=[PushRefUpdate(**u) for u in result.ref_updates],
        operation_id=result.operation_id,
        coordinator_epoch=result.coordinator_epoch,
        writer_region=result.writer_region,
        manifest_generation=result.manifest_generation,
        commit_state=result.commit_state,
    )


def _validate_ref_updates(ref_updates: list[PushRefUpdate]) -> None:
    if not ref_updates:
        raise InvalidRefError("at least one ref update is required")
    seen: set[str] = set()
    for update in ref_updates:
        if update.ref_name in seen:
            raise InvalidRefError(f"duplicate ref update: {update.ref_name}")
        seen.add(update.ref_name)
        validate_ref_update(update.ref_name, update.old_oid, update.new_oid)


def _ref_names(ref_updates: list[PushRefUpdate]) -> list[str]:
    return [update.ref_name for update in ref_updates]


def _push_finalize_audit_fields(
    *,
    claims: TokenClaims,
    provider: str,
    policy_decision: str,
    cas_result: str,
    verified_paths: list[str] | None = None,
) -> dict[str, Any]:
    fields: dict[str, Any] = {
        "identity": claims.identity,
        "groups_hash": _hash_values(claims.groups),
        "provider": provider,
        "policy_decision": policy_decision,
        "cas_result": cas_result,
    }
    if verified_paths is not None:
        fields["verified_path_count"] = len(verified_paths)
        fields["verified_path_hash"] = _hash_values(verified_paths)
    return fields


def _staging_cleanup_ttl_seconds(log) -> int:
    raw = os.environ.get(
        "CRAB_AUTH_STAGING_TTL_SECONDS",
        str(DEFAULT_STAGING_TTL_SECONDS),
    )
    try:
        return int(raw)
    except ValueError:
        log.warning("push_staging_cleanup_invalid_ttl", value=raw)
        return DEFAULT_STAGING_TTL_SECONDS


async def _cleanup_staging_best_effort(
    provider,
    repo_url: str,
    log,
) -> None:
    ttl_seconds = _staging_cleanup_ttl_seconds(log)
    if ttl_seconds <= 0:
        return

    cleanup = getattr(provider, "cleanup_staging", None)
    if cleanup is None:
        return

    try:
        deleted = await asyncio.to_thread(
            cleanup,
            repo_url=repo_url,
            older_than_seconds=ttl_seconds,
        )
    except Exception as e:
        log.warning(
            "push_staging_cleanup_failed",
            error=str(e),
            ttl_seconds=ttl_seconds,
        )
        return

    log.info(
        "push_staging_cleanup_completed",
        deleted_count=deleted,
        ttl_seconds=ttl_seconds,
    )


def _ref_updates_payload(
    ref_updates: list[PushRefUpdate],
) -> list[dict[str, str | None]]:
    return _normalize_ref_update_payloads([
        {
            "ref_name": update.ref_name,
            "old_oid": update.old_oid,
            "new_oid": update.new_oid,
        }
        for update in ref_updates
    ])


def _normalize_ref_update_payloads(
    ref_updates: list[dict[str, str | None]],
) -> list[dict[str, str | None]]:
    return [
        {
            "ref_name": str(update["ref_name"]),
            "old_oid": _normalize_oid_payload(update.get("old_oid")),
            "new_oid": _normalize_oid_payload(update.get("new_oid")),
        }
        for update in ref_updates
    ]


def _ref_update_names_payload(ref_updates: list[dict[str, str | None]]) -> list[str]:
    return sorted(str(update["ref_name"]) for update in ref_updates)


def _normalize_changed_path_payloads(paths: list[str]) -> list[str]:
    try:
        return sorted(_validate_changed_path_list(paths))
    except ValueError as e:
        raise ReceiveInvalidBundleError(f"invalid changed path: {e}") from e


def _normalize_oid_payload(value: str | None) -> str | None:
    if value is None:
        return None
    normalized = value.strip()
    if not normalized or normalized == "0" * 40:
        return None
    return normalized.lower()


async def _verify_identity(
    *,
    id_token: str,
    request: Request,
    log: Any,
) -> TokenClaims:
    allowed, retry_after = await get_rate_limiter().allow(
        client_rate_limit_key(request)
    )
    if not allowed:
        log.warning("rate_limited", retry_after=retry_after)
        raise HTTPException(
            status_code=429,
            detail={
                "error": "rate_limited",
                "message": "Too many credential requests",
            },
            headers={"Retry-After": str(retry_after)},
        )

    _validate_authorization_header(request, id_token, log)
    verifier = get_verifier()
    try:
        return await verifier.verify(id_token)
    except ValueError as e:
        log.warning("token_verification_failed", error=str(e))
        raise HTTPException(
            status_code=401,
            detail={"error": "unauthorized", "message": str(e)},
        )


async def _authorize_request(
    *,
    id_token: str,
    repo_url: str,
    operation: str,
    request: Request,
    log: Any,
    changed_paths: list[str] | None = None,
) -> tuple[TokenClaims, PolicyDecision]:
    _parse_repo_url_or_400(repo_url, log)
    allowed, retry_after = await get_rate_limiter().allow(
        client_rate_limit_key(request)
    )
    if not allowed:
        log.warning("rate_limited", retry_after=retry_after)
        raise HTTPException(
            status_code=429,
            detail={
                "error": "rate_limited",
                "message": "Too many credential requests",
            },
            headers={"Retry-After": str(retry_after)},
        )

    _validate_authorization_header(request, id_token, log)
    verifier = get_verifier()
    try:
        claims: TokenClaims = await verifier.verify(id_token)
    except ValueError as e:
        log.warning("token_verification_failed", error=str(e))
        raise HTTPException(
            status_code=401,
            detail={"error": "unauthorized", "message": str(e)},
        )

    decision: PolicyDecision = get_policy().evaluate(
        identity=claims.identity,
        groups=claims.groups,
        repo_url=repo_url,
        operation=operation,
        changed_paths=changed_paths,
    )
    if not decision.allowed:
        log.warning(
            "access_denied",
            reason=decision.reason,
            identity=claims.identity,
            groups_hash=_hash_values(claims.groups),
            policy_decision="denied",
        )
        raise HTTPException(
            status_code=403,
            detail={
                "error": "forbidden",
                "message": f"{claims.identity} does not have {operation} "
                f"access to {repo_url}: {decision.reason}",
            },
        )

    return claims, decision


def _validate_authorization_header(request: Request, id_token: str, log: Any) -> None:
    header = request.headers.get("authorization")
    if header is None:
        return
    scheme, separator, token = header.partition(" ")
    if scheme.lower() != "bearer" or not separator or not token.strip():
        log.warning("authorization_header_invalid")
        raise HTTPException(
            status_code=401,
            detail={
                "error": "unauthorized",
                "message": "Authorization header must be a Bearer token",
            },
        )
    if token.strip() != id_token:
        log.warning("authorization_header_mismatch")
        raise HTTPException(
            status_code=401,
            detail={
                "error": "unauthorized",
                "message": "Authorization header token does not match request token",
            },
        )


def _resolve_provider_or_403(
    *,
    claims: TokenClaims,
    repo_url: str,
    operation: str,
    log: Any,
) -> PolicyDecision:
    decision = get_policy().resolve_provider(
        identity=claims.identity,
        groups=claims.groups,
        repo_url=repo_url,
        operation=operation,
    )
    if decision.allowed:
        return decision

    log.warning(
        "provider_resolution_denied",
        reason=decision.reason,
        identity=claims.identity,
        groups_hash=_hash_values(claims.groups),
    )
    raise HTTPException(
        status_code=403,
        detail={
            "error": "forbidden",
            "message": f"{claims.identity} does not have {operation} "
            f"access to {repo_url}: {decision.reason}",
        },
    )


def _parse_repo_url_or_400(repo_url: str, log: Any) -> ParsedRepoUrl:
    try:
        return parse_repo_url(repo_url)
    except RepoUrlError as e:
        log.warning("invalid_repo_url", error=str(e))
        raise HTTPException(
            status_code=400,
            detail={
                "error": "invalid_repo_url",
                "message": str(e),
            },
        ) from e


def _validate_push_id_or_400(push_id: str, log: Any) -> str:
    try:
        return validate_push_id(push_id)
    except RepoUrlError as e:
        log.warning("invalid_push_id", error=str(e))
        raise HTTPException(
            status_code=400,
            detail={
                "error": "invalid_push_id",
                "message": str(e),
            },
        ) from e


def _upload_prefix(repo_url: str, push_id: str) -> str:
    parsed = parse_repo_url(repo_url)
    return f"{parsed.prefix}/staging/{validate_push_id(push_id)}/"


def _push_upload_permissions(permissions: list[str]) -> list[str]:
    if "write" in permissions or "immutable-write" in permissions:
        return ["immutable-write"]
    return permissions


def _requires_filtered_read_view(decision: PolicyDecision) -> bool:
    return decision.read_paths is not None or bool(decision.denied_read_paths)


def _read_scope_hash(
    *,
    source_repo: str,
    read_paths: list[str] | None,
    denied_read_paths: list[str],
) -> str:
    payload = {
        "source_repo": source_repo,
        "read_paths": sorted(read_paths if read_paths is not None else ["*"]),
        "denied_read_paths": sorted(denied_read_paths),
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def _hash_values(values: list[str]) -> str:
    encoded = "\0".join(sorted(values)).encode()
    return hashlib.sha256(encoded).hexdigest()


# ---------------------------------------------------------------------------
# Standalone entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    import uvicorn

    port = int(os.environ.get("PORT", "8080"))
    log_level = os.environ.get("CRAB_AUTH_LOG_LEVEL", "info").lower()
    uvicorn.run(app, host="0.0.0.0", port=port, log_level=log_level)
