"""Shared test fixtures for the crab-auth test suite."""

from __future__ import annotations

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
