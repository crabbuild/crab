#!/usr/bin/env python3
"""Validate hosted release archive and Homebrew install contracts."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


FORBIDDEN_CLI_ARCHIVE_BINARIES = (
    "crab-cache-server",
    "crab-auth-receive",
    "crab-auth-view",
)


@dataclass(frozen=True)
class TextCheck:
    label: str
    path: Path
    contains: tuple[str, ...] = ()
    excludes: tuple[str, ...] = ()


def repo_root() -> Path:
    return Path(__file__).resolve().parents[3]


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def check_text(check: TextCheck) -> list[str]:
    errors: list[str] = []
    if not check.path.is_file():
        return [f"{check.label}: missing file {check.path}"]

    text = read_text(check.path)
    for needle in check.contains:
        if needle not in text:
            errors.append(f"{check.label}: expected {needle!r} in {check.path}")
    for needle in check.excludes:
        if needle in text:
            errors.append(f"{check.label}: unexpected {needle!r} in {check.path}")
    return errors


def function_body(text: str, name: str) -> str:
    pattern = re.compile(rf"^{re.escape(name)}\(\) \{{\n(?P<body>.*?)\n\}}", re.MULTILINE | re.DOTALL)
    match = pattern.search(text)
    if match is None:
        raise ValueError(f"missing shell function {name}")
    return match.group("body")


def workflow_step_run(text: str, step_name: str) -> str:
    marker = f"      - name: {step_name}\n"
    start = text.find(marker)
    if start == -1:
        raise ValueError(f"missing workflow step {step_name}")
    run_marker = "        run: |\n"
    run_start = text.find(run_marker, start)
    if run_start == -1:
        raise ValueError(f"missing run block for workflow step {step_name}")
    body_start = run_start + len(run_marker)
    next_step = text.find("\n      - ", body_start)
    if next_step == -1:
        return text[body_start:]
    return text[body_start:next_step]


def heredoc_block(text: str, marker: str, terminator: str) -> str:
    lines = text.splitlines()
    start_index = next((index for index, line in enumerate(lines) if marker in line), None)
    if start_index is None:
        raise ValueError(f"missing heredoc marker {marker}")
    body: list[str] = []
    for line in lines[start_index + 1 :]:
        if line.strip() == terminator:
            return "\n".join(body)
        body.append(line)
    raise ValueError(f"missing {terminator} terminator for {marker}")


def make_target_body(text: str, target: str) -> str:
    pattern = re.compile(
        rf"^{re.escape(target)}:\n(?P<body>(?:\t.*\n|\n)*)",
        re.MULTILINE,
    )
    match = pattern.search(text)
    if match is None:
        raise ValueError(f"missing make target {target}")
    return match.group("body")


def check_makefile_nfs_feature_gate(root: Path) -> list[str]:
    path = root / "crab" / "Makefile"
    text = read_text(path)
    errors: list[str] = []
    try:
        body = make_target_body(text, "nfs-feature-gate")
    except ValueError as error:
        return [f"Makefile: {error}"]

    for needle in (
        "$(MAKE) --no-print-directory nfs-smoke-script-check",
        "$(PYTHON) scripts/release/check-release-archive-contents.py",
        "$(PYTHON) scripts/verify-nfs-smoke-report.py self-test",
    ):
        if needle not in body:
            errors.append(f"Makefile nfs-feature-gate: expected {needle!r}")
    return errors


def check_release_script(root: Path) -> list[str]:
    path = root / "crab" / "scripts" / "release" / "release.sh"
    text = read_text(path)
    errors: list[str] = []

    try:
        unix = function_body(text, "package_unix_binaries")
    except ValueError as error:
        errors.append(f"release.sh: {error}")
    else:
        expected = 'tar -czf "$archive" -C "$(dirname "$bin_path")" "$(basename "$bin_path")" "$(basename "$fuse_path")" "$(basename "$nfs_path")"'
        if expected not in unix:
            errors.append("release.sh: unix archives must contain crab, crab-fuse-mount, and crab-nfs-mount only")

    try:
        darwin = function_body(text, "package_darwin_binaries")
    except ValueError as error:
        errors.append(f"release.sh: {error}")
    else:
        expected = 'tar -czf "$archive" -C "$(dirname "$bin_path")" "$(basename "$bin_path")" "$(basename "$fuse_path")" "$(basename "$nfs_path")"'
        if expected not in darwin:
            errors.append("release.sh: Darwin archives must contain crab, crab-fuse-mount, and crab-nfs-mount only")

    try:
        windows = function_body(text, "package_windows_binary")
    except ValueError as error:
        errors.append(f"release.sh: {error}")
    else:
        expected = '(cd "$(dirname "$bin_path")" && zip -q "$archive" "$(basename "$bin_path")" "$(basename "$nfs_path")")'
        if expected not in windows:
            errors.append("release.sh: Windows archives must contain crab.exe and crab-nfs-mount.exe only")

    for needle in (
        "-p crab --bin crab",
        'TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_DIR/target}"',
        'local bin_path="$TARGET_DIR/$triple/release/crab"',
        'local fuse_path="$TARGET_DIR/$triple/release/crab-fuse-mount"',
        'local nfs_path="$TARGET_DIR/$triple/release/crab-nfs-mount"',
        'local bin_path="$TARGET_DIR/$triple/release/crab.exe"',
        'local nfs_path="$TARGET_DIR/$triple/release/crab-nfs-mount.exe"',
        'package_darwin_binaries "$bin_path" "$fuse_path" "$nfs_path" "$archive"',
        'ln -sf "$(basename "$bin_path")" "$nfs_path"',
        '--mount "type=bind,source=$WORKSPACE_DIR,target=/workspace,readonly"',
        '--mount "type=bind,source=$DIST_DIR,target=/dist"',
        '-e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc"',
        '-e "CC_aarch64_unknown_linux_gnu=cc"',
        'docker_job_args=(-e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-12}")',
        '${docker_job_args[@]+"${docker_job_args[@]}"}',
        'cp "$binary_dir/crab" /dist/crab-fuse-mount',
        'cp "$binary_dir/crab" /dist/crab',
        'ln -sf crab "$DIST_DIR/crab-nfs-mount"',
        "-p crab --bin crab-nfs-mount",
        "--no-default-features --features simd-accel,tier,watch,nfs,gix-pathmatch",
    ):
        if needle not in text:
            errors.append(f"release.sh: expected {needle!r}")

    for forbidden in FORBIDDEN_CLI_ARCHIVE_BINARIES:
        if forbidden in text:
            errors.append(f"release.sh: CLI release archive script must not package {forbidden}")
    for forbidden in ("gh release create", "gh release upload", "RELEASE_REPO"):
        if forbidden in text:
            errors.append(f"release.sh: local builds must not publish releases: {forbidden!r}")

    return errors


def check_release_workflow(root: Path) -> list[str]:
    path = root / ".github" / "workflows" / "release.yml"
    text = read_text(path)
    errors: list[str] = []

    try:
        package_step = workflow_step_run(text, "Package archive")
    except ValueError as error:
        errors.append(f"release.yml: {error}")
    else:
        for needle in (
            "cp target/${{ matrix.target }}/release/crab.exe dist/",
            "cp target/${{ matrix.target }}/release/crab-nfs-mount.exe dist/",
            "Compress-Archive -LiteralPath 'dist/crab.exe','dist/crab-nfs-mount.exe'",
            "-DestinationPath 'dist/${{ matrix.archive }}' -Force",
            "rm dist/crab.exe dist/crab-nfs-mount.exe",
            "cp target/${{ matrix.target }}/release/crab-fuse-mount dist/",
            "(cd dist && ln -sf crab crab-nfs-mount)",
            "cp target/${{ matrix.target }}/release/crab dist/",
            'tar czf "dist/${{ matrix.archive }}" -C dist crab crab-fuse-mount crab-nfs-mount',
            "rm dist/crab dist/crab-fuse-mount dist/crab-nfs-mount",
        ):
            if needle not in package_step:
                errors.append(f"release.yml Package archive: expected {needle!r}")
        for forbidden in FORBIDDEN_CLI_ARCHIVE_BINARIES:
            if forbidden in package_step:
                errors.append(f"release.yml Package archive: must not package {forbidden}")

    try:
        build_step = workflow_step_run(text, "Build")
    except ValueError as error:
        errors.append(f"release.yml: {error}")
    else:
        for needle in (
            'no_fuse_features="simd-accel,tier,replication-s3-control-plane,replication-gcs-control-plane,replication-azure-control-plane,coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb,watch,nfs,gix-pathmatch"',
            "--no-default-features --features simd-accel,tier,watch,nfs,gix-pathmatch",
            '--no-default-features --features "$no_fuse_features"',
            "-p crab --bin crab-nfs-mount",
            "ln -sf crab target/${{ matrix.target }}/release/crab-nfs-mount",
            "cargo build --release --locked --target ${{ matrix.target }}",
        ):
            if needle not in build_step:
                errors.append(f"release.yml Build: expected {needle!r}")

        for forbidden in ("cargo xwin build", "cross build"):
            if forbidden in build_step:
                errors.append(f"release.yml Build: unexpected {forbidden!r}")

    try:
        verify_step = workflow_step_run(text, "Verify archive layout")
    except ValueError as error:
        errors.append(f"release.yml: {error}")
    else:
        for needle in (
            "Expand-Archive -LiteralPath 'dist/${{ matrix.archive }}'",
            '"$extract_dir/crab.exe" version',
            '"$extract_dir/crab.exe" --help',
            '"$extract_dir/crab" version',
            '"$extract_dir/crab" --help',
            'readlink "$extract_dir/crab-nfs-mount"',
        ):
            if needle not in verify_step:
                errors.append(f"release.yml Verify archive layout: expected {needle!r}")

    if 'crab/scripts/release/update-homebrew.sh "$TAG"' not in text:
        errors.append("release.yml: Homebrew publishing must use crab/scripts/release/update-homebrew.sh")

    for needle in (
        "os: macos-15",
        "os: macos-15-intel",
        "os: ubuntu-24.04",
        "os: ubuntu-24.04-arm",
        "os: windows-2025",
        "os: windows-11-arm",
        "archive: crab-darwin-aarch64.tar.gz",
        "archive: crab-darwin-x86_64.tar.gz",
        "archive: crab-linux-x86_64.tar.gz",
        "archive: crab-linux-aarch64.tar.gz",
        "archive: crab-windows-x86_64.zip",
        "archive: crab-windows-aarch64.zip",
        "attestations: write",
        "id-token: write",
        "actions/attest-build-provenance@977bb373ede98d70efdf65b84cb5f73e068dcc2a # v3",
        "subject-path: dist/${{ matrix.archive }}",
        "path: dist/${{ matrix.archive }}",
        "if-no-files-found: error",
        "sha256sum crab-*.* > SHA256SUMS.txt",
        "FILES=(dist/crab-*.tar.gz dist/crab-*.zip dist/SHA256SUMS.txt)",
        "RELEASE_REPO: ${{ github.repository }}",
        "contents: write",
        "Publish GitHub release",
        "GH_TOKEN: ${{ github.token }}",
        "--verify-tag",
        "--fail-on-no-commits",
        '--notes-file "$notes_file"',
        "cargo test -p crab --test workflow_migration --no-default-features --features simd-accel,tier,watch --locked",
        "require_nfs_evidence:",
        "require_cloud_evidence:",
        "nfs_release_verify_args:",
        "vars.CRAB_RELEASE_REQUIRE_NFS_EVIDENCE == 'true'",
        "vars.CRAB_RELEASE_REQUIRE_CLOUD_EVIDENCE == 'true'",
        "CRAB_NFS_RELEASE_VERIFY_ARGS",
        "nfs_evidence_run_id:",
        "NFS_RELEASE_EVIDENCE_RUN_ID",
        "NFS_RELEASE_VERIFY_ARGS",
        "nfs-native-evidence-gate:",
        "NFS Mount Evidence",
        "NFS_RELEASE_EXPECTED_RUN_SUFFIX=${run_id}-${attempt}",
        "NFS_RELEASE_EXPECTED_GIT_COMMIT=${release_sha}",
        'gh run download "$run_id"',
        '--pattern "nfs-smoke-*-${run_id}-${attempt}"',
        "make -C crab nfs-release-gate",
        "NFS_RELEASE_EVIDENCE_DIR=../nfs-release-evidence",
        'NFS_RELEASE_EXPECTED_GIT_COMMIT="${NFS_RELEASE_EXPECTED_GIT_COMMIT}"',
        "NFS_RELEASE_SUMMARY_OUTPUT=../nfs-release-smoke-summary.json",
        'NFS_RELEASE_VERIFY_ARGS="${NFS_RELEASE_VERIFY_ARGS}"',
        "scripts/nfs-evidence-summary.py smoke",
        "nfs-release-evidence-gate-${{ github.run_id }}-${{ github.run_attempt }}",
        "needs.nfs-native-evidence-gate.result == 'success'",
    ):
        if needle not in text:
            errors.append(f"release.yml: expected {needle!r}")

    for forbidden in (
        "os: macos-13",
        "cargo install cross",
        "cargo install cargo-xwin",
        "cargo xwin build",
        "cross build",
        "CRAB_RELEASE_GITHUB_TOKEN",
        "crabbuild/crab-release",
        "--clobber",
    ):
        if forbidden in text:
            errors.append(f"release.yml: unexpected {forbidden!r}")

    return errors


def check_homebrew_formula_text(label: str, text: str) -> list[str]:
    errors: list[str] = []
    for needle in (
        'bin.install "crab"',
        'bin.install "crab-fuse-mount"',
        'bin.install_symlink "crab" => "crab-nfs-mount"',
        'bin.install_symlink "crab" => "git-remote-crab"',
        'shell_output("#{bin}/crab version")',
    ):
        if needle not in text:
            errors.append(f"{label}: expected {needle!r}")
    for forbidden in FORBIDDEN_CLI_ARCHIVE_BINARIES:
        if forbidden in text:
            errors.append(f"{label}: must not install {forbidden}")
    return errors


def check_homebrew_script(
    root: Path,
    relative: str,
    marker: str,
    terminator: str,
) -> list[str]:
    path = root / relative
    text = read_text(path)
    try:
        formula = heredoc_block(text, marker, terminator)
    except ValueError as error:
        return [f"{relative}: {error}"]
    return check_homebrew_formula_text(relative, formula)


def checks(root: Path) -> list[TextCheck]:
    return [
        TextCheck(
            "version bump keeps all shipped product manifests aligned",
            root / "crab" / "scripts" / "release" / "bump-version.sh",
            contains=(
                "PRODUCT_MANIFESTS=(",
                '"$CRAB_DIR/Cargo.toml"',
                '"$WORKSPACE_DIR/crates/crab-auth-server/Cargo.toml"',
                '"$WORKSPACE_DIR/crates/crab-cache-server/Cargo.toml"',
                "cargo metadata --format-version 1 --no-deps",
                "restore_on_error",
            ),
            excludes=(),
        ),
        TextCheck(
            "public release badge follows the source repository",
            root / "README.md",
            contains=(
                "https://github.com/crabbuild/crab-oss/releases/latest",
                "github/v/release/crabbuild/crab-oss",
            ),
            excludes=("github/v/release/crabbuild/crab-release",),
        ),
        TextCheck(
            "self-updater follows the source repository release API",
            root / "crab" / "src" / "cmd" / "update.rs",
            contains=(
                "https://api.github.com/repos/crabbuild/crab-oss/releases/latest",
                "No crab release is available from crabbuild/crab-oss.",
            ),
            excludes=("crabbuild/crab-release",),
        ),
        TextCheck(
            "Homebrew updater follows source repository releases",
            root / "crab" / "scripts" / "release" / "update-homebrew.sh",
            contains=('RELEASE_REPO="${RELEASE_REPO:-crabbuild/crab-oss}"',),
            excludes=("crabbuild/crab-release",),
        ),
        TextCheck(
            "release profile strips symbols from published binaries",
            root / "Cargo.toml",
            contains=(
                "[profile.release]",
                "lto = false",
                "debug = 0",
                'strip = "symbols"',
            ),
            excludes=('strip = "debuginfo"',),
        ),
        TextCheck(
            "local release Dockerfiles export CLI and FUSE mount helper",
            root / "crab" / "scripts" / "release" / "docker" / "Dockerfile.linux-x86_64",
            contains=(
                "cp target/release/crab /crab",
                "cp target/release/crab /crab-fuse-mount",
                '--no-default-features --features "$CRAB_CLI_FEATURES_WITH_FUSE"',
                '--no-default-features --features "$CRAB_CLI_FEATURES_NO_FUSE"',
            ),
            excludes=(*FORBIDDEN_CLI_ARCHIVE_BINARIES, "cp target/release/crab /crab-nfs-mount"),
        ),
        TextCheck(
            "local release Dockerfiles export CLI and FUSE mount helper",
            root / "crab" / "scripts" / "release" / "docker" / "Dockerfile.linux-aarch64",
            contains=(
                "cp target/release/crab /crab",
                "cp target/release/crab /crab-fuse-mount",
                '--no-default-features --features "$CRAB_CLI_FEATURES_WITH_FUSE"',
                '--no-default-features --features "$CRAB_CLI_FEATURES_NO_FUSE"',
            ),
            excludes=(*FORBIDDEN_CLI_ARCHIVE_BINARIES, "cp target/release/crab /crab-nfs-mount"),
        ),
        TextCheck(
            "POSIX installer targets release repo and verifies archives",
            root / "packages" / "web" / "public" / "install.sh",
            contains=(
                'REPO="crabbuild/crab-oss"',
                "verify_checksum",
                "verify_tarball_layout",
                "crab-fuse-mount",
                "crab-nfs-mount",
                "Expected root-level crab, crab-fuse-mount, and crab-nfs-mount entries.",
                'ln -sf "crab" "$INSTALL_DIR/crab-nfs-mount"',
                'ln -sf "$INSTALL_DIR/crab" "$INSTALL_DIR/git-remote-crab"',
            ),
            excludes=("CrabBuild/crab-release", "crabbuild/crab-release"),
        ),
        TextCheck(
            "PowerShell installer targets release repo and installs helper exe",
            root / "packages" / "web" / "public" / "install.ps1",
            contains=(
                '$Repo = "crabbuild/crab-oss"',
                "Verify-Checksum",
                "Verify-ZipLayout",
                '"crab-nfs-mount.exe"',
                '"git-remote-crab.exe"',
            ),
            excludes=("CrabBuild/crab-release", "crabbuild/crab-release", "Created wrapper: git-remote-crab.cmd"),
        ),
        TextCheck(
            "local install-layout verifier covers mount helpers",
            root / "crab" / "scripts" / "release" / "check-install-layout.py",
            contains=(
                '"crab-nfs-mount"',
                '"crab-fuse-mount"',
                "--nfs-mount-bin",
                "--fuse-mount-bin",
                "--bin",
                "CRAB_CLI_FEATURES_NO_FUSE",
                "CRAB_CLI_FEATURES_WITH_FUSE",
                "expected symlink target 'crab'",
            ),
            excludes=(),
        ),
        TextCheck(
            "Windows NFS smoke builds the helper launcher",
            root / "crab" / "scripts" / "run-mount-nfs-windows-smoke.ps1",
            contains=(
                '"crab-nfs-mount"',
                "cargo-build-nfs-helper.log",
                "$HelperExe",
                "mount status --mountpoint",
                "--live-only --json",
                "crab mount status --live-only --json failed",
                "control_status",
                "control_shutdown",
                "control-status.json",
                "control-shutdown.json",
                "mount-doctor.json",
                "writeback-check.json",
                "unmount-check.json",
                "remount-check.json",
                "nfs_runtime.lifecycle",
                "server_bind_ms",
                "read_leases",
                "NFS read lease hits must be positive",
                "NFS read lease misses must be positive",
                "directory_pages",
                "write_journal",
                "total_sync_latency_ms",
                "native_read_benchmark",
                "nfs-native-read-benchmark",
                "nfs_protocol_delta",
                "nfs_read_leases_delta",
                "nfs_vfs_delta",
                "nfs_hydration_delta",
                "Get-NfsRuntimeSnapshot",
                "resolver_calls_avoided",
                "read_window_cache_hits",
                "read_requested_bytes",
                "requested_bytes_per_user_byte",
                "read_rpcs_per_mib",
                "git_commit",
                "$GitCommit",
                "gitdir_overwrite_rejected",
                "gitdir_rename_rejected",
                "verify-nfs-smoke-report.py",
                "verify-nfs-smoke-report.log",
                "--require-artifacts",
                "mount-nfs-windows",
                "nfs-smoke-report.json",
                "nfs_smoke_report=",
            ),
            excludes=("Copy-Item -LiteralPath $CrabExe -Destination $HelperExe",),
        ),
        TextCheck(
            "Linux NFS smoke uses the POSIX helper symlink layout",
            root / "crab" / "scripts" / "docker" / "run-mount-nfs-linux-smoke.sh",
            contains=(
                "ln -sf crab /src/target/debug/crab-nfs-mount",
                "mount status --mountpoint",
                "--live-only --json",
                "control_status",
                "control_shutdown",
                "control-status.json",
                "control-shutdown.json",
                "mount-doctor.json",
                "writeback-check.json",
                "unmount-check.json",
                "remount-check.json",
                'runtime.get("lifecycle")',
                "server_bind_ms",
                'runtime.get("read_leases")',
                "NFS read lease hits must be positive",
                "NFS read lease misses must be positive",
                'runtime.get("directory_pages")',
                'runtime.get("write_journal")',
                "total_sync_latency_ms",
                "native_read_benchmark",
                "nfs-native-read-benchmark",
                "nfs_protocol_delta",
                "nfs_read_leases_delta",
                "nfs_vfs_delta",
                "nfs_hydration_delta",
                "runtime_snapshot",
                "resolver_calls_avoided",
                "read_window_cache_hits",
                "read_requested_bytes",
                "requested_bytes_per_user_byte",
                "read_rpcs_per_mib",
                "git_commit",
                "GIT_COMMIT",
                "symlink_created",
                "symlink_preserved",
                "gitdir_overwrite_rejected",
                "gitdir_rename_rejected",
                "nfs-smoke-report.json",
                "verify-nfs-smoke-report.py",
                "nfs_smoke_report=",
            ),
            excludes=("cp /src/target/debug/crab /src/target/debug/crab-nfs-mount",),
        ),
        TextCheck(
            "macOS NFS smoke emits retained JSON evidence",
            root / "crab" / "scripts" / "run-mount-nfs-macos-smoke.sh",
            contains=(
                "mount status --mountpoint",
                "--live-only --json",
                "control_status",
                "control_shutdown",
                "control-status.json",
                "control-shutdown.json",
                "mount-doctor.json",
                "writeback-check.json",
                "unmount-check.json",
                "remount-check.json",
                'runtime.get("lifecycle")',
                "server_bind_ms",
                'runtime.get("read_leases")',
                "NFS read lease hits must be positive",
                "NFS read lease misses must be positive",
                'runtime.get("directory_pages")',
                'runtime.get("write_journal")',
                "total_sync_latency_ms",
                "native_read_benchmark",
                "nfs-native-read-benchmark",
                "nfs_protocol_delta",
                "nfs_read_leases_delta",
                "nfs_vfs_delta",
                "nfs_hydration_delta",
                "runtime_snapshot",
                "resolver_calls_avoided",
                "read_window_cache_hits",
                "read_requested_bytes",
                "requested_bytes_per_user_byte",
                "read_rpcs_per_mib",
                "git_commit",
                "GIT_COMMIT",
                "symlink_created",
                "symlink_preserved",
                "gitdir_overwrite_rejected",
                "gitdir_rename_rejected",
                "nfs-smoke-report.json",
                "verify-nfs-smoke-report.py",
                "nfs_smoke_report=",
            ),
            excludes=(),
        ),
        TextCheck(
            "macOS NFS RustFS smoke covers large repository contracts",
            root / "crab" / "scripts" / "run-mount-large-macos-rustfs-smoke.sh",
            contains=(
                'BACKEND="${CRAB_MOUNT_MACOS_BACKEND:-fuse}"',
                'DIRECTORY_ENTRIES="${CRAB_MOUNT_MACOS_DIRECTORY_ENTRIES:-0}"',
                'EXTERNAL_ENDPOINT="${CRAB_MOUNT_MACOS_ENDPOINT_URL:-}"',
                'mount doctor --backend "$BACKEND"',
                'mount --repo "$REMOTE_URL" --mountpoint "$RO" --ref main --backend "$BACKEND"',
                'mount --repo "$REMOTE_URL" --mountpoint "$RW" --ref main --backend "$BACKEND"',
                'mount commit --mountpoint "$RW"',
                '"seed_dedup": json.loads((root / "seed-dedup.json").read_text())',
                '"large_directory_entries": int(directory_entries)',
                'cmp "$RUN_ROOT/expected-new-large.sha256" "$RUN_ROOT/clone-new-large.sha256"',
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS proof gates are exposed through Makefile",
            root / "crab" / "Makefile",
            contains=(
                "nfs-smoke-report-verify",
                "nfs-smoke-report-verify-dir",
                "nfs-smoke-report-compare",
                "nfs-smoke-report-compare-dir",
                "nfs-smoke-report-verify-self-test",
                "nfs-smoke-script-check",
                "nfs-feature-gate",
                "mount-large-macos-nfs-rustfs-smoke",
                "nfs-threshold-suggestions",
                "nfs-release-gate",
                "nfs-release-evidence-ci",
                "nfs-release-evidence-dispatch-self-test",
                "nfs-read-path-bench-report-verify",
                "nfs-read-path-bench-report-compare",
                "NFS_READ_PATH_BENCH_BASELINE_REPORT ?=",
                "NFS_READ_PATH_BENCH_COMPARE_ARGS ?=",
                "NFS_READ_PATH_BENCH_COMPARE_OUTPUT ?=",
                "NFS_READ_PATH_BENCH_EXPECTED_GIT_COMMIT ?=",
                "NFS_READ_PATH_BENCH_EXPECTED_RUN_ID ?=",
                "NFS_READ_PATH_BENCH_REPORT ?=",
                "NFS_READ_PATH_BENCH_VERIFY_ARGS ?=",
                "NFS_READ_PATH_BENCH_VERIFY_ARGS",
                "NFS_SMOKE_BASELINE_REPORT ?=",
                "NFS_SMOKE_BASELINE_REPORT_DIR ?=",
                "NFS_SMOKE_COMPARE_ARGS ?=",
                "NFS_SMOKE_COMPARE_OUTPUT ?=",
                "NFS_SMOKE_COMPARE_SUMMARY ?=",
                "NFS_SMOKE_REPORT ?=",
                "NFS_SMOKE_REPORT_DIR ?=",
                "NFS_SMOKE_VERIFY_ARGS ?=",
                "NFS_SMOKE_VERIFY_SUMMARY ?=",
                "NFS_SMOKE_EXPECTED_RUN_SUFFIX ?=",
                "NFS_SMOKE_VERIFY_ARGS",
                "NFS_THRESHOLD_BENCHMARK_REPORT ?=",
                "NFS_THRESHOLD_BENCHMARK_REPORTS ?=",
                "NFS_THRESHOLD_BENCHMARK_DIR ?=",
                "NFS_THRESHOLD_BENCHMARK_DIRS ?=",
                "NFS_THRESHOLD_SMOKE_SUMMARY ?=",
                "NFS_THRESHOLD_SMOKE_SUMMARIES ?=",
                "NFS_THRESHOLD_SMOKE_DIR ?=",
                "NFS_THRESHOLD_SMOKE_DIRS ?=",
                "NFS_THRESHOLD_SUGGESTION_OUTPUT ?=",
                "NFS_THRESHOLD_SUGGESTION_JSON ?=",
                "NFS_THRESHOLD_BENCHMARK_MARGIN_PCT ?=",
                "NFS_THRESHOLD_SMOKE_MARGIN_PCT ?=",
                "NFS_THRESHOLD_BENCHMARK_REGRESSION_PCT ?=",
                "NFS_THRESHOLD_SMOKE_REGRESSION_PCT ?=",
                "NFS_THRESHOLD_MIN_BENCHMARK_REPORTS ?= 1",
                "NFS_THRESHOLD_MIN_SMOKE_SUMMARIES ?= 1",
                "NFS_RELEASE_EVIDENCE_DIR ?=",
                "NFS_RELEASE_EVIDENCE_RUN_ID ?=",
                "NFS_RELEASE_EVIDENCE_REF ?=",
                "NFS_RELEASE_EVIDENCE_WAIT ?= 0",
                "NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS ?= 7200",
                "NFS_RELEASE_EVIDENCE_POLL_SECS ?= 30",
                "NFS_RELEASE_EVIDENCE_OUTPUT ?=",
                "NFS_RELEASE_EXPECTED_RUN_SUFFIX ?=",
                "NFS_RELEASE_EXPECTED_GIT_COMMIT ?=",
                "NFS_RELEASE_SUMMARY_OUTPUT ?=",
                "NFS_RELEASE_VERIFY_ARGS ?=",
                "NFS_RELEASE_REQUIRE_EVIDENCE ?= 1",
                "NFS_RELEASE_REQUIRE_EXPECTED_RUN_SUFFIX ?= 1",
                "NFS_RELEASE_REQUIRE_EXPECTED_GIT_COMMIT ?= 1",
                "NFS_RELEASE_EXPECTED_GIT_COMMIT is required for release-grade NFS evidence",
                "--expected-git-commit",
                "RELEASE_REQUIRE_NFS_EVIDENCE ?= 0",
                "RELEASE_REQUIRE_CLOUD_EVIDENCE ?= 0",
                "NFS_RELEASE_EVIDENCE_RUN_ID is required for make release-ci",
                "Run the NFS Mount Evidence workflow on the exact release commit",
                "NFS_RELEASE_EVIDENCE_WAIT",
                "NFS_RELEASE_EVIDENCE_OUTPUT",
                'set -- "$$@" -f "nfs_evidence_run_id=$(NFS_RELEASE_EVIDENCE_RUN_ID)"',
                'set -- "$$@" -f "nfs_release_verify_args=$(NFS_RELEASE_VERIFY_ARGS)"',
                '"require_nfs_evidence=$$require_nfs_evidence"',
                "bash -n scripts/docker/run-mount-nfs-linux-smoke.sh",
                "bash -n scripts/run-mount-nfs-macos-smoke.sh",
                "$(PYTHON) scripts/check-nfs-smoke-scripts.py --self-test",
                'command -v "$(POWERSHELL)"',
                '"$(POWERSHELL)" -NoLogo -NoProfile -NonInteractive -Command',
                "[System.Management.Automation.Language.Parser]::ParseFile",
                "$(POWERSHELL) not found; skipping Windows smoke PowerShell parse",
                "@$(MAKE) --no-print-directory nfs-smoke-script-check",
                "dispatch-nfs-release-evidence.sh",
                "scripts/release/dispatch-nfs-release-evidence.sh self-test",
                "nfs-evidence-summary-self-test",
                "scripts/nfs-evidence-summary.py self-test",
                "scripts/nfs-evidence-summary.py thresholds",
                "--min-benchmark-reports",
                "--min-smoke-summaries",
                "--expected-run-suffix $(NFS_RELEASE_EXPECTED_RUN_SUFFIX)",
                "--require-all-platforms",
                "check -p crab --bin crab --no-default-features --features nfs,gix-all",
                "test -p crab-vfs --features nfs,gix-facade",
                "--test prop_coordinator response_contains --features fuse",
                "scripts/verify-nfs-smoke-report.py self-test",
            ),
            excludes=(
                "release-macos-publish:",
                "release-publish-dist:",
                "RELEASE_BYPASS_EVIDENCE",
                "gh release create",
                "gh release upload",
            ),
        ),
        TextCheck(
            "Native NFS smoke script contract checker covers retained evidence",
            root / "crab" / "scripts" / "check-nfs-smoke-scripts.py",
            contains=(
                "ScriptContract",
                "CONTRACTS",
                "linux",
                "macos",
                "windows",
                "ln -sf crab /src/target/debug/crab-nfs-mount",
                'cp "$CRAB_EXE" "$HELPER_EXE"',
                "native Windows Client for NFS preflight",
                "build and helper identity",
                "retained artifacts",
                "retained control redaction",
                "native read evidence",
                "POSIX writeback checks",
                "POSIX remount checks",
                "portable Windows writeback checks",
                "portable Windows remount checks",
                "report identity and verifier invocation",
                "forbidden_needles",
                "symlink_created",
                "symlink_preserved",
                "expected at least",
                "retained control redaction",
                "self-test did not catch missing platform preflight",
                "self-test did not catch missing native-read check",
                "self-test did not catch missing POSIX remount check",
                "windows self-test did not catch POSIX-only Windows check",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS control endpoint diagnostics redact TCP tokens",
            root / "crates" / "crab-vfs" / "src" / "nfs_control.rs",
            contains=(
                "display_control_endpoint(endpoint)",
                "endpoint = %display_control_endpoint(&endpoint)",
                "tcp:{addr}?token=<redacted>",
                "tcp_endpoint_errors_redact_control_token",
                "tcp:not-an-addr?token=secret-token",
                "tcp:192.0.2.10:50000?token=secret-token",
                "tcp_control_server_requires_token_and_accepts_shutdown",
                "unauthorized NFS control request",
                "control_probe_replaces_stale_unix_socket_before_bind",
                "control_probe_replaces_empty_unix_placeholder_before_bind",
                "control_probe_creates_private_unix_socket_directory",
                "control_server_reports_status_accepts_shutdown_and_removes_socket",
                "assert_eq!(dir_mode, 0o700)",
                "assert!(!socket_path.exists())",
                "!malformed.contains(\"secret-token\")",
                "!non_loopback.contains(\"secret-token\")",
                "NFS control endpoint must be loopback: {}",
            ),
            excludes=(
                "warn!(endpoint, error = %error",
                "unsupported NFS control endpoint: {endpoint}",
                "NFS control endpoint must be loopback: {endpoint}",
            ),
        ),
        TextCheck(
            "NFS write journal shutdown drain is source-gated",
            root / "crates" / "crab-vfs" / "src" / "nfs.rs",
            contains=(
                "fn nfs_write_journal_sync_all_clears_successful_shutdown_drain()",
                "fn nfs_write_journal_sync_all_retains_failures_and_continues_shutdown_drain()",
                "journal.sync_all(&fixture.engine)",
                "sync_attempts, 2",
                "sync_successes, 1",
                "sync_failures, 1",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS adapter stale read retry is source-gated",
            root / "crates" / "crab-vfs" / "src" / "nfs.rs",
            contains=(
                "fn nfs_read_retries_stale_pooled_lease_once()",
                "self.read_leases.record_stale_retry();",
                "self.read_leases.evict(id);",
                "let retry_pin = self.open_read_lease_pin(id, path)?;",
                "stale_overlay_view_rejections, 1",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS adapter write stability protocol is source-gated",
            root / "crates" / "crab-vfs" / "src" / "nfs.rs",
            contains=(
                "fn nfs_unstable_write_stays_pending_until_commit()",
                "fn nfs_stable_write_syncs_and_clears_journal_before_reply()",
                "stable_how::UNSTABLE => stable_how::UNSTABLE",
                "stable_how::DATA_SYNC | stable_how::FILE_SYNC",
                "self.sync_journal_path(&path)?;",
                "<CrabNfsFs as NfsFileSystem>::commit",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS adapter mutation protocol state is source-gated",
            root / "crates" / "crab-vfs" / "src" / "nfs.rs",
            contains=(
                "fn nfs_remove_clears_derived_protocol_state_after_engine_mutation()",
                "fn nfs_rename_moves_derived_protocol_state_after_engine_mutation()",
                "fn nfs_failed_remove_keeps_derived_protocol_state_unchanged()",
                "fn nfs_failed_rename_keeps_derived_protocol_state_unchanged()",
                "<CrabNfsFs as NfsFileSystem>::remove",
                "<CrabNfsFs as NfsFileSystem>::rename",
                "let removed_ids = self.ids.remove_path(&path)?;",
                "let renamed_ids = self.ids.rename_path(&from_path, &to_path)?;",
                "self.read_leases.evict_many(removed_ids);",
                "self.read_leases",
                ".evict_many(renamed_ids.moved.into_iter().chain(renamed_ids.replaced));",
                "self.write_journal.remove_subtree(&path);",
                "self.write_journal.rename_subtree(&from_path, &to_path);",
                "directory_pages.stale_evictions, 3",
                "leases.evictions, 0",
                "directory_pages.stale_evictions, 0",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS smoke evidence artifacts redact retained TCP control tokens",
            root / "crab" / "scripts" / "verify-nfs-smoke-report.py",
            contains=(
                "check_retained_control_endpoints_redacted",
                'check_retained_control_endpoints_redacted(payload, errors, "artifacts.control_status")',
                'check_retained_control_endpoints_redacted(payload, errors, "artifacts.writeback_check")',
                'check_retained_control_endpoints_redacted(payload, errors, "artifacts.unmount_check")',
                'check_retained_control_endpoints_redacted(payload, errors, "artifacts.control_shutdown")',
                'check_retained_control_endpoints_redacted(payload, errors, "artifacts.remount_check")',
                "must redact TCP control token",
                "control_status.control_endpoint must redact TCP control token",
                "writeback_check.control_endpoint must redact TCP control token",
                "unmount_check.control_endpoint must redact TCP control token",
                "control_shutdown.control_endpoint must redact TCP control token",
                "remount_check.control_endpoint must redact TCP control token",
                "tcp:127.0.0.1:58123?token=secret-token",
                "self-test raw retained TCP control token was not rejected",
                "self-test raw control-status TCP control token was not rejected",
                "self-test raw writeback TCP control token was not rejected",
                "self-test raw unmount TCP control token was not rejected",
                "self-test raw control-shutdown TCP control token was not rejected",
                "self-test raw remount TCP control token was not rejected",
            ),
            excludes=(),
        ),
        TextCheck(
            "Linux NFS smoke redacts retained control endpoint artifacts",
            root / "crab" / "scripts" / "docker" / "run-mount-nfs-linux-smoke.sh",
            contains=(
                "redact_retained_control_endpoint_filter",
                "?token=<redacted>",
                "crab mount list --json | redact_retained_control_endpoint_filter",
                "crab mount status --mountpoint \"$MNT\" --json | redact_retained_control_endpoint_filter",
                "crab mount status --mountpoint \"$MNT\" --live-only --json | redact_retained_control_endpoint_filter",
            ),
            excludes=(),
        ),
        TextCheck(
            "macOS NFS smoke redacts retained control endpoint artifacts",
            root / "crab" / "scripts" / "run-mount-nfs-macos-smoke.sh",
            contains=(
                "redact_retained_control_endpoint_filter",
                "?token=<redacted>",
                "mount list --json | redact_retained_control_endpoint_filter",
                "mount status --mountpoint \"$MNT\" --json | redact_retained_control_endpoint_filter",
                "mount status --mountpoint \"$MNT\" --live-only --json | redact_retained_control_endpoint_filter",
            ),
            excludes=(),
        ),
        TextCheck(
            "Windows NFS smoke redacts retained control endpoint artifacts",
            root / "crab" / "scripts" / "run-mount-nfs-windows-smoke.ps1",
            contains=(
                "function Redact-ControlEndpoint",
                "?token=<redacted>",
                "$entry.control_endpoint = (Redact-ControlEndpoint",
                "$mountStatus.control_endpoint = (Redact-ControlEndpoint",
                "$controlStatus.control_endpoint = (Redact-ControlEndpoint",
                "$remountStatus.control_endpoint = (Redact-ControlEndpoint",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS release evidence dispatch helper forwards calibration inputs",
            root / "crab" / "scripts" / "release" / "dispatch-nfs-release-evidence.sh",
            contains=(
                "workflow run nfs-mount.yml",
                "NFS_RELEASE_EVIDENCE_REF",
                "NFS_RELEASE_EVIDENCE_WAIT",
                "NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS",
                "NFS_RELEASE_EVIDENCE_POLL_SECS",
                "NFS_RELEASE_EVIDENCE_OUTPUT",
                "nfs_smoke_baseline_run_id",
                "nfs_smoke_verify_args",
                "nfs_smoke_compare_args",
                "nfs_threshold_min_smoke_summaries",
                "gh run list --workflow nfs-mount.yml --limit 10",
                "gh run view",
                "NFS_RELEASE_EXPECTED_RUN_SUFFIX",
                "NFS_RELEASE_EVIDENCE_URL",
                "NFS_RELEASE_EVIDENCE_GIT_COMMIT",
                "write_env_assignment",
                "source \"$output_file\"",
                "nfs_smoke_baseline_run_id=22222",
                "nfs_smoke_verify_args=--max-native-read-rpcs-per-mib 99",
                "nfs_smoke_compare_args=--max-native-read-rpc-density-regression-pct 15",
                "nfs_threshold_min_smoke_summaries=4",
                "completed with conclusion 'failure'",
                "headSha changed unexpectedly",
                "failed NFS Mount Evidence run was not rejected",
                "mismatched NFS Mount Evidence headSha was not rejected",
                "Wrote release evidence variables",
                "Source it before release commands",
                "NFS release evidence dispatch self-test passed",
                "make release-ci NFS_RELEASE_EVIDENCE_RUN_ID=<run-id>",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS mount doctor exposes machine-readable preflight",
            root / "crab" / "src" / "cmd" / "mount.rs",
            contains=(
                "MountDoctorNfsPreflight",
                "MountDoctorAutoDecision",
                "nfs_preflight",
                "auto_decision",
                "selected_backend",
                "backend_available",
                "native_client_available",
                "mountpoint_ready",
                "loopback_bind_ready",
                "control_endpoint_ready",
                "privilege_ready",
                "next_action",
                "ensure_nfs_preflight_ready",
                "auto_backend_fallback_message",
                "NFS preflight failed; using FUSE for --backend=auto",
                "nfs_background_startup_failure_hint",
                "NFS preflight now reports",
                "resolve_nfs_background_helper",
                "check_nfs_background_helper",
                "background NFS mounts use the helper shipped with this crab binary",
                "crab-nfs-mount was not found next to crab or on PATH",
                "nfs_background_helper_check_requires_colocated_matching_helper",
                "nfs_background_preflight_failure_hint_preserves_actionable_blocker",
                "nfs_background_preflight_failure_hint_is_empty_when_ready",
                "auto_backend_fallback_message_names_nfs_blocker_and_next_action",
                "explicit_nfs_backend_prerequisites_use_preflight_report",
                "mount_doctor_payload_includes_machine_readable_auto_decision",
                "auto_decision_reports_fuse_fallback_with_nfs_next_action",
                "auto_decision_keeps_non_fallback_nfs_blockers_visible",
                "mount_doctor_payload_includes_machine_readable_nfs_preflight",
            ),
            excludes=(),
        ),
        TextCheck(
            "release workflow blocks packaging on NFS feature and native evidence gates",
            root / ".github" / "workflows" / "release.yml",
            contains=(
                "nfs-feature-gate:",
                "Run NFS feature gate",
                "make nfs-feature-gate",
                "Install macOS FUSE dependencies",
                'brew install --cask macfuse',
                'PKG_CONFIG_PATH=$(dirname "${fuse_pc}")',
                "nfs-native-evidence-gate:",
                "NFS Mount Evidence",
                "NFS_RELEASE_EVIDENCE_RUN_ID",
                "make -C crab nfs-release-gate",
                "NFS_RELEASE_EXPECTED_RUN_SUFFIX",
                "NFS_RELEASE_EXPECTED_GIT_COMMIT",
                "needs.nfs-feature-gate.result == 'success'",
                "needs.nfs-native-evidence-gate.result == 'success'",
            ),
            excludes=(),
        ),
        TextCheck(
            "architecture workflow reruns NFS evidence guards when evidence scripts change",
            root / ".github" / "workflows" / "architecture.yml",
            contains=(
                "make release-archive-contents-check",
                "crab/scripts/release/dispatch-nfs-release-evidence.sh",
                "crab/scripts/nfs-evidence-summary.py",
                "crab/scripts/nfs-read-path-bench-report.py",
                "crab/scripts/verify-nfs-smoke-report.py",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS mount workflow retains native smoke evidence",
            root / ".github" / "workflows" / "nfs-mount.yml",
            contains=(
                "name: NFS Mount Evidence",
                "make nfs-feature-gate",
                "NFS_THRESHOLD_MIN_SMOKE_SUMMARIES",
                "inputs.nfs_threshold_min_smoke_summaries",
                "crab/scripts/release/dispatch-nfs-release-evidence.sh",
                "crab/scripts/nfs-evidence-summary.py",
                "GITHUB_STEP_SUMMARY",
                "make mount-nfs-linux-smoke",
                "make mount-nfs-macos-smoke",
                "./scripts/run-mount-nfs-windows-smoke.ps1",
                "Install-WindowsFeature -Name NFS-Client",
                "actions/upload-artifact@v4",
                "actions/download-artifact@v4",
                "actions: read",
                "NFS_SMOKE_BASELINE_RUN_ID",
                "nfs_smoke_verify_args",
                "nfs_smoke_compare_args",
                "inputs.nfs_smoke_verify_args",
                "inputs.nfs_smoke_compare_args",
                "gh run download",
                "pattern: nfs-smoke-*-${{ github.run_id }}-${{ github.run_attempt }}",
                "make nfs-smoke-report-verify-dir",
                "make nfs-smoke-report-compare-dir",
                "NFS_SMOKE_REPORT_DIR=../nfs-smoke-retained",
                "NFS_SMOKE_BASELINE_REPORT_DIR=../nfs-smoke-baseline",
                "NFS_SMOKE_VERIFY_SUMMARY=../nfs-smoke-retained-summary.json",
                "NFS_SMOKE_EXPECTED_RUN_SUFFIX=${{ github.run_id }}-${{ github.run_attempt }}",
                "NFS_SMOKE_EXPECTED_GIT_COMMIT=${{ github.sha }}",
                "NFS_SMOKE_COMPARE_SUMMARY",
                "nfs-smoke-comparison-summary.json",
                "Summarize retained NFS smoke evidence",
                "scripts/nfs-evidence-summary.py smoke",
                "Suggest NFS threshold args",
                "scripts/nfs-evidence-summary.py thresholds",
                "--min-benchmark-reports 0",
                '--min-smoke-summaries "${NFS_THRESHOLD_MIN_SMOKE_SUMMARIES:-1}"',
                "nfs-threshold-suggestions.env",
                "nfs-threshold-suggestions.json",
                "nfs-smoke-retained-summary-${{ github.run_id }}-${{ github.run_attempt }}",
                "github.event_name != 'pull_request'",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS evidence summaries render GitHub markdown",
            root / "crab" / "scripts" / "nfs-evidence-summary.py",
            contains=(
                "NFS Read-Path Benchmark Evidence",
                "Retained Native NFS Smoke Evidence",
                "short_commit",
                "Commit",
                "Benchmark Trend",
                "Native Smoke Trend",
                "Suggested NFS threshold args",
                "--allow-missing",
                "--benchmark-margin-pct",
                "--min-benchmark-reports",
                "--min-smoke-summaries",
                "--benchmark-dir",
                "--smoke-dir",
                "min_benchmark_reports",
                "min_smoke_summaries",
                "nfs-read-path-bench-report.json",
                "nfs-smoke-retained-summary.json",
                'action="append"',
                "# Benchmark reports:",
                "# Smoke summaries:",
                "# Benchmark run attempts:",
                "# Smoke run attempts:",
                "Benchmark run suffixes",
                "Native smoke run suffixes",
                "Doctor",
                "Evidence commit",
                "Run suffix",
                "doctor_state",
                "Lease hits/MiB",
                "Lease misses/MiB",
                "Lease-miss regression",
                "read_lease_hits_per_mib",
                "read_lease_misses_per_mib",
                "NFS_SMOKE_VERIFY_ARGS",
                "--min-native-read-lease-hits-per-mib",
                "--max-native-read-lease-misses-per-mib",
                "--max-native-read-lease-hit-density-regression-pct",
                "--max-native-read-lease-miss-density-regression-pct",
                "self-test summary omitted",
                "self-test threshold suggestions omitted",
                "git_commit must match report rows",
                "run_id_suffix must match report rows",
                "missing release smoke suite(s)",
                "forged smoke header omitted",
                "split smoke evidence was release-shaped",
                "duplicate benchmark attempt was calibration-ready",
                "duplicate smoke attempt was calibration-ready",
                "NFS evidence summary self-test passed",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS read-path benchmark verifier supports ratio thresholds",
            root / "crab" / "scripts" / "nfs-read-path-bench-report.py",
            contains=(
                "lease_vs_path_mib_per_sec_ratio",
                "--min-pointer-sequential-lease-ratio",
                "--min-pointer-random-lease-ratio",
                "--min-overlay-modified-lease-ratio",
                "--max-throughput-regression-pct",
                "--max-ratio-regression-pct",
                "--expected-git-commit",
                "--expected-run-id",
                "git.commit must be a lowercase full Git object id",
                "expected git.commit",
                "run_id must start with",
                "run_id_suffix must match run_id",
                "benchmark git commit mismatch was not rejected",
                "benchmark run id mismatch was not rejected",
                "nfs-read-path-bench-comparison",
                "baseline_run_id",
                "current_run_id",
                "is below the configured threshold",
                "benchmark ratio threshold was not rejected",
                "benchmark trend regression was not rejected",
            ),
            excludes=(),
        ),
        TextCheck(
            "NFS smoke report verifier validates retained evidence shape",
            root / "crab" / "scripts" / "verify-nfs-smoke-report.py",
            contains=(
                "REQUIRED_CHECKS",
                "REQUIRED_ARTIFACTS",
                "mount_doctor",
                "mount_status",
                "control_status",
                "control_shutdown",
                "helper_version must match crab_version",
                "check_git_commit_field",
                "git_commit must be a lowercase full Git object id",
                "expected git_commit",
                "run_id_suffix",
                "mixed git_commit values in retained NFS smoke directory",
                "mismatched git commit was not rejected",
                "summary omitted evidence git commit",
                "summary omitted run suffix",
                "verify-dir summary omitted git commit",
                "mixed run_id suffixes in retained NFS smoke directory",
                "run_id must start with suite prefix",
                "mixed run suffixes were not rejected",
                "malformed run id was not rejected",
                "check_mount_list_artifact",
                "mount_list must include a running nfs entry",
                "artifacts.mount_doctor",
                "status must be ok or warn",
                "nfs helper version",
                "nfs helper layout",
                "checks entry {required} must be ok",
                "summary.ok must match check statuses",
                "warning_count must match warnings",
                "mount_doctor.mountpoint must match",
                "mount_doctor.nfs_preflight.ready must be true",
                "artifacts.mount_status.source",
                "artifacts.mount_status.pid",
                "artifacts.control_status",
                "check_control_status_artifact",
                "control_status.pid must match",
                "control_status.nfs_runtime must match",
                "mismatched control-status runtime was not rejected",
                "artifacts.writeback_check",
                "writeback_check.action must be writeback",
                "writeback_check.content_checks",
                "symlink_created",
                "symlink_preserved",
                "gitdir_overwrite_rejected",
                "gitdir_rename_rejected",
                "failed writeback content check was not rejected",
                "failed .git rename rejection check was not rejected",
                "failed Unix symlink writeback check was not rejected",
                "native_read_benchmark.mountpoint must match",
                "artifacts.unmount_check",
                "unmount_check.mounted_after must be false",
                "artifacts.control_shutdown",
                "check_control_shutdown_artifact",
                "control_shutdown.action must be control_shutdown",
                "artifacts.remount_check",
                "remount_check.mounted_after must be true",
                "remount_check.content_checks",
                "failed Unix symlink remount check was not rejected",
                "must match artifacts.mount_list entry",
                "nfs_runtime",
                "lifecycle",
                "server_bind_ms",
                "NFS_RUNTIME_PROTOCOL_COUNTERS",
                "readdirplus_materialized_entries",
                "missing READDIRPLUS runtime counter was not rejected",
                "read_leases",
                "read_leases.hits must be positive",
                "read_leases.misses must be positive",
                "missing read lease hit evidence was not rejected",
                "directory_pages",
                "check_vfs_runtime_status",
                "source_cache_max_entries",
                "resolver_calls_avoided",
                "base_pointer",
                "check_hydration_runtime_status",
                "read_window_cache_hits",
                "chunk_remote_bytes",
                "duplicate required NFS smoke suites",
                "duplicate suite was not rejected",
                "write_journal",
                "check_write_journal_runtime_status",
                "pending_paths",
                "oldest_dirty_age_secs",
                "paths_with_sync_errors",
                "poisoned",
                "last_sync_error",
                "total_sync_latency_ms",
                "missing write-journal pending count was not rejected",
                "inconsistent write-journal sync-error count was not rejected",
                "native_read_benchmark",
                "nfs-native-read-benchmark",
                "nfs_protocol_delta",
                "nfs_read_leases_delta",
                "nfs_vfs_delta",
                "nfs_hydration_delta",
                "read_requested_bytes",
                "resolver_calls_avoided",
                "read_window_cache_hits",
                "requested_bytes_per_user_byte",
                "read_rpcs_per_mib",
                "--max-native-read-rpcs-per-mib",
                "--min-native-read-lease-hits-per-mib",
                "--max-native-read-lease-misses-per-mib",
                "--max-native-read-throughput-regression-pct",
                "--max-native-read-rpc-density-regression-pct",
                "--max-native-read-vfs-call-density-regression-pct",
                "--max-native-read-lease-hit-density-regression-pct",
                "--max-native-read-lease-miss-density-regression-pct",
                "--max-native-read-resolver-avoidance-regression-pct",
                "--max-native-read-hydration-remote-byte-regression-pct",
                "vfs_read_calls_per_mib",
                "read_lease_hits_per_mib",
                "read_lease_misses_per_mib",
                "resolver_calls_avoided_per_mib",
                "hydration_remote_bytes_per_user_byte",
                "native_read_summary_for_report",
                "mount_doctor_summary_for_report",
                "verify-dir summary omitted mount doctor readiness",
                "verify-dir summary omitted native read metrics",
                "verify-dir summary omitted read-path deltas",
                "nfs-smoke-report-comparison",
                "nfs-smoke-report-directory-comparison",
                "verify-dir",
                "compare",
                "compare-dir",
                "--require-all-platforms",
                "--expected-run-suffix",
                "--expected-git-commit",
                "--summary-output",
                "resolve_artifact_path",
                "below the configured threshold",
                "def self_test",
                "retained artifact fallback failed",
                "verify-dir missing platform set was not rejected",
                "mismatched helper version was not rejected",
                "missing control-status artifact was not rejected",
                "mismatched control-status artifact was not rejected",
                "missing mount-list NFS entry was not rejected",
                "mount-status/list mismatch was not rejected",
                "mount-status/list PID mismatch was not rejected",
                "mounted-after shutdown artifact was not rejected",
                "failed remount content check was not rejected",
                "native smoke trend regression was not rejected",
                "native smoke workload mismatch was not rejected",
                "native smoke directory comparison failed",
                "native smoke missing baseline suite was not rejected",
                "missing VFS runtime evidence was not rejected",
                "missing hydration runtime evidence was not rejected",
                "missing native VFS delta was not rejected",
                "mismatched native hydration delta was not rejected",
                "mismatched protocol delta was not rejected",
                "missing native read lease hit delta was not rejected",
                "missing native read lease miss delta was not rejected",
                "mismatched run suffix was not rejected",
                "native read lease-hit threshold violation was not rejected",
                "native read lease-miss threshold violation was not rejected",
                "native read lease-hit trend regression was not rejected",
                "native read lease-miss trend regression was not rejected",
                "native hydration trend regression was not rejected",
                "mount-nfs-linux",
                "mount-nfs-macos",
                "mount-nfs-windows",
            ),
            excludes=(),
        ),
        TextCheck(
            "Homebrew tap seed delegates formula generation",
            root / "crab" / "scripts" / "release" / "seed-homebrew-tap.sh",
            contains=('"$SCRIPT_DIR/update-homebrew.sh" "$TAG"',),
            excludes=("PLACEHOLDER",),
        ),
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate hosted release archive contents.")
    parser.parse_args()

    root = repo_root()
    errors: list[str] = []
    for check in checks(root):
        errors.extend(check_text(check))
    errors.extend(check_makefile_nfs_feature_gate(root))
    errors.extend(check_release_script(root))
    errors.extend(check_release_workflow(root))
    errors.extend(
        check_homebrew_script(
            root,
            "crab/scripts/release/update-homebrew.sh",
            'cat > "$FORMULA_PATH" << EOF',
            "EOF",
        )
    )

    if errors:
        print("error: release archive contract drifted:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print("ok: hosted release archives, installers, and Homebrew layout match the CLI/FUSE/NFS mount contract")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
