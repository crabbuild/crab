import subprocess

from src.view_helper import (
    SubprocessViewHelper,
    _doctor_result,
    _materialize_result,
)


def test_materialize_result_rejects_unknown_top_level_field():
    try:
        _materialize_result({
            "repo_prefix": "repo/acl-views/v1/scope/7-deadbeef",
            "global_prefix": "repo/acl-views/v1/scope/7-deadbeef/.crab",
            "source_repo": "repo",
            "scope_hash": "scope",
            "source_generation": 7,
            "source_manifest_hash": "deadbeef",
            "cache_hit": True,
            "debug_path": "/tmp/view.git",
        })
    except RuntimeError as err:
        assert "unknown materialize result field" in str(err)
    else:
        raise AssertionError("unknown view materialize fields must fail")


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
    except RuntimeError as err:
        assert "unknown doctor result field" in str(err)
    else:
        raise AssertionError("unknown view doctor fields must fail")


def test_doctor_result_rejects_invalid_git_version():
    try:
        _doctor_result({
            "status": "ok",
            "git_version": "2.50.0",
        })
    except RuntimeError as err:
        assert str(err) == "view helper doctor returned invalid git version"
    else:
        raise AssertionError("invalid view doctor git version must fail")


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
    helper = SubprocessViewHelper()

    result = helper.check_runtime()

    assert calls[0][0] == ["crab-auth-view", "doctor"]
    assert calls[0][1]["timeout"] == 900
    assert result.status == "ok"
    assert result.git_version == "git version 2.50.0"


def test_subprocess_run_maps_timeout_to_runtime_error(monkeypatch):
    def timed_out(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd="crab-auth-view", timeout=900)

    monkeypatch.setattr("subprocess.run", timed_out)
    helper = SubprocessViewHelper()

    try:
        helper.check_runtime()
    except RuntimeError as err:
        assert str(err) == "view helper timed out"
    else:
        raise AssertionError("view helper timeout must fail cleanly")
