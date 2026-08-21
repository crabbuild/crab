"""Rust receive-helper orchestration for protected Crab pushes."""

from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass
from typing import Any, Protocol

_HEX_64_RE = re.compile(r"^[0-9a-f]{64}$")
_OID_RE = re.compile(r"^[0-9a-fA-F]{40}$")
_PREPARE_FIELDS = frozenset({"status", "source_generation"})
_VERIFY_FIELDS = frozenset({"ref_updates", "verified_changed_paths", "plan_digest"})
_COMMIT_FIELDS = frozenset({
    "status",
    "ref_updates",
    "operation_id",
    "coordinator_epoch",
    "writer_region",
    "manifest_generation",
    "commit_state",
})
_DOCTOR_FIELDS = frozenset({"status", "git_version"})
_REF_UPDATE_FIELDS = frozenset({"ref_name", "old_oid", "new_oid"})
_COMMIT_STATES = frozenset({
    "pending",
    "objects_uploaded",
    "committed",
    "materialized",
    "aborted",
})


@dataclass
class ReceivePrepareResult:
    status: str
    source_generation: int | None


@dataclass
class ReceiveVerifyResult:
    ref_updates: list[dict[str, str | None]]
    verified_changed_paths: list[str]
    plan_digest: str


@dataclass
class ReceiveCommitResult:
    status: str
    ref_updates: list[dict[str, str | None]]
    operation_id: str | None = None
    coordinator_epoch: int | None = None
    writer_region: str | None = None
    manifest_generation: int | None = None
    commit_state: str | None = None


@dataclass
class ReceiveRuntimeStatus:
    status: str
    git_version: str


class ReceiveInvalidBundleError(Exception):
    """Raised when staged push data is malformed or stale."""


class ReceiveConflictError(Exception):
    """Raised when the manifest CAS or expected ref state is stale."""


class ReceiveHelper(Protocol):
    def prepare(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
        ref_updates: list[dict[str, str | None]],
        view_scope: dict[str, str] | None = None,
    ) -> ReceivePrepareResult: ...

    def verify(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
    ) -> ReceiveVerifyResult: ...

    def commit(
        self,
        *,
        repo_url: str,
        push_id: str,
        plan_digest: str,
        provider: str,
        active_active: dict[str, Any] | None = None,
    ) -> ReceiveCommitResult: ...

    def check_runtime(self) -> ReceiveRuntimeStatus: ...


class SubprocessReceiveHelper:
    """Calls the packaged `crab-auth-receive` Rust helper."""

    def __init__(self) -> None:
        self._binary = os.environ.get("CRAB_AUTH_RECEIVE_HELPER", "crab-auth-receive")
        self._timeout = int(os.environ.get("CRAB_AUTH_RECEIVE_TIMEOUT_SECONDS", "300"))

    def prepare(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
        ref_updates: list[dict[str, str | None]],
        view_scope: dict[str, str] | None = None,
    ) -> ReceivePrepareResult:
        args = [
            "prepare",
            "--repo-url",
            repo_url,
            "--push-id",
            push_id,
            "--provider",
            provider,
            "--ref-updates-json",
            json.dumps(ref_updates, separators=(",", ":")),
        ]
        if view_scope is not None:
            args.extend([
                "--view-scope-json",
                json.dumps(view_scope, separators=(",", ":"), sort_keys=True),
            ])
        output = self._run(args)
        return _prepare_result(output)

    def verify(
        self,
        *,
        repo_url: str,
        push_id: str,
        provider: str,
    ) -> ReceiveVerifyResult:
        output = self._run([
            "verify",
            "--repo-url",
            repo_url,
            "--push-id",
            push_id,
            "--provider",
            provider,
        ])
        return _verify_result(output)

    def commit(
        self,
        *,
        repo_url: str,
        push_id: str,
        plan_digest: str,
        provider: str,
        active_active: dict[str, Any] | None = None,
    ) -> ReceiveCommitResult:
        args = [
            "commit",
            "--repo-url",
            repo_url,
            "--push-id",
            push_id,
            "--plan-digest",
            plan_digest,
            "--provider",
            provider,
        ]
        if active_active is not None:
            args.extend([
                "--active-active-json",
                json.dumps(active_active, separators=(",", ":"), sort_keys=True),
            ])
        output = self._run(args)
        return _commit_result(output)

    def check_runtime(self) -> ReceiveRuntimeStatus:
        output = self._run(["doctor"])
        return _doctor_result(output)

    def _run(self, args: list[str]) -> dict[str, Any]:
        try:
            completed = subprocess.run(
                [self._binary, *args],
                check=False,
                capture_output=True,
                text=True,
                timeout=self._timeout,
            )
        except FileNotFoundError as e:
            raise RuntimeError("receive helper binary not found") from e
        except subprocess.TimeoutExpired as e:
            raise RuntimeError("receive helper timed out") from e
        if completed.returncode != 0:
            raise _map_helper_error(completed.stderr.strip())
        try:
            output = json.loads(completed.stdout)
        except json.JSONDecodeError as e:
            raise ReceiveInvalidBundleError(
                f"receive helper returned invalid JSON: {e}"
            ) from e
        if not isinstance(output, dict):
            raise ReceiveInvalidBundleError(
                "receive helper returned a non-object JSON response"
            )
        return output


