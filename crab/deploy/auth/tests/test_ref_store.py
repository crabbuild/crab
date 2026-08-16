"""Tests for protected push ref validation."""

from __future__ import annotations

import pytest

from src.ref_store import InvalidRefError, _normalize_oid, _ref_key, validate_ref_update


def test_ref_key_builds_repo_scoped_ref_path():
    assert _ref_key("team/repo", "refs/heads/main") == "team/repo/refs/heads/main"
    assert _ref_key("", "refs/heads/main") == "refs/heads/main"


def test_ref_key_rejects_unsafe_ref_names():
    with pytest.raises(InvalidRefError):
        _ref_key("team/repo", "../main")
    with pytest.raises(InvalidRefError):
        _ref_key("team/repo", "refs/heads//main")


def test_protected_push_accepts_branch_refs_only():
    validate_ref_update("refs/heads/main", "1" * 40, "2" * 40)

    for ref_name in [
        "refs/tags/v1.0",
        "refs/notes/review",
        "refs/pull/1/head",
        "refs/heads/",
    ]:
        with pytest.raises(InvalidRefError):
            validate_ref_update(ref_name, "1" * 40, "2" * 40)


def test_protected_push_rejects_git_unsafe_branch_refs():
    for ref_name in [
        "refs/heads/.hidden",
        "refs/heads/main.lock",
        "refs/heads/main/",
        "refs/heads/main.",
        "refs/heads/main@{1}",
        "refs/heads/main~1",
        "refs/heads/main:other",
        "refs/heads/main other",
        "refs/heads/main[1]",
        "refs/heads/main\\other",
    ]:
        with pytest.raises(InvalidRefError):
            validate_ref_update(ref_name, "1" * 40, "2" * 40)


def test_protected_push_rejects_noop_ref_updates():
    with pytest.raises(InvalidRefError, match="no-op"):
        validate_ref_update("refs/heads/main", "a" * 40, "A" * 40)


def test_normalize_oid_maps_empty_and_zero_to_missing():
    assert _normalize_oid(None) is None
    assert _normalize_oid("") is None
    assert _normalize_oid("0" * 40) is None


def test_normalize_oid_requires_sha1_hex():
    assert _normalize_oid("A" * 40) == "a" * 40
    with pytest.raises(InvalidRefError):
        _normalize_oid("z" * 40)
    with pytest.raises(InvalidRefError):
        _normalize_oid("a" * 39)
