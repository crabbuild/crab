#!/usr/bin/env python3
"""Validate retained-evidence contracts for native NFS smoke wrappers."""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent


@dataclass(frozen=True)
class ScriptContract:
    platform: str
    path: Path
    required_groups: tuple[tuple[str, tuple[str, ...]], ...]
    forbidden_needles: tuple[str, ...] = ()
    repeated_needles: tuple[tuple[str, str, int], ...] = ()


SHARED_REPORT_CHECKS = (
    "mount_doctor",
    "initial_read",
    "native_read_benchmark",
    "writeback",
    "mount_list",
    "mount_status",
    "control_status",
    "unmount",
    "control_shutdown",
    "remount",
)


SHARED_ARTIFACTS = (
    "mount-list.json",
    "mount-doctor.json",
    "mount-status.json",
    "control-status.json",
    "native-read-benchmark.json",
    "writeback-check.json",
    "unmount-check.json",
    "control-shutdown.json",
    "remount-check.json",
)


SHARED_NATIVE_READ = (
    "nfs-native-read-benchmark",
    "native_sequential_read",
    "nfs_protocol_delta",
    "nfs_read_leases_delta",
    "nfs_vfs_delta",
    "nfs_hydration_delta",
    "requested_bytes_per_user_byte",
    "returned_bytes_per_user_byte",
    "read_rpcs_per_mib",
    "resolver_calls_avoided",
    "read_window_cache_hits",
)


SHARED_RUNTIME = (
    "server_bind_ms",
    "native_mount_ms",
    "startup_ms",
    "NFS read lease hits must be positive",
    "NFS read lease misses must be positive",
    "directory_pages",
    "write_journal",
    "total_sync_latency_ms",
)


SHARED_CONTROL = (
    "?token=<redacted>",
    "mount list --json",
    "mount status --mountpoint",
    "--live-only --json",
    "control_endpoint",
)


SHARED_WRITEBACK = (
    "hello_appended",
    "renamed_file_created",
    "exclusive_file_created",
    "gitdir_preserved",
    "gitdir_overwrite_rejected",
    "gitdir_rename_rejected",
    "removed_directory_absent",
)


SHARED_REMOUNT = (
    "hello_preserved",
    "renamed_file_preserved",
    "exclusive_file_preserved",
    "gitdir_preserved",
    "removed_directory_absent",
)


def shell_contract(
    *,
    platform: str,
    path: Path,
    helper_layout: tuple[str, ...],
    platform_probe: tuple[str, ...],
    report_suite: str,
    report_platform: str,
) -> ScriptContract:
    return ScriptContract(
        platform=platform,
        path=path,
        required_groups=(
            ("platform preflight", platform_probe),
            ("helper layout", helper_layout),
            ("retained report checks", SHARED_REPORT_CHECKS),
            ("retained artifacts", SHARED_ARTIFACTS),
            ("retained control redaction", ("redact_retained_control_endpoint_filter",) + SHARED_CONTROL),
            ("native read evidence", SHARED_NATIVE_READ),
            ("runtime evidence", SHARED_RUNTIME),
            ("POSIX writeback checks", SHARED_WRITEBACK + ("symlink_created",)),
            ("POSIX remount checks", ('"action": "remount"',) + SHARED_REMOUNT + ("symlink_preserved",)),
            (
                "report identity and verifier invocation",
                (
                    f'"suite": "{report_suite}"',
                    f'"platform": "{report_platform}"',
                    '"backend": "nfs"',
                    '"git_commit"',
                    "GIT_COMMIT",
                    "verify-nfs-smoke-report.py",
                    "--require-artifacts",
                    "--expected-git-commit",
                    "nfs-smoke-report.json",
                    "nfs_smoke_report=",
                ),
            ),
        ),
        repeated_needles=(
            ("mount lifecycle", "--backend nfs --no-refresh", 2),
            ("retained status sampling", "--live-only --json", 1),
        ),
    )


