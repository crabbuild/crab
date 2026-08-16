"""JWT verification against the IdP's JWKS endpoint.

Verifies:
- Signature (RS256 or ES256) against the IdP's published keys
- Issuer (`iss`) matches expected value
- Audience (`aud`) matches expected value
- Expiration (`exp`) is in the future
- Not-before (`nbf`) is in the past (if present)
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any

import jwt
from jwt import PyJWKClient
import structlog

logger = structlog.get_logger()

# Cache JWKS for 1 hour to avoid hitting the IdP on every request.
_JWKS_CACHE_LIFETIME = 3600


@dataclass
class TokenClaims:
    """Verified claims extracted from an ID token."""

    subject: str
    email: str | None
    name: str | None
    groups: list[str]

    @property
    def identity(self) -> str:
        """Primary identity — email if available, otherwise subject."""
        return self.email or self.subject


class JWTVerifier:
    """Verifies OIDC ID tokens against the IdP's JWKS.

    This is the security-critical component. Every token is verified:
    1. Fetch signing keys from the JWKS endpoint (cached)
    2. Decode and verify the JWT signature (RS256/ES256)
    3. Validate iss, aud, exp, nbf claims
    """

    def __init__(self, jwks_url: str, issuer: str, audience: str) -> None:
        self._jwks_url = jwks_url
        self._issuer = issuer
        self._audience = audience
        self._jwks_client = PyJWKClient(
            jwks_url,
            cache_jwk_set=True,
            lifespan=_JWKS_CACHE_LIFETIME,
        )

    async def verify(self, token: str) -> TokenClaims:
        """Verify an ID token and return extracted claims.

        Raises ValueError if the token is invalid, expired, or has
        a bad signature.
        """
        try:
            payload = await asyncio.to_thread(self._decode_payload, token)
        except jwt.ExpiredSignatureError:
            raise ValueError("ID token has expired")
        except jwt.InvalidAudienceError:
            raise ValueError(
                f"ID token audience does not match expected: {self._audience}"
            )
        except jwt.InvalidIssuerError:
            raise ValueError(
                f"ID token issuer does not match expected: {self._issuer}"
            )
        except jwt.InvalidSignatureError:
            raise ValueError("ID token signature verification failed")
        except jwt.DecodeError as e:
            raise ValueError(f"ID token is malformed: {e}")
        except jwt.PyJWKClientError as e:
            raise ValueError(f"Failed to fetch signing keys from JWKS: {e}")
        except Exception as e:
            raise ValueError(f"Token verification failed: {e}")

        # Extract claims.
        subject = payload.get("sub", "")
        email = payload.get("email")
        name = payload.get("name")
        groups = payload.get("groups", [])

        if not isinstance(groups, list):
            groups = []

        return TokenClaims(
            subject=subject,
            email=email,
            name=name,
            groups=[str(g) for g in groups],
        )

    async def check_runtime(self) -> dict[str, int]:
        """Verify the JWKS endpoint returns at least one signing key."""
        try:
            signing_keys = await asyncio.to_thread(
                self._jwks_client.get_signing_keys
            )
        except jwt.PyJWKClientError as e:
            raise ValueError(f"JWKS endpoint is unavailable: {e}") from e
        except Exception as e:
            raise ValueError(f"JWKS readiness check failed: {e}") from e
        return {"key_count": len(signing_keys)}

    def _decode_payload(self, token: str) -> dict[str, Any]:
        # PyJWKClient performs synchronous network I/O on cache misses.
        signing_key = self._jwks_client.get_signing_key_from_jwt(token)
        return jwt.decode(
            token,
            signing_key.key,
            algorithms=["RS256", "ES256"],
            issuer=self._issuer,
            audience=self._audience,
            options={
                "verify_signature": True,
                "verify_exp": True,
                "verify_nbf": True,
                "verify_iss": True,
                "verify_aud": True,
                "require": ["exp", "iss", "sub"],
            },
        )
