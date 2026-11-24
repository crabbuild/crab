"""Viewer grants follow the existing read policy and provider signature contracts."""

from __future__ import annotations

import base64
import hmac
import json
from datetime import datetime, timedelta, timezone

import jwt
import pytest
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa

from src.cdn import CdnExport, issue_viewer_grant, resolve_export
from src.cdn_approval import CdnApproval
from src.policy import PolicyEngine

NOW = datetime(2026, 10, 1, tzinfo=timezone.utc)
VIEW_SUFFIX = f"/acl-views/v2/{'a' * 64}/7-{'b' * 64}"


@pytest.fixture(scope="module")
def rsa_key():
    return rsa.generate_private_key(public_exponent=65537, key_size=2048)


@pytest.fixture(autouse=True)
def approved_placement(monkeypatch):
    # Provider placement and receipt checks have their own tests. This module
    # exercises the wire signatures and viewer scope checks.
    monkeypatch.setattr(CdnApproval, "verify", lambda *args, **kwargs: None)


def make_export(tmp_path, rsa_key, provider="cloudfront", **overrides):
    key_file = tmp_path / "key"
    if provider == "cloud_cdn":
        key_file.write_bytes(base64.urlsafe_b64encode(bytes(range(16))))
    else:
        key_file.write_bytes(rsa_key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        ))
    origins = {
        "cloudfront": "s3://test-bucket",
        "cloud_cdn": "gs://test-bucket",
        "front_door": "https://account.blob.core.windows.net/test-bucket",
    }
    fields = dict(
        provider=provider,
        origin=origins[provider],
        repo_prefix="org/models",
        base_url="https://downloads.example.com/objects",
        allow_unfiltered=True,
        approval={"path": tmp_path / "approval", "sha256": "a" * 64},
        key_id="viewer-key",
        key_file=key_file,
    )
    return CdnExport(**(fields | overrides))


def mint(export, **overrides):
    arguments = dict(
        operation="fetch",
        permissions=["read"],
        parent_expiry=(NOW + timedelta(seconds=90)).isoformat(),
        view_repo_prefix=None,
        view_global_prefix=None,
        credentials={},
        approvals=frozenset({export.approval.sha256}) if export.approval is not None else frozenset(),
        now=NOW,
    )
    return issue_viewer_grant(export, **(arguments | overrides))


def test_cloudfront_cookie_verifies_real_policy_and_parent_expiry(tmp_path, rsa_key):
    grant = mint(make_export(tmp_path, rsa_key))
    assert grant is not None
    first = grant.grants[0]
    values = dict(pair.split("=", 1) for pair in first.credential.split("; "))
    assert values["CloudFront-Hash-Algorithm"] == "SHA256"
    assert values["CloudFront-Key-Pair-Id"] == "viewer-key"
    alphabet = str.maketrans("-_~", "+=/")
    policy = base64.b64decode(values["CloudFront-Policy"].translate(alphabet))
    signature = base64.b64decode(values["CloudFront-Signature"].translate(alphabet))
    rsa_key.public_key().verify(signature, policy, padding.PKCS1v15(), hashes.SHA256())
    assert first.prefix == "org/models/v2/capsules/"
    assert json.loads(policy)["Statement"] == [{
        "Resource": "https://downloads.example.com/objects/org/models/v2/capsules/*",
        "Condition": {"DateLessThan": {"AWS:EpochTime": int(NOW.timestamp()) + 90}},
    }]
    assert grant.global_prefix is None


def test_google_cookie_signature_has_delimited_filtered_view_prefix(tmp_path, rsa_key):
    export = make_export(
        tmp_path,
        rsa_key,
        provider="cloud_cdn",
        allow_unfiltered=False,
        protected_view_suffix=VIEW_SUFFIX,
    )
    repo = export.repo_prefix + VIEW_SUFFIX
    grant = mint(export, view_repo_prefix=repo, view_global_prefix=f"{repo}/.crab")
    assert grant is not None
    xorb = next(item for item in grant.grants if item.prefix.endswith("/xorbs/"))
    cookie = xorb.credential.removeprefix("Cloud-CDN-Cookie=")
    policy, signature = cookie.split(":Signature=")
    value = policy.split(":")[0].removeprefix("URLPrefix=")
    assert base64.urlsafe_b64decode(value).decode() == f"{export.base_url}/{repo}/.crab/xorbs/"
    assert base64.urlsafe_b64decode(signature) == hmac.digest(
        bytes(range(16)), policy.encode(), "sha1"
    )
    assert all(item.prefix.startswith(f"{repo}/") for item in grant.grants)
    assert mint(export) is None