CONTRACTS: dict[str, ScriptContract] = {
    "linux": shell_contract(
        platform="linux",
        path=SCRIPT_DIR / "docker" / "run-mount-nfs-linux-smoke.sh",
        helper_layout=("ln -sf crab /src/target/debug/crab-nfs-mount",),
        platform_probe=("docker is required", "mount.nfs", "--cap-add SYS_ADMIN"),
        report_suite="mount-nfs-linux",
        report_platform="linux",
    ),
    "macos": shell_contract(
        platform="macos",
        path=SCRIPT_DIR / "run-mount-nfs-macos-smoke.sh",
        helper_layout=('cp "$CRAB_EXE" "$HELPER_EXE"',),
        platform_probe=("native macOS NFS smoke must run on macOS", "mount_nfs is required"),
        report_suite="mount-nfs-macos",
        report_platform="macos",
    ),
    "windows": ScriptContract(
        platform="windows",
        path=SCRIPT_DIR / "run-mount-nfs-windows-smoke.ps1",
        required_groups=(
            (
                "native Windows Client for NFS preflight",
                (
                    "native Windows NFS smoke must run on Windows",
                    'Resolve-SystemCommand "mount.exe"',
                    'Resolve-SystemCommand "umount.exe"',
                    "Windows Client for NFS command not found",
                ),
            ),
            (
                "build and helper identity",
                (
                    '@("build", "-p", "crab", "--bin", "crab", "--no-default-features", "--features", "nfs")',
                    '@("build", "-p", "crab", "--bin", "crab-nfs-mount", "--no-default-features")',
                    '"crab-version.txt"',
                    '"crab-nfs-mount-version.txt"',
                    "helper_version",
                ),
            ),
            (
                "retained report checks",
                SHARED_REPORT_CHECKS,
            ),
            (
                "retained artifacts",
                (
                    '$MountDoctorPath = Join-Path $RunRoot "mount-doctor.json"',
                    '$MountStatusPath = Join-Path $RunRoot "mount-status.json"',
                    '$ControlStatusPath = Join-Path $RunRoot "control-status.json"',
                    '$NativeReadBenchmarkPath = Join-Path $RunRoot "native-read-benchmark.json"',
                    '$WritebackCheckPath = Join-Path $RunRoot "writeback-check.json"',
                    '$UnmountCheckPath = Join-Path $RunRoot "unmount-check.json"',
                    '$ControlShutdownPath = Join-Path $RunRoot "control-shutdown.json"',
                    '$RemountCheckPath = Join-Path $RunRoot "remount-check.json"',
                    "mount_list = $mountListPath",
                    "mount_doctor = $MountDoctorPath",
                    "mount_status = $MountStatusPath",
                    "control_status = $ControlStatusPath",
                    "native_read_benchmark = $NativeReadBenchmarkPath",
                    "writeback_check = $WritebackCheckPath",
                    "unmount_check = $UnmountCheckPath",
                    "control_shutdown = $ControlShutdownPath",
                    "remount_check = $RemountCheckPath",
                ),
            ),
            (
                "retained control redaction",
                (
                    "function Redact-ControlEndpoint",
                    "?token=<redacted>",
                    "$entry.control_endpoint = (Redact-ControlEndpoint",
                    "$mountStatus.control_endpoint = (Redact-ControlEndpoint",
                    "$controlStatus.control_endpoint = (Redact-ControlEndpoint",
                    "$remountStatus.control_endpoint = (Redact-ControlEndpoint",
                    "control_endpoint = (Redact-ControlEndpoint -Endpoint",
                ),
            ),
            ("native read evidence", SHARED_NATIVE_READ),
            ("runtime evidence", SHARED_RUNTIME),
            ("portable Windows writeback checks", SHARED_WRITEBACK),
            ("portable Windows remount checks", ('action = "remount"',) + SHARED_REMOUNT),
            (
                "report identity and verifier invocation",
                (
                    'suite = "mount-nfs-windows"',
                    'platform = "windows"',
                    'backend = "nfs"',
                    "git_commit = $GitCommit",
                    "helper_version =",
                    "Invoke-PythonVerifier",
                    '"mount-nfs-windows"',
                    '"windows"',
                    '"--require-artifacts"',
                    '"--expected-git-commit"',
                    "$ExpectedGitCommit",
                    "verify-nfs-smoke-report.py",
                    "windows_nfs_mount_smoke=ok",
                ),
            ),
        ),
        forbidden_needles=("symlink_created", "symlink_preserved"),
        repeated_needles=(
            (
                "mount lifecycle",
                '@("mount", "--repo", $Source, "--mountpoint", $Drive, "--backend", "nfs", "--no-refresh")',
                2,
            ),
            ("retained control redaction", "control_endpoint = (Redact-ControlEndpoint -Endpoint", 3),
        ),
    ),
}


