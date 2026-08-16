"""Minimal OIDC Identity Provider for local testing.

Provides:
- /.well-known/openid-configuration — discovery document
- /.well-known/jwks.json — public signing keys
- /token — issue a signed ID token for a given identity

This is NOT a real IdP. It's a test fixture that issues valid JWTs
signed with a known key so the crab-auth endpoint can verify them.
"""

import hashlib
import json
import os
import secrets
import time
from typing import Any
from urllib.parse import parse_qs

import jwt as pyjwt
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.backends import default_backend
from fastapi import FastAPI, HTTPException, Query, Request

app = FastAPI(title="Mock OIDC IdP", version="0.1.0")

# Load a mounted evaluation key when configured so container restarts do not
# silently rotate the issuer underneath clients. The generated fallback keeps
# the standalone legacy fixture behavior.
_PRIVATE_KEY_FILE = os.environ.get("MOCK_OIDC_PRIVATE_KEY_FILE")
if _PRIVATE_KEY_FILE:
    with open(_PRIVATE_KEY_FILE, "rb") as private_key_file:
        _PRIVATE_KEY = serialization.load_pem_private_key(
            private_key_file.read(),
            password=None,
            backend=default_backend(),
        )
else:
    _PRIVATE_KEY = rsa.generate_private_key(
        public_exponent=65537,
        key_size=2048,
        backend=default_backend(),
    )
_PUBLIC_KEY = _PRIVATE_KEY.public_key()

_PUBLIC_DER = _PUBLIC_KEY.public_bytes(
    encoding=serialization.Encoding.DER,
    format=serialization.PublicFormat.SubjectPublicKeyInfo,
)
_KID = hashlib.sha256(_PUBLIC_DER).hexdigest()[:16]
_ISSUER = os.environ.get("MOCK_OIDC_ISSUER", "http://mock-idp:9090")
_AUDIENCE = os.environ.get("MOCK_OIDC_AUDIENCE", "crab-cli")
_DEVICE_TOKEN_TTL_SECONDS = int(os.environ.get("MOCK_OIDC_DEVICE_TOKEN_TTL_SECONDS", "3600"))
_REFRESH_TOKEN_TTL_SECONDS = int(os.environ.get("MOCK_OIDC_REFRESH_TOKEN_TTL_SECONDS", "3600"))
_IDENTITY = {
    "sub": os.environ.get("MOCK_OIDC_SUBJECT", "user-123"),
    "email": os.environ.get("MOCK_OIDC_EMAIL", "alice@corp.example.com"),
}
_COUNTERS = {"device_grants": 0, "refresh_grants": 0}


def _public_key_jwk() -> dict[str, Any]:
    """Export the public key as a JWK."""
    numbers = _PUBLIC_KEY.public_numbers()

    def _int_to_base64url(n: int, length: int) -> str:
        import base64
        data = n.to_bytes(length, byteorder="big")
        return base64.urlsafe_b64encode(data).rstrip(b"=").decode()

    return {
        "kty": "RSA",
        "kid": _KID,
        "use": "sig",
        "alg": "RS256",
        "n": _int_to_base64url(numbers.n, 256),
        "e": _int_to_base64url(numbers.e, 3),
    }


@app.get("/.well-known/openid-configuration")
def discovery():
    """OIDC discovery document."""
    return {
        "issuer": _ISSUER,
        "authorization_endpoint": f"{_ISSUER}/authorize",
        "token_endpoint": f"{_ISSUER}/token",
        "jwks_uri": f"{_ISSUER}/.well-known/jwks.json",
        "device_authorization_endpoint": f"{_ISSUER}/device",
        "revocation_endpoint": f"{_ISSUER}/revoke",
        "userinfo_endpoint": f"{_ISSUER}/userinfo",
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }


@app.get("/.well-known/jwks.json")
def jwks():
    """JSON Web Key Set — the public key for verifying tokens."""
    return {"keys": [_public_key_jwk()]}