@pytest.mark.parametrize("provider", ["cloudfront", "cloud_cdn", "front_door"])
def test_viewer_grant_requires_exact_approved_view(tmp_path, rsa_key, provider):
    export = make_export(
        tmp_path, rsa_key, provider=provider, allow_unfiltered=False, protected_view_suffix=VIEW_SUFFIX,
    )
    repo = export.repo_prefix + VIEW_SUFFIX
    grant = mint(export, view_repo_prefix=repo, view_global_prefix=f"{repo}/.crab")
    assert grant is not None
    assert grant.repo_prefix == repo
    assert grant.global_prefix == f"{repo}/.crab"
    assert {item.prefix for item in grant.grants if item.prefix.startswith(f"{repo}/.crab/")} == {
        f"{repo}/.crab/xorbs/",
        f"{repo}/.crab/shards/",
        f"{repo}/.crab/capsule-chunk-pages/",
    }
    assert all(item.prefix.startswith(f"{repo}/") for item in grant.grants)
    other_views = [
        repo.replace("/7-", "/8-"),
        repo.replace("a" * 64, "c" * 64),
        repo.replace("b" * 64, "c" * 64),
    ]
    for other in other_views:
        assert mint(export, view_repo_prefix=other, view_global_prefix=f"{other}/.crab") is None
    with pytest.raises(ValueError, match="global prefix"):
        mint(export, view_repo_prefix=repo, view_global_prefix=".crab")
    assert mint(export) is None


def test_unfiltered_export_omits_viewer_grant_for_protected_reader(tmp_path, rsa_key):
    export = make_export(tmp_path, rsa_key)
    repo = export.repo_prefix + VIEW_SUFFIX
    assert mint(export, view_repo_prefix=repo, view_global_prefix=f"{repo}/.crab") is None


def test_export_without_private_binding_approval_cannot_mint(tmp_path, rsa_key):
    export = make_export(tmp_path, rsa_key, approval=None)
    export.key_file.unlink()
    assert mint(export) is None


def test_azure_jwt_signs_gateway_audience_origin_and_prefix(tmp_path, rsa_key):
    export = make_export(tmp_path, rsa_key, provider="front_door")
    grant = mint(export)
    assert grant is not None
    first = grant.grants[0]
    claims = jwt.decode(
        first.credential,
        rsa_key.public_key(),
        algorithms=["RS256"],
        audience=export.base_url,
        issuer=export.base_url,
        options={"verify_exp": False, "require": ["exp", "prefix", "origin"]},
    )
    assert claims["prefix"] == first.prefix
    assert claims["origin"] == export.origin
    assert claims["exp"] == grant.expires_at
    assert resolve_export((export,), "azure", "test-bucket", "org/models", {
        "storage_account": "different-account",
    }) is None


def test_grant_rejects_cross_view_and_write_authority(tmp_path, rsa_key):
    export = make_export(tmp_path, rsa_key)
    assert mint(export, operation="fsck") is None
    assert mint(export, permissions=["read", "write"]) is None
    assert mint(export, parent_expiry=NOW.isoformat()) is None
    with pytest.raises(ValueError, match="outside"):
        mint(export, view_repo_prefix="org/other/acl-views/v2/generation")
    assert resolve_export((export,), "s3", "test-bucket", "org/models", {}) is None


def test_policy_disallows_ambiguous_exports_and_bad_base(tmp_path, rsa_key):
    export = make_export(tmp_path, rsa_key)
    with pytest.raises(ValueError, match="Duplicate"):
        PolicyEngine.from_dict({"cdn": [export.model_dump(), export.model_dump()]})
    alias = make_export(
        tmp_path, rsa_key, provider="cloud_cdn", base_url=f"{export.base_url}/org",
        repo_prefix="models",
    )
    with pytest.raises(ValueError, match="viewer path is bound more than once"):
        PolicyEngine.from_dict({"cdn": [export.model_dump(), alias.model_dump()]})
    protected = make_export(
        tmp_path, rsa_key, allow_unfiltered=False, protected_view_suffix=VIEW_SUFFIX,
    )
    physical_alias = make_export(
        tmp_path, rsa_key, provider="cloud_cdn", repo_prefix=protected.repo_prefix + VIEW_SUFFIX,
    )
    with pytest.raises(ValueError, match="physical CDN viewer path"):
        PolicyEngine.from_dict({"cdn": [protected.model_dump(), physical_alias.model_dump()]})
    with pytest.raises(ValueError, match="HTTPS"):
        make_export(tmp_path, rsa_key, base_url="https://viewer:secret@example.com")
    with pytest.raises(ValueError, match="exact protected v2 view suffix"):
        make_export(tmp_path, rsa_key, allow_unfiltered=False, protected_view_suffix="/acl-views/*")
    with pytest.raises(ValueError, match="both source and protected view"):
        make_export(tmp_path, rsa_key, protected_view_suffix=VIEW_SUFFIX)
