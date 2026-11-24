"""Shared test fixtures for the crab-auth test suite."""

from __future__ import annotations

import hashlib
import json
import time

import jwt as pyjwt
import pytest
from cryptography.hazmat.primitives.asymmetric import rsa


# ---------------------------------------------------------------------------
# RSA key pair for test JWT signing
# ---------------------------------------------------------------------------

_TEST_PRIVATE_KEY = rsa.generate_private_key(
    public_exponent=65537,
    key_size=2048,
)
_TEST_PUBLIC_KEY = _TEST_PRIVATE_KEY.public_key()


@pytest.fixture
def private_key():
    """RSA private key for signing test JWTs."""
    return _TEST_PRIVATE_KEY


@pytest.fixture
def public_key():
    """RSA public key for verifying test JWTs."""
    return _TEST_PUBLIC_KEY


# ---------------------------------------------------------------------------
# JWT helpers
# ---------------------------------------------------------------------------


def make_token(
    sub: str = "user-123",
    email: str = "alice@corp.example.com",
    groups: list[str] | None = None,
    issuer: str = "https://idp.example.com",
    audience: str = "crab-cli",
    expires_in: int = 3600,
    extra_claims: dict | None = None,
) -> str:
    """Create a signed JWT for testing."""
    now = int(time.time())
    payload = {
        "sub": sub,
        "iss": issuer,
        "aud": audience,
        "exp": now + expires_in,
        "iat": now,
    }
    if email:
        payload["email"] = email
    if groups:
        payload["groups"] = groups
    if extra_claims:
        payload.update(extra_claims)

    return pyjwt.encode(
        payload,
        _TEST_PRIVATE_KEY,
        algorithm="RS256",
        headers={"kid": "test-key-1"},
    )


@pytest.fixture
def valid_token() -> str:
    """A valid signed JWT with default claims."""
    return make_token()


@pytest.fixture
def expired_token() -> str:
    """An expired JWT."""
    return make_token(expires_in=-3600)


@pytest.fixture
def ml_team_token() -> str:
    """A JWT for a user in the ml-team group."""
    return make_token(
        email="bob@corp.example.com",
        groups=["ml-team"],
    )


# ---------------------------------------------------------------------------
# Policy fixture
# ---------------------------------------------------------------------------

SAMPLE_POLICY = {
    "version": "1",
    "default_provider": "aws",
    "rules": [
        {
            "group": "platform-admins",
            "repos": ["*"],
            "operations": ["*"],
        },
        {
            "group": "ml-team",
            "repos": ["ml-models/*", "datasets/*"],
            "operations": ["push", "fetch", "clone", "gc"],
        },
        {
            "identity": "alice@corp.example.com",
            "repos": ["experiments/alice/*"],
            "operations": ["push", "fetch", "clone"],
        },
        {
            "identity": "*",
            "repos": ["public/*"],
            "operations": ["fetch", "clone"],
        },
    ],
    "deny": [
        {
            "identity": "banned@corp.example.com",
            "repos": ["*"],
            "operations": ["*"],
        },
    ],
}


@pytest.fixture
def sample_policy() -> dict:
    """Sample RBAC policy for testing."""
    return SAMPLE_POLICY


@pytest.fixture
def approve_cdn(tmp_path):
    def create(entry, **observed):
        path = tmp_path / "private-binding.json"
        edge = entry["provider"].replace("_", "-")
        bucket = entry["origin"].rsplit("/", 1)[-1]
        azure_group = "/subscriptions/00000000-0000-0000-0000-000000000001/resourcegroups/test"
        account = "account"
        resources = {
            "cloudfront": ("arn:aws:cloudfront::123456789012:distribution/EDIST",
                           f"accounts/123456789012/buckets/{bucket}", "us-west-2"),
            "cloud_cdn": ("projects/test-project/global/backendBuckets/test-backend",
                          f"projects/123456789/buckets/{bucket}", "US"),
            "front_door": (f"{azure_group}/providers/microsoft.cdn/profiles/test/afdendpoints/test/routes/test",
                           f"{azure_group}/providers/microsoft.storage/storageaccounts/{account}/blobservices/default/containers/{bucket}",
                           "westus2"),
        }
        deployment, native_resource, native_location = resources[entry["provider"]]
        binding = {
            "deployment": deployment, "native_resource": native_resource, "native_location": native_location,
            "native_origin": entry["origin"], "repo_prefix": entry["repo_prefix"],
            "view_suffix": entry.get("protected_view_suffix", ""), "base_url": entry["base_url"],
        } | observed
        raw = (json.dumps({
            "schema": "crab.cdn-private-binding-candidate/v1", "binding": binding,
            "edge": edge,
            "configuration_sha256": "a" * 64, "deployment_policy_sha256": "b" * 64,
            "object": {"key_sha256": "d" * 64, "body_sha256": "e" * 64, "bytes": 8},
        }, sort_keys=True) + "\n").encode()
        path.write_bytes(raw)
        return {"path": path, "sha256": hashlib.sha256(raw).hexdigest()}
    return create