@app.get("/token")
def issue_token(
    email: str = Query(default="alice@corp.example.com"),
    sub: str = Query(default="user-123"),
    groups: str = Query(default=""),
    expires_in: int = Query(default=3600),
):
    """Issue a signed ID token for testing.

    Usage:
        curl "http://localhost:9090/token?email=alice@corp.example.com&groups=ml-team,admins"

    Returns a JSON object with the signed JWT.
    """
    return _issue_token_response(email, sub, groups, expires_in, include_refresh=False)


def _issue_token_response(
    email: str,
    sub: str,
    groups: str,
    expires_in: int,
    *,
    include_refresh: bool,
) -> dict[str, Any]:
    now = int(time.time())
    group_list = [g.strip() for g in groups.split(",") if g.strip()] if groups else []

    payload = {
        "iss": _ISSUER,
        "sub": sub,
        "aud": _AUDIENCE,
        "email": email,
        "name": email.split("@")[0].replace(".", " ").title(),
        "groups": group_list,
        "iat": now,
        "exp": now + expires_in,
    }

    token = pyjwt.encode(
        payload,
        _PRIVATE_KEY,
        algorithm="RS256",
        headers={"kid": _KID},
    )

    response = {
        "id_token": token,
        "access_token": token,
        "token_type": "Bearer",
        "email": email,
        "groups": group_list,
        "expires_in": expires_in,
    }
    if include_refresh:
        response["refresh_token"] = f"mock-refresh-{secrets.token_urlsafe(24)}"
    return response


@app.post("/device")
async def device_authorization(request: Request):
    """Issue an immediately approved device code for non-interactive E2E login."""
    form = parse_qs((await request.body()).decode())
    if form.get("client_id", [None])[0] != _AUDIENCE:
        raise HTTPException(status_code=400, detail="invalid client_id")
    device_code = f"mock-device-{secrets.token_urlsafe(24)}"
    return {
        "device_code": device_code,
        "user_code": "CRAB-E2E",
        "verification_uri": f"{_ISSUER}/verify",
        "interval": 1,
        "expires_in": 60,
    }


@app.post("/token")
async def exchange_token(request: Request):
    """Exchange a mock device or refresh grant for a signed token set."""
    form = parse_qs((await request.body()).decode())
    if form.get("client_id", [None])[0] != _AUDIENCE:
        raise HTTPException(status_code=400, detail="invalid client_id")
    grant_type = form.get("grant_type", [""])[0]
    if grant_type == "urn:ietf:params:oauth:grant-type:device_code":
        if not form.get("device_code", [""])[0].startswith("mock-device-"):
            raise HTTPException(status_code=400, detail="invalid device_code")
        _COUNTERS["device_grants"] += 1
        return _issue_token_response(
            _IDENTITY["email"],
            _IDENTITY["sub"],
            "",
            _DEVICE_TOKEN_TTL_SECONDS,
            include_refresh=True,
        )
    if grant_type == "refresh_token":
        if not form.get("refresh_token", [""])[0].startswith("mock-refresh-"):
            raise HTTPException(status_code=400, detail="invalid refresh_token")
        _COUNTERS["refresh_grants"] += 1
        return _issue_token_response(
            _IDENTITY["email"],
            _IDENTITY["sub"],
            "",
            _REFRESH_TOKEN_TTL_SECONDS,
            include_refresh=True,
        )
    raise HTTPException(status_code=400, detail="unsupported grant_type")


@app.post("/revoke", status_code=204)
async def revoke_token():
    """Accept revocation for the local fixture without retaining token material."""


@app.post("/test/identity")
def select_test_identity(
    sub: str = Query(...),
    email: str = Query(...),
):
    """Select the identity minted by later device and refresh grants."""
    _IDENTITY.update({"sub": sub, "email": email})
    return {"sub": sub, "email": email}


@app.get("/test/state")
def test_state():
    """Expose non-secret counters used to prove refresh behavior."""
    return {"identity": dict(_IDENTITY), **_COUNTERS}


@app.get("/health")
def health():
    return {"status": "ok"}