def _prepare_result(output: dict[str, Any]) -> ReceivePrepareResult:
    _reject_unknown_fields(output, _PREPARE_FIELDS, "prepare result")
    status = _required_string(output, "status")
    if status != "prepared":
        raise ReceiveInvalidBundleError("receive helper returned invalid status")
    source_generation = output.get("source_generation")
    if source_generation is not None and not isinstance(source_generation, int):
        raise ReceiveInvalidBundleError(
            "receive helper returned invalid source_generation"
        )
    return ReceivePrepareResult(
        status=status,
        source_generation=source_generation,
    )


def _verify_result(output: dict[str, Any]) -> ReceiveVerifyResult:
    _reject_unknown_fields(output, _VERIFY_FIELDS, "verify result")
    return ReceiveVerifyResult(
        ref_updates=_ref_updates(output),
        verified_changed_paths=_string_list(output, "verified_changed_paths"),
        plan_digest=_plan_digest(output),
    )


def _commit_result(output: dict[str, Any]) -> ReceiveCommitResult:
    _reject_unknown_fields(output, _COMMIT_FIELDS, "commit result")
    status = _required_string(output, "status")
    if status != "updated":
        raise ReceiveInvalidBundleError("receive helper returned invalid status")
    active_active = _active_active_commit_metadata(output)
    return ReceiveCommitResult(
        status=status,
        ref_updates=_ref_updates(output),
        operation_id=active_active["operation_id"],
        coordinator_epoch=active_active["coordinator_epoch"],
        writer_region=active_active["writer_region"],
        manifest_generation=active_active["manifest_generation"],
        commit_state=active_active["commit_state"],
    )


def _active_active_commit_metadata(output: dict[str, Any]) -> dict[str, Any]:
    fields = {
        "operation_id",
        "coordinator_epoch",
        "writer_region",
        "manifest_generation",
        "commit_state",
    }
    present = {field for field in fields if field in output and output[field] is not None}
    if not present:
        return {
            "operation_id": None,
            "coordinator_epoch": None,
            "writer_region": None,
            "manifest_generation": None,
            "commit_state": None,
        }
    if present != fields:
        raise ReceiveInvalidBundleError(
            "receive helper returned partial active-active commit metadata"
        )
    operation_id = _required_string(output, "operation_id")
    writer_region = _required_string(output, "writer_region")
    coordinator_epoch = _required_int(output, "coordinator_epoch")
    manifest_generation = _required_int(output, "manifest_generation")
    commit_state = _required_string(output, "commit_state")
    if commit_state not in _COMMIT_STATES:
        raise ReceiveInvalidBundleError(
            "receive helper returned invalid active-active commit_state"
        )
    return {
        "operation_id": operation_id,
        "coordinator_epoch": coordinator_epoch,
        "writer_region": writer_region,
        "manifest_generation": manifest_generation,
        "commit_state": commit_state,
    }


