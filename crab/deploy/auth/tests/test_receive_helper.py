"""Tests for protected push receive-helper subprocess mapping."""

from __future__ import annotations

import subprocess

from src.receive_helper import (
    ReceiveConflictError,
    ReceiveInvalidBundleError,
    SubprocessReceiveHelper,
    _commit_result,
    _doctor_result,
    _map_helper_error,
    _prepare_result,
    _verify_result,
)


def test_map_helper_error_conflict_prefix_returns_conflict():
    err = _map_helper_error("conflict: base manifest generation changed")

    assert isinstance(err, ReceiveConflictError)
    assert str(err) == "base manifest generation changed"


def test_map_helper_error_invalid_prefix_returns_invalid_bundle():
    err = _map_helper_error("invalid: staged plan digest changed after verification")

    assert isinstance(err, ReceiveInvalidBundleError)
    assert str(err) == "staged plan digest changed after verification"


def test_map_helper_error_unknown_prefix_returns_runtime_error():
    err = _map_helper_error("git executable missing")

    assert isinstance(err, RuntimeError)
    assert str(err) == "git executable missing"


def test_map_helper_error_empty_stderr_keeps_generic_runtime_error():
    err = _map_helper_error("")

    assert isinstance(err, RuntimeError)
    assert str(err) == "receive helper failed"


def test_verify_result_validates_success_shape():
    result = _verify_result({
        "ref_updates": [
            {
                "ref_name": "refs/heads/main",
                "old_oid": None,
                "new_oid": "1" * 40,
            }
        ],
        "verified_changed_paths": ["src/lib.rs"],
        "plan_digest": "a" * 64,
    })

    assert result.ref_updates[0]["ref_name"] == "refs/heads/main"
    assert result.verified_changed_paths == ["src/lib.rs"]
    assert result.plan_digest == "a" * 64


def test_prepare_result_validates_success_shape():
    result = _prepare_result({
        "status": "prepared",
        "source_generation": 7,
    })

    assert result.status == "prepared"
    assert result.source_generation == 7


def test_prepare_result_rejects_unknown_top_level_field():
    try:
        _prepare_result({
            "status": "prepared",
            "source_generation": None,
            "legacy": True,
        })
    except ReceiveInvalidBundleError as err:
        assert "unknown prepare result field" in str(err)
    else:
        raise AssertionError("unknown prepare result fields must fail")


def test_verify_result_rejects_missing_verified_paths():
    try:
        _verify_result({
            "ref_updates": [],
            "plan_digest": "a" * 64,
        })
    except ReceiveInvalidBundleError as err:
        assert "verified_changed_paths" in str(err)
    else:
        raise AssertionError("missing verified paths must fail")


def test_verify_result_rejects_unknown_top_level_field():
    try:
        _verify_result({
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                }
            ],
            "verified_changed_paths": ["src/lib.rs"],
            "plan_digest": "a" * 64,
            "legacy_mode": True,
        })
    except ReceiveInvalidBundleError as err:
        assert "unknown verify result field" in str(err)
    else:
        raise AssertionError("unknown verify result fields must fail")


def test_commit_result_rejects_invalid_ref_update_shape():
    try:
        _commit_result({
            "status": "updated",
            "ref_updates": [{"ref_name": "refs/heads/main", "new_oid": ""}],
        })
    except ReceiveInvalidBundleError as err:
        assert "new_oid" in str(err)
    else:
        raise AssertionError("invalid ref update must fail")


def test_commit_result_rejects_unknown_top_level_field():
    try:
        _commit_result({
            "status": "updated",
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                }
            ],
            "partial": False,
        })
    except ReceiveInvalidBundleError as err:
        assert "unknown commit result field" in str(err)
    else:
        raise AssertionError("unknown commit result fields must fail")


def test_commit_result_rejects_unknown_ref_update_field():
    try:
        _commit_result({
            "status": "updated",
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                    "force": True,
                }
            ],
        })
    except ReceiveInvalidBundleError as err:
        assert "unknown ref update field" in str(err)
    else:
        raise AssertionError("unknown helper ref update fields must fail")


def test_verify_result_rejects_invalid_plan_digest():
    try:
        _verify_result({
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                }
            ],
            "verified_changed_paths": ["src/lib.rs"],
            "plan_digest": "not-a-digest",
        })
    except ReceiveInvalidBundleError as err:
        assert "plan_digest" in str(err)
    else:
        raise AssertionError("invalid plan digest must fail")


def test_commit_result_rejects_unknown_status():
    try:
        _commit_result({
            "status": "partial",
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                }
            ],
        })
    except ReceiveInvalidBundleError as err:
        assert "status" in str(err)
    else:
        raise AssertionError("unknown helper commit status must fail")


