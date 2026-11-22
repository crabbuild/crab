"""Obtain a short-lived viewer grant for a private hosted CDN fixture."""

from __future__ import annotations

import base64
import json
import os
import re
import time
from datetime import datetime
from urllib.parse import SplitResult, parse_qsl, quote, urlsplit

from run_cdn_transport_checks import (
    FAMILY_ROOTS, fixture_base, fixture_repository_prefix, fixture_object_prefix, https_response,
    native_remote, private_viewer_headers, viewer_policy,
)

from cdn_qualification import MAX_VIEWER_GRANT_SECONDS, SHA256, CdnEvidenceError, decode, fields, matches, positive, require


HOST_LABEL = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?")
AUDIENCE = re.compile(r"[\x21-\x7e]{1,256}")
BEARER = re.compile(r"[\x21-\x7e]{1,20000}")
JWT = re.compile(r"[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
PROVIDERS = {"cloudfront": ("aws", "cloudfront"), "cloud-cdn": ("gcp", "cloud_cdn"),
             "front-door": ("azure", "front_door")}
MAX_ISSUER_RESPONSE = 1024 * 1024
APPROVAL_HEADER = "X-Crab-CDN-Approval-Sha256"


def https_endpoint(value: str, label: str) -> SplitResult:
    require(isinstance(value, str) and len(value) <= 8192 and re.fullmatch(r"[\x21-\x7e]+", value)
            and not any(char in value for char in "#\\"), f"invalid {label} URL")
    try:
        endpoint = urlsplit(value)
        host = endpoint.hostname
        require(endpoint.scheme == "https" and host and endpoint.netloc == host
                and "." in host and len(host) <= 253 and all(matches(HOST_LABEL, part) for part in host.split("."))
                and endpoint.path.startswith("/") and value == endpoint.geturl(), f"invalid {label} URL")
    except ValueError:
        raise CdnEvidenceError(f"invalid {label} URL") from None
    return endpoint


def issuer_settings(fixture: dict) -> tuple[SplitResult, str, str]:
    settings = fields(fixture.get("viewer_issuer"), {"endpoint", "audience", "approval_sha256"}, "CDN viewer issuer")
    endpoint = https_endpoint(settings["endpoint"], "Crab Auth issuer")
    require(endpoint.path == "/v1/credentials" and not endpoint.query
            and endpoint.hostname != fixture_base(fixture["base_url"]).hostname,
            "Crab Auth issuer must use its own HTTPS /v1/credentials endpoint")
    require(matches(AUDIENCE, settings["audience"]), "invalid OIDC audience")
    require(matches(SHA256, settings["approval_sha256"]), "invalid private binding approval digest")
    return endpoint, settings["audience"], settings["approval_sha256"]


def response_json(
    endpoint: SplitResult, target: str, headers: dict[str, str], label: str, *,
    body: bytes | None = None, approval: str | None = None,
) -> dict:
    with https_response(endpoint, "GET" if body is None else "POST", target, headers, label, body=body) as response:
        require(response.status == 200, f"{label} rejected the request (HTTP {response.status})")
        require(response.headers.get_all("Content-Encoding") in (None, ["identity"]),
                f"{label} returned a transformed response")
        if body is not None:
            directives = response.getheader("Cache-Control", "").lower().split(",")
            require(any(entry.strip() == "no-store" for entry in directives),
                    "Crab Auth viewer response must require no-store")
            require(response.headers.get_all(APPROVAL_HEADER) == [approval],
                    "Crab Auth returned a different private binding approval")
        raw = response.read(MAX_ISSUER_RESPONSE + 1)
        require(len(raw) <= MAX_ISSUER_RESPONSE, f"{label} response is too large")
    try:
        value = decode(raw)
    except (UnicodeError, ValueError, RecursionError):
        raise CdnEvidenceError(f"{label} returned invalid JSON") from None
    require(isinstance(value, dict), f"{label} response must be a JSON object")
    return value


def github_id_token(audience: str) -> str:
    endpoint = https_endpoint(os.environ.get("ACTIONS_ID_TOKEN_REQUEST_URL", ""), "GitHub OIDC runtime")
    require(endpoint.hostname.endswith(".actions.githubusercontent.com") and endpoint.query
            and not any(name.casefold() == "audience" for name, _ in parse_qsl(endpoint.query, keep_blank_values=True)),
            "GitHub OIDC request must use the hosted runtime and the approved audience")
    bearer = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "")
    require(matches(BEARER, bearer), "GitHub OIDC runtime request token is unavailable")
    target = f"{endpoint.path}?{endpoint.query}&audience={quote(audience, safe='')}"
    response = response_json(endpoint, target, {"Authorization": f"Bearer {bearer}", "Accept-Encoding": "identity"},
                             "GitHub OIDC runtime")
    token = response.get("value")
    require(matches(BEARER, token) and matches(JWT, token), "GitHub OIDC runtime returned no bounded ID token")
    return token


