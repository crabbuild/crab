"""Ref update input validation for protected push requests."""

from __future__ import annotations


class InvalidRefError(Exception):
    """Raised when a ref name or object ID is not safe to receive."""


def validate_ref_update(ref_name: str, old_oid: str | None, new_oid: str) -> None:
    _validate_ref_name(ref_name)
    normalized_old = _normalize_oid(old_oid)
    normalized_new = _normalize_oid(new_oid)
    if normalized_new is None:
        raise InvalidRefError("new object id must be non-zero")
    if normalized_old == normalized_new:
        raise InvalidRefError("protected push does not allow no-op ref updates")


def _ref_key(prefix: str, ref_name: str) -> str:
    _validate_ref_name(ref_name)
    prefix = prefix.strip("/")
    ref_path = ref_name.removeprefix("refs/")
    return f"{prefix}/refs/{ref_path}" if prefix else f"refs/{ref_path}"


def _validate_ref_name(ref_name: str) -> None:
    if not ref_name.startswith("refs/heads/"):
        raise InvalidRefError("protected push only accepts branch refs under refs/heads/")
    if ref_name == "refs/heads/":
        raise InvalidRefError("branch ref name is empty")
    if (
        ref_name.startswith("/")
        or ref_name.endswith("/")
        or ref_name.endswith(".")
        or ".." in ref_name
        or "//" in ref_name
        or "@{" in ref_name
        or ref_name == "@"
        or any(ord(c) < 32 or ord(c) == 127 for c in ref_name)
        or any(c in ref_name for c in " ~^:?*[\\")
    ):
        raise InvalidRefError("ref_name contains an unsafe path component")
    for segment in ref_name.split("/"):
        if segment.startswith(".") or segment.endswith(".lock"):
            raise InvalidRefError("ref_name contains an unsafe path component")


def _normalize_oid(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    if value in {"", "0" * 40}:
        return None
    if len(value) != 40 or any(c not in "0123456789abcdefABCDEF" for c in value):
        raise InvalidRefError("object id must be 40 hex characters")
    return value.lower()