def test_doctor_result_validates_runtime_shape():
    result = _doctor_result({
        "status": "ok",
        "git_version": "git version 2.50.0",
    })

    assert result.status == "ok"
    assert result.git_version == "git version 2.50.0"


def test_doctor_result_rejects_unknown_top_level_field():
    try:
        _doctor_result({
            "status": "ok",
            "git_version": "git version 2.50.0",
            "path": "/usr/bin/git",
        })
    except ReceiveInvalidBundleError as err:
        assert "unknown doctor result field" in str(err)
    else:
        raise AssertionError("unknown doctor result fields must fail")


def test_doctor_result_rejects_invalid_git_version():
    try:
        _doctor_result({
            "status": "ok",
            "git_version": "2.50.0",
        })
    except RuntimeError as err:
        assert str(err) == "receive helper doctor returned invalid git version"
    else:
        raise AssertionError("invalid doctor git version must fail")


def test_ref_update_oids_are_validated_and_normalized():
    result = _commit_result({
        "status": "updated",
        "ref_updates": [
            {
                "ref_name": "refs/heads/main",
                "old_oid": "A" * 40,
                "new_oid": "B" * 40,
            }
        ],
    })

    assert result.ref_updates == [
        {
            "ref_name": "refs/heads/main",
            "old_oid": "a" * 40,
            "new_oid": "b" * 40,
        }
    ]


def test_commit_result_parses_active_active_metadata():
    result = _commit_result({
        "status": "updated",
        "ref_updates": [
            {
                "ref_name": "refs/heads/main",
                "old_oid": None,
                "new_oid": "1" * 40,
            }
        ],
        "operation_id": "op-123",
        "coordinator_epoch": 7,
        "writer_region": "us-west-2",
        "manifest_generation": 42,
        "commit_state": "materialized",
    })

    assert result.operation_id == "op-123"
    assert result.coordinator_epoch == 7
    assert result.writer_region == "us-west-2"
    assert result.manifest_generation == 42
    assert result.commit_state == "materialized"


def test_commit_result_rejects_partial_active_active_metadata():
    try:
        _commit_result({
            "status": "updated",
            "ref_updates": [
                {
                    "ref_name": "refs/heads/main",
                    "old_oid": None,
                    "new_oid": "1" * 40,
                }
            ],
            "operation_id": "op-123",
        })
    except ReceiveInvalidBundleError as err:
        assert "partial active-active commit metadata" in str(err)
    else:
        raise AssertionError("partial active-active metadata must fail")


def test_subprocess_run_rejects_non_object_json(monkeypatch):
    class Completed:
        returncode = 0
        stdout = "[]"
        stderr = ""

    monkeypatch.setattr("subprocess.run", lambda *args, **kwargs: Completed())
    helper = SubprocessReceiveHelper()

    try:
        helper._run(["verify"])
    except ReceiveInvalidBundleError as err:
        assert "non-object JSON" in str(err)
    else:
        raise AssertionError("non-object helper JSON must fail")