def _doctor_result(output: dict[str, Any]) -> ReceiveRuntimeStatus:
    _reject_unknown_fields(output, _DOCTOR_FIELDS, "doctor result")
    status = _required_string(output, "status")
    git_version = _required_string(output, "git_version")
    if status != "ok":
        raise RuntimeError("receive helper doctor returned invalid status")
    if not git_version.startswith("git version "):
        raise RuntimeError("receive helper doctor returned invalid git version")
    return ReceiveRuntimeStatus(status=status, git_version=git_version)


def _ref_updates(output: dict[str, Any]) -> list[dict[str, str | None]]:
    value = output.get("ref_updates")
    if not isinstance(value, list):
        raise ReceiveInvalidBundleError("receive helper returned invalid ref_updates")

    updates = []
    for item in value:
        if not isinstance(item, dict):
            raise ReceiveInvalidBundleError("receive helper returned invalid ref update")
        _reject_unknown_fields(item, _REF_UPDATE_FIELDS, "ref update")
        ref_name = item.get("ref_name")
        old_oid = item.get("old_oid")
        new_oid = item.get("new_oid")
        if not isinstance(ref_name, str) or not ref_name:
            raise ReceiveInvalidBundleError("receive helper returned invalid ref_name")
        old_oid = _oid_or_none(old_oid, "old_oid")
        new_oid = _oid_or_none(new_oid, "new_oid")
        if new_oid is None:
            raise ReceiveInvalidBundleError("receive helper returned invalid new_oid")
        updates.append({
            "ref_name": ref_name,
            "old_oid": old_oid,
            "new_oid": new_oid,
        })
    return updates


def _reject_unknown_fields(
    output: dict[str, Any], allowed: frozenset[str], context: str
) -> None:
    unknown = sorted(set(output) - allowed)
    if unknown:
        raise ReceiveInvalidBundleError(
            f"receive helper returned unknown {context} field: {unknown[0]}"
        )


def _oid_or_none(value: Any, field: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str):
        raise ReceiveInvalidBundleError(f"receive helper returned invalid {field}")
    normalized = value.strip()
    if not normalized or normalized == "0" * 40:
        return None
    if not _OID_RE.fullmatch(normalized):
        raise ReceiveInvalidBundleError(f"receive helper returned invalid {field}")
    return normalized.lower()


def _string_list(output: dict[str, Any], field: str) -> list[str]:
    value = output.get(field)
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise ReceiveInvalidBundleError(f"receive helper returned invalid {field}")
    return value


def _plan_digest(output: dict[str, Any]) -> str:
    value = _required_string(output, "plan_digest")
    if not _HEX_64_RE.fullmatch(value):
        raise ReceiveInvalidBundleError("receive helper returned invalid plan_digest")
    return value


def _required_string(output: dict[str, Any], field: str) -> str:
    value = output.get(field)
    if not isinstance(value, str) or not value:
        raise ReceiveInvalidBundleError(f"receive helper returned invalid {field}")
    return value


def _required_int(output: dict[str, Any], field: str) -> int:
    value = output.get(field)
    if not isinstance(value, int):
        raise ReceiveInvalidBundleError(f"receive helper returned invalid {field}")
    return value


def _map_helper_error(stderr: str) -> Exception:
    if stderr.startswith("conflict:"):
        return ReceiveConflictError(stderr.removeprefix("conflict:").strip())
    if stderr.startswith("invalid:"):
        return ReceiveInvalidBundleError(stderr.removeprefix("invalid:").strip())
    return RuntimeError(stderr or "receive helper failed")
