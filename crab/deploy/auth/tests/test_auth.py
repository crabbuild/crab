"""Tests for JWT verification."""

import jwt as pyjwt
import pytest

from src.auth import TokenClaims

# Import test helpers — pytest auto-discovers conftest.py, but for
# direct imports we need the full path. These are re-exported from conftest.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
from conftest import _TEST_PUBLIC_KEY, make_token


class TestTokenClaims:
    """Test TokenClaims identity resolution."""

    def test_identity_prefers_email(self):
        claims = TokenClaims(
            subject="user-123",
            email="alice@example.com",
            name="Alice",
            groups=[],
        )
        assert claims.identity == "alice@example.com"

    def test_identity_falls_back_to_subject(self):
        claims = TokenClaims(
            subject="user-123",
            email=None,
            name=None,
            groups=[],
        )
        assert claims.identity == "user-123"

    def test_groups_default_empty(self):
        claims = TokenClaims(
            subject="user-123",
            email=None,
            name=None,
            groups=[],
        )
        assert claims.groups == []


class TestMakeToken:
    """Test the test helper itself to ensure tokens are well-formed."""

    def test_default_token_decodes(self):
        token = make_token()
        payload = pyjwt.decode(
            token,
            _TEST_PUBLIC_KEY,
            algorithms=["RS256"],
            audience="crab-cli",
            issuer="https://idp.example.com",
        )
        assert payload["sub"] == "user-123"
        assert payload["email"] == "alice@corp.example.com"

    def test_expired_token_is_expired(self):
        token = make_token(expires_in=-3600)
        with pytest.raises(pyjwt.ExpiredSignatureError):
            pyjwt.decode(
                token,
                _TEST_PUBLIC_KEY,
                algorithms=["RS256"],
                audience="crab-cli",
                issuer="https://idp.example.com",
            )

    def test_custom_groups(self):
        token = make_token(groups=["ml-team", "admins"])
        payload = pyjwt.decode(
            token,
            _TEST_PUBLIC_KEY,
            algorithms=["RS256"],
            audience="crab-cli",
            issuer="https://idp.example.com",
        )
        assert payload["groups"] == ["ml-team", "admins"]

    def test_token_has_kid_header(self):
        token = make_token()
        header = pyjwt.get_unverified_header(token)
        assert header["kid"] == "test-key-1"
        assert header["alg"] == "RS256"