def test_subprocess_verify_builds_command_and_validates_result(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = (
            '{"ref_updates":[{"ref_name":"refs/heads/main",'
            '"old_oid":null,"new_oid":"1111111111111111111111111111111111111111"}],'
            '"verified_changed_paths":["src/lib.rs"],'
            '"plan_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}'
        )
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    result = helper.verify(
        repo_url="crab://bucket/team/repo",
        push_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        provider="aws",
    )

    assert calls[0][0] == [
        "crab-auth-receive",
        "verify",
        "--repo-url",
        "crab://bucket/team/repo",
        "--push-id",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--provider",
        "aws",
    ]
    assert calls[0][1]["timeout"] == 300
    assert result.verified_changed_paths == ["src/lib.rs"]
    assert result.plan_digest == "a" * 64


def test_subprocess_prepare_builds_command_and_validates_result(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = '{"status":"prepared","source_generation":4}'
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    result = helper.prepare(
        repo_url="crab://bucket/team/repo",
        push_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        provider="aws",
        ref_updates=[
            {
                "ref_name": "refs/heads/main",
                "old_oid": None,
                "new_oid": "1" * 40,
            }
        ],
    )

    assert calls[0][0] == [
        "crab-auth-receive",
        "prepare",
        "--repo-url",
        "crab://bucket/team/repo",
        "--push-id",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--provider",
        "aws",
        "--ref-updates-json",
        (
            '[{"ref_name":"refs/heads/main","old_oid":null,'
            '"new_oid":"1111111111111111111111111111111111111111"}]'
        ),
    ]
    assert calls[0][1]["timeout"] == 300
    assert result.status == "prepared"
    assert result.source_generation == 4


def test_subprocess_prepare_passes_view_scope_json(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = '{"status":"prepared","source_generation":4}'
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    helper.prepare(
        repo_url="crab://bucket/team/repo",
        push_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        provider="aws",
        ref_updates=[
            {
                "ref_name": "refs/heads/main",
                "old_oid": None,
                "new_oid": "1" * 40,
            }
        ],
        view_scope={
            "repo_prefix": "team/repo/acl-views/v1/scope/1-manifest",
            "global_prefix": "team/repo/acl-views/v1/scope/1-manifest/.crab",
            "source_repo": "team/repo",
            "scope_hash": "a" * 64,
        },
    )

    assert "--view-scope-json" in calls[0][0]
    scope_json = calls[0][0][calls[0][0].index("--view-scope-json") + 1]
    assert scope_json == (
        '{"global_prefix":"team/repo/acl-views/v1/scope/1-manifest/.crab",'
        '"repo_prefix":"team/repo/acl-views/v1/scope/1-manifest",'
        '"scope_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",'
        '"source_repo":"team/repo"}'
    )


def test_subprocess_commit_builds_command_and_validates_result(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = (
            '{"status":"updated",'
            '"ref_updates":[{"ref_name":"refs/heads/main",'
            '"old_oid":null,"new_oid":"1111111111111111111111111111111111111111"}]}'
        )
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    result = helper.commit(
        repo_url="crab://bucket/team/repo",
        push_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        plan_digest="a" * 64,
        provider="aws",
    )

    assert calls[0][0] == [
        "crab-auth-receive",
        "commit",
        "--repo-url",
        "crab://bucket/team/repo",
        "--push-id",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--plan-digest",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--provider",
        "aws",
    ]
    assert result.status == "updated"
    assert result.ref_updates[0]["new_oid"] == "1" * 40


def test_subprocess_commit_passes_active_active_json(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = (
            '{"status":"updated",'
            '"ref_updates":[{"ref_name":"refs/heads/main",'
            '"old_oid":null,"new_oid":"1111111111111111111111111111111111111111"}],'
            '"operation_id":"op-123","coordinator_epoch":7,'
            '"writer_region":"us-west-2","manifest_generation":42,'
            '"commit_state":"materialized"}'
        )
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    result = helper.commit(
        repo_url="crab://bucket/team/repo",
        push_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        plan_digest="a" * 64,
        provider="aws",
        active_active={
            "writer": "west",
            "replication": {
                "mode": "active-active",
                "coordinator": {"url": "dynamodb://crab-coordinator"},
            },
        },
    )

    assert "--active-active-json" in calls[0][0]
    active_active_json = calls[0][0][calls[0][0].index("--active-active-json") + 1]
    assert active_active_json == (
        '{"replication":{"coordinator":{"url":"dynamodb://crab-coordinator"},'
        '"mode":"active-active"},"writer":"west"}'
    )
    assert result.operation_id == "op-123"
    assert result.commit_state == "materialized"


def test_subprocess_check_runtime_builds_doctor_command(monkeypatch):
    calls = []

    class Completed:
        returncode = 0
        stdout = '{"status":"ok","git_version":"git version 2.50.0"}'
        stderr = ""

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    monkeypatch.setattr("subprocess.run", fake_run)
    helper = SubprocessReceiveHelper()

    result = helper.check_runtime()

    assert calls[0][0] == ["crab-auth-receive", "doctor"]
    assert calls[0][1]["timeout"] == 300
    assert result.status == "ok"
    assert result.git_version == "git version 2.50.0"


def test_subprocess_run_maps_missing_binary_to_runtime_error(monkeypatch):
    def missing_binary(*args, **kwargs):
        raise FileNotFoundError("not found")

    monkeypatch.setattr("subprocess.run", missing_binary)
    helper = SubprocessReceiveHelper()

    try:
        helper._run(["verify"])
    except RuntimeError as err:
        assert str(err) == "receive helper binary not found"
    else:
        raise AssertionError("missing receive helper binary must fail cleanly")


def test_subprocess_run_maps_timeout_to_runtime_error(monkeypatch):
    def timed_out(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=["crab-auth-receive"], timeout=1)

    monkeypatch.setattr("subprocess.run", timed_out)
    helper = SubprocessReceiveHelper()

    try:
        helper._run(["verify"])
    except RuntimeError as err:
        assert str(err) == "receive helper timed out"
    else:
        raise AssertionError("timed out receive helper must fail cleanly")
