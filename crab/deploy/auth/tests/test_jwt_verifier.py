"""Direct tests for JWTVerifier."""

from __future__ import annotations

import pytest
import jwt as pyjwt
from cryptography.hazmat.primitives.asymmetric import rsa

from src.auth import JWTVerifier


class StaticSigningKey:
    def __init__(self, key) -> None:
        self.key = key


class StaticJwksClient:
    def __init__(self, key) -> None:
        self._key = key

    def get_signing_key_from_jwt(self, token: str) -> StaticSigningKey:
        return StaticSigningKey(self._key)

    def get_signing_keys(self):
        return [StaticSigningKey(self._key)]


def verifier(public_key) -> JWTVerifier:
    v = JWTVerifier(
        jwks_url="https://idp.example.com/.well-known/jwks.json",
        issuer="https://idp.example.com",
        audience="crab-cli",
    )
    v._jwks_client = StaticJwksClient(public_key)
    return v


def make_test_token(
    private_key,
    *,
    issuer: str = "https://idp.example.com",
    audience: str = "crab-cli",
    expires_in: int = 3600,
    email: str = "alice@corp.example.com",
    groups=None,
    extra_claims: dict | None = None,
) -> str:
    import time

    now = int(time.time())
    payload = {
        "sub": "user-123",
        "iss": issuer,
        "aud": audience,
        "exp": now + expires_in,
        "iat": now,
        "email": email,
    }
    if groups is not None:
        payload["groups"] = groups
    if extra_claims:
        payload.update(extra_claims)
    return pyjwt.encode(
        payload,
        private_key,
        algorithm="RS256",
        headers={"kid": "test-key-1"},
    )


@pytest.mark.asyncio
async def test_verify_valid_token(public_key, private_key):
    claims = await verifier(public_key).verify(
        make_test_token(private_key, groups=["ml-team"])
    )
    assert claims.subject == "user-123"
    assert claims.identity == "alice@corp.example.com"
    assert claims.groups == ["ml-team"]


@pytest.mark.asyncio
async def test_verify_expired_token(public_key, private_key):
    with pytest.raises(ValueError, match="expired"):
        await verifier(public_key).verify(make_test_token(private_key, expires_in=-1))


@pytest.mark.asyncio
async def test_verify_invalid_audience(public_key, private_key):
    with pytest.raises(ValueError, match="audience"):
        await verifier(public_key).verify(
            make_test_token(private_key, audience="other-client")
        )


@pytest.mark.asyncio
async def test_verify_invalid_issuer(public_key, private_key):
    with pytest.raises(ValueError, match="issuer"):
        await verifier(public_key).verify(
            make_test_token(private_key, issuer="https://evil.example.com")
        )


@pytest.mark.asyncio
async def test_verify_invalid_signature(public_key):
    other_private_key = rsa.generate_private_key(
        public_exponent=65537,
        key_size=2048,
    )

    with pytest.raises(ValueError, match="signature"):
        await verifier(public_key).verify(make_test_token(other_private_key))


@pytest.mark.asyncio
async def test_verify_future_nbf(public_key, private_key):
    import time

    with pytest.raises(ValueError, match="nbf|not yet valid"):
        await verifier(public_key).verify(
            make_test_token(
                private_key,
                extra_claims={"nbf": int(time.time()) + 3600},
            )
        )


@pytest.mark.asyncio
async def test_verify_missing_required_exp(public_key, private_key):
    import time

    token = pyjwt.encode(
        {
            "sub": "user-123",
            "iss": "https://idp.example.com",
            "aud": "crab-cli",
            "iat": int(time.time()),
            "email": "alice@corp.example.com",
        },
        private_key,
        algorithm="RS256",
        headers={"kid": "test-key-1"},
    )

    with pytest.raises(ValueError, match="exp"):
        await verifier(public_key).verify(token)


@pytest.mark.asyncio
async def test_verify_non_list_groups_becomes_empty(public_key, private_key):
    claims = await verifier(public_key).verify(
        make_test_token(private_key, extra_claims={"groups": "ml-team"})
    )
    assert claims.groups == []


@pytest.mark.asyncio
async def test_check_runtime_reports_jwks_signing_key_count(public_key):
    assert await verifier(public_key).check_runtime() == {"key_count": 1}