def check_text(contract: ScriptContract, text: str) -> list[str]:
    errors: list[str] = []
    for group, needles in contract.required_groups:
        for needle in needles:
            if needle not in text:
                errors.append(f"{contract.platform} {group}: missing {needle!r}")

    for group, needle, minimum in contract.repeated_needles:
        actual = text.count(needle)
        if actual < minimum:
            errors.append(
                f"{contract.platform} {group}: expected at least {minimum} occurrences of {needle!r}, found {actual}"
            )

    for needle in contract.forbidden_needles:
        if needle in text:
            errors.append(f"{contract.platform} contract: forbidden check {needle!r}")

    return errors


def read_contract(contract: ScriptContract) -> tuple[str | None, list[str]]:
    try:
        return contract.path.read_text(encoding="utf-8"), []
    except OSError as error:
        return None, [f"{contract.platform}: {contract.path}: {error}"]


def check_contract(contract: ScriptContract) -> list[str]:
    text, errors = read_contract(contract)
    if text is None:
        return errors
    return check_text(contract, text)


def self_test() -> int:
    all_errors: list[str] = []
    for contract in CONTRACTS.values():
        text, errors = read_contract(contract)
        if text is None:
            all_errors.extend(errors)
            continue

        errors = check_text(contract, text)
        if errors:
            all_errors.extend(errors)
            continue

        first_group = contract.required_groups[0][1]
        mutated = text.replace(first_group[0], "", 1)
        errors = check_text(contract, mutated)
        if not any(first_group[0] in error for error in errors):
            all_errors.append(f"{contract.platform} self-test did not catch missing platform preflight")

        mutated = text.replace("native_read_benchmark", "")
        errors = check_text(contract, mutated)
        if not any("native_read_benchmark" in error for error in errors):
            all_errors.append(f"{contract.platform} self-test did not catch missing native-read check")

        if contract.platform == "windows":
            mutated = text + "\nsymlink_created = $true\n"
            errors = check_text(contract, mutated)
            if not any("symlink_created" in error for error in errors):
                all_errors.append("windows self-test did not catch POSIX-only Windows check")
        else:
            mutated = text.replace("symlink_preserved", "", 1)
            errors = check_text(contract, mutated)
            if not any("symlink_preserved" in error for error in errors):
                all_errors.append(f"{contract.platform} self-test did not catch missing POSIX remount check")

    if all_errors:
        for error in all_errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    print("ok: native NFS smoke script contracts self-test passed")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--platform",
        action="append",
        choices=tuple(CONTRACTS),
        help="platform contract to check; may be passed more than once",
    )
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return self_test()

    platforms = args.platform or list(CONTRACTS)
    errors: list[str] = []
    for platform in platforms:
        errors.extend(check_contract(CONTRACTS[platform]))

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    checked = ", ".join(platforms)
    print(f"ok: native NFS smoke script contracts verified: {checked}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