def approved_credential(fixture: dict, edge: str, family: str, response: dict) -> dict[str, str]:
    source = fixture["fixture_prefix"]
    physical = fixture_repository_prefix(fixture, "private", family)
    global_prefix = f"{physical}/.crab" if physical != source else None
    expected = {"provider", "credentials", "expires_at", "permissions", "cdn"}
    if global_prefix is not None:
        expected.add("storage_scope")
    fields(response, expected, "Crab Auth viewer response")
    provider, viewer_provider = PROVIDERS[edge]
    require(response["provider"] == provider and isinstance(response["credentials"], dict)
            and response["permissions"] == ["read"], "Crab Auth returned a different provider or read permission")
    if global_prefix is not None:
        scope_hash = fixture["view_suffix"].split("/")[3]
        require(response["storage_scope"] == {"repo_prefix": physical, "global_prefix": global_prefix,
                                              "source_repo": source, "scope_hash": scope_hash},
                "Crab Auth returned a different protected view")
    envelope = fields(response["cdn"], {"provider", "origin", "base_url", "repo_prefix", "expires_at", "grants"}
                      | ({"global_prefix"} if global_prefix is not None else set()), "Crab Auth viewer grant")
    base_url = fixture["base_url"].rstrip("/")
    require((envelope["provider"], envelope["origin"], envelope["base_url"], envelope["repo_prefix"], envelope.get("global_prefix"))
            == (viewer_provider, fixture["native_origin"], base_url, physical, global_prefix),
            "Crab Auth viewer grant differs from the approved origin, viewer endpoint, or physical prefix")
    try:
        parent = datetime.fromisoformat(response["expires_at"])
        require(parent.tzinfo is not None, "Crab Auth expiry needs a timezone")
        parent_epoch = parent.timestamp()
    except (ValueError, TypeError, OverflowError):
        raise CdnEvidenceError("Crab Auth returned an invalid parent expiration") from None
    expiry = envelope["expires_at"]
    now = time.time()
    require(positive(expiry) and now < expiry <= min(parent_epoch, now + MAX_VIEWER_GRANT_SECONDS),
            "Crab Auth viewer grant must be live and bounded by its parent read grant")
    grants = envelope["grants"]
    require(isinstance(grants, list) and 0 < len(grants) <= 32, "invalid Crab Auth viewer grant list")
    prefix = f"{physical}/{fixture_object_prefix(fixture, FAMILY_ROOTS[family])}/"
    candidates = [fields(grant, {"prefix", "credential"}, "Crab Auth viewer credential") for grant in grants]
    matches_prefix = [grant["credential"] for grant in candidates if grant["prefix"] == prefix]
    require(len(matches_prefix) == 1, "Crab Auth must issue one grant for the approved object family")
    viewer = private_viewer_headers(edge, matches_prefix[0])
    signed_expiry = verify_viewer_scope(edge, viewer, fixture["native_origin"], base_url, physical, prefix)
    require(signed_expiry == expiry, "viewer signed expiration differs from the grant envelope")
    return viewer


def verify_viewer_scope(
    edge: str, viewer: dict[str, str], origin: str, base_url: str, physical: str, prefix: str,
) -> int:
    signed_expiry, policy = viewer_policy(edge, viewer)
    now = time.time()
    require(now < signed_expiry <= now + MAX_VIEWER_GRANT_SECONDS, "viewer credential must be live and short-lived")
    resource = f"{base_url}/{prefix}"
    if edge == "cloudfront":
        cookie_parts = [part.strip() for part in viewer["Cookie"].split(";")]
        pairs = [re.fullmatch(r"(CloudFront-[A-Za-z-]+)=([A-Za-z0-9_~-]+)", part) for part in cookie_parts]
        require(all(pairs), "Crab Auth returned an invalid CloudFront cookie set")
        cookies = {pair[1]: pair[2] for pair in pairs}
        names = {"CloudFront-Policy", "CloudFront-Signature", "CloudFront-Key-Pair-Id"}
        if cookies.get("CloudFront-Hash-Algorithm") == "SHA256":
            names.add("CloudFront-Hash-Algorithm")
        require(len(cookies) == len(pairs) and cookies.keys() == names,
                "Crab Auth returned an unexpected CloudFront cookie set")
        correct = policy == {"Statement": [{"Resource": resource + "*",
                                            "Condition": {"DateLessThan": {"AWS:EpochTime": signed_expiry}}}]}
    elif edge == "cloud-cdn":
        correct = policy == base64.urlsafe_b64encode(resource.encode()).decode()
    else:
        require(matches(JWT, viewer["Authorization"].removeprefix("Bearer ")), "invalid Azure viewer JWT")
        correct = (
            policy.get("iss"), policy.get("aud"), policy.get("sub"), policy.get("prefix"), policy.get("origin"),
        ) == (base_url, base_url, physical, prefix, origin)
    require(correct, "signed viewer policy differs from the approved object family or endpoint")
    return signed_expiry


def issue_viewer(fixture: dict, edge: str, family: str, token: str) -> dict[str, str]:
    endpoint, _, approval = issuer_settings(fixture)
    require(matches(BEARER, token) and matches(JWT, token), "Crab Auth requires a bounded OIDC ID token")
    body = json.dumps({"id_token": token, "repo_url": native_remote(fixture, edge),
                       "operation": "fetch", "client_version": "provider-cdn-qualification"}).encode()
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json",
               "Accept-Encoding": "identity", "X-Crab-CDN-Grant-Version": "1", APPROVAL_HEADER: approval}
    response = response_json(endpoint, endpoint.path, headers, "Crab Auth viewer issuer", body=body, approval=approval)
    return approved_credential(fixture, edge, family, response)


def issue_github_viewer(fixture: dict, edge: str, family: str) -> dict[str, str]:
    _, audience, _ = issuer_settings(fixture)
    return issue_viewer(fixture, edge, family, github_id_token(audience))
