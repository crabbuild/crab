"""Rust view-helper orchestration for path-scoped read ACLs."""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass
from typing import Any, Protocol

_MATERIALIZE_FIELDS = frozenset({
    "repo_prefix",
    "global_prefix",
    "source_repo",
    "scope_hash",
    "source_generation",
    "source_manifest_hash",
    "cache_hit",
})
_DOCTOR_FIELDS = frozenset({"status", "git_version"})


@dataclass(frozen=True)
class ViewMaterializationResult:
    repo_prefix: str
    global_prefix: str
    source_repo: str
    scope_hash: str
    source_generation: int | None
    source_manifest_hash: str | None
    cache_hit: bool


@dataclass(frozen=True)
class ViewRuntimeStatus:
    status: str
    git_version: str


class ViewHelper(Protocol):
    def materialize(
        self,
        *,
        repo_url: str,
        provider: str,
        scope_hash: str,
        read_paths: list[str],
        denied_read_paths: list[str],
    ) -> ViewMaterializationResult: ...

    def check_runtime(self) -> ViewRuntimeStatus: ...


class SubprocessViewHelper:
    """Calls the packaged `crab-auth-view` Rust helper."""

    def __init__(self) -> None:
        self._binary = os.environ.get("CRAB_AUTH_VIEW_HELPER", "crab-auth-view")
        self._timeout = int(os.environ.get("CRAB_AUTH_VIEW_TIMEOUT_SECONDS", "900"))

    def materialize(
        self,
        *,
        repo_url: str,
        provider: str,
        scope_hash: str,
        read_paths: list[str],
        denied_read_paths: list[str],
    ) -> ViewMaterializationResult:
        args = [
            "materialize",
            "--repo-url",
            repo_url,
            "--provider",
            provider,
            "--scope-hash",
            scope_hash,
        ]
        for path in read_paths:
            args.extend(["--read-path", path])
        for path in denied_read_paths:
            args.extend(["--deny-path", path])
        output = self._run(args)
        return _materialize_result(output)

    def check_runtime(self) -> ViewRuntimeStatus:
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
            raise RuntimeError("view helper binary not found") from e
        except subprocess.TimeoutExpired as e:
            raise RuntimeError("view helper timed out") from e
        if completed.returncode != 0:
            raise RuntimeError(completed.stderr.strip() or "view helper failed")
        try:
            output = json.loads(completed.stdout)
        except json.JSONDecodeError as e:
            raise RuntimeError(f"view helper returned invalid JSON: {e}") from e
        if not isinstance(output, dict):
            raise RuntimeError("view helper returned a non-object JSON response")
        return output


def _materialize_result(output: dict[str, Any]) -> ViewMaterializationResult:
    _reject_unknown_fields(output, _MATERIALIZE_FIELDS, "materialize result")
    repo_prefix = _required_string(output, "repo_prefix")
    global_prefix = _required_string(output, "global_prefix")
    source_repo = _required_string(output, "source_repo")
    scope_hash = _required_string(output, "scope_hash")
    source_generation = _optional_int(output, "source_generation")
    source_manifest_hash = _optional_string(output, "source_manifest_hash")
    cache_hit = output.get("cache_hit")
    if not isinstance(cache_hit, bool):
        raise RuntimeError("view helper returned invalid cache_hit")
    return ViewMaterializationResult(
        repo_prefix=repo_prefix,
        global_prefix=global_prefix,
        source_repo=source_repo,
        scope_hash=scope_hash,
        source_generation=source_generation,
        source_manifest_hash=source_manifest_hash,
        cache_hit=cache_hit,
    )


def _doctor_result(output: dict[str, Any]) -> ViewRuntimeStatus:
    _reject_unknown_fields(output, _DOCTOR_FIELDS, "doctor result")
    status = _required_string(output, "status")
    git_version = _required_string(output, "git_version")
    if status != "ok":
        raise RuntimeError("view helper doctor returned invalid status")
    if not git_version.startswith("git version "):
        raise RuntimeError("view helper doctor returned invalid git version")
    return ViewRuntimeStatus(status=status, git_version=git_version)


def _reject_unknown_fields(
    output: dict[str, Any], allowed: frozenset[str], context: str
) -> None:
    unknown = sorted(set(output) - allowed)
    if unknown:
        raise RuntimeError(f"view helper returned unknown {context} field: {unknown[0]}")


def _required_string(output: dict[str, Any], field: str) -> str:
    value = output.get(field)
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"view helper returned invalid {field}")
    return value


def _optional_string(output: dict[str, Any], field: str) -> str | None:
    value = output.get(field)
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"view helper returned invalid {field}")
    return value


def _optional_int(output: dict[str, Any], field: str) -> int | None:
    value = output.get(field)
    if value is None:
        return None
    if not isinstance(value, int) or value < 0:
        raise RuntimeError(f"view helper returned invalid {field}")
    return value
