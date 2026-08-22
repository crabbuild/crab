use std::path::PathBuf;
use std::process::{Command, Output};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/check-replica-live-env.sh")
}

fn release_evidence_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/release/verify-replica-release-evidence.sh")
}

fn cross_region_matrix_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/run-replica-cross-region-matrix.sh")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crab crate has repository parent")
        .to_path_buf()
}

fn workflow_body(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).expect("read workflow")
}

fn run_preflight(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new("/bin/bash");
    command.arg(script()).args(args).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("run live preflight")
}

fn run_release_evidence(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new("/bin/bash");
    command
        .arg(release_evidence_script())
        .args(args)
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("run release evidence verifier")
}

fn run_cross_region_matrix(envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new("/bin/bash");
    command.arg(cross_region_matrix_script()).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("run cross-region matrix")
}

fn base_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CRAB_REPLICA_LIVE", "1"),
        ("CRAB_REPLICA_LIVE_MUTATE", "1"),
        (
            "CRAB_REPLICA_LIVE_EVIDENCE_DIR",
            "/tmp/crab-replica-evidence",
        ),
        ("CRAB_REPLICA_LIVE_EVIDENCE_REDACT", "1"),
        ("CRAB_REPLICA_LIVE_RUN_ID", "replica-live-123456789-1"),
    ]
}

fn credential_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("AWS_ACCESS_KEY_ID", "test-access-key"),
        ("AWS_SECRET_ACCESS_KEY", "test-secret-key"),
        ("GOOGLE_APPLICATION_CREDENTIALS_JSON", "{}"),
        ("AZURE_TENANT_ID", "tenant"),
        ("AZURE_CLIENT_ID", "client"),
        ("AZURE_CLIENT_SECRET", "secret"),
        ("AZURE_SUBSCRIPTION_ID", "subscription"),
        ("AZURE_RESOURCE_GROUP", "resource-group"),
    ]
}

fn enterprise_matrix_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CRAB_REPLICA_LIVE_PRODUCTION_LOAD", "1"),
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_GCS", "1"),
        ("CRAB_REPLICA_LIVE_GCS_PRIMARY", "gs://primary/repo"),
        ("CRAB_REPLICA_LIVE_GCS_REPLICA", "gs://replica/repo"),
        ("CRAB_REPLICA_LIVE_GCS_REGION", "us-west2"),
        ("CRAB_REPLICA_LIVE_AZURE", "1"),
        ("CRAB_REPLICA_LIVE_AZURE_PRIMARY", "az://primary/repo"),
        ("CRAB_REPLICA_LIVE_AZURE_REPLICA", "az://replica/repo"),
        ("CRAB_REPLICA_LIVE_AZURE_REGION", "westus2"),
        (
            "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/s3-provider-log.json",
        ),
        (
            "CRAB_REPLICA_LIVE_GCS_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/gcs-provider-log.json",
        ),
        (
            "CRAB_REPLICA_LIVE_AZURE_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/azure-provider-log.json",
        ),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE", "1"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_PRIMARY_BUCKET", "primary"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REPLICA_BUCKET", "replica"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_GCS_HYDRATE", "1"),
        ("CRAB_REPLICA_LIVE_GCS_HYDRATE_PRIMARY_BUCKET", "primary"),
        ("CRAB_REPLICA_LIVE_GCS_HYDRATE_REPLICA_BUCKET", "replica"),
        ("CRAB_REPLICA_LIVE_GCS_HYDRATE_REGION", "us-west2"),
        ("CRAB_REPLICA_LIVE_AZURE_HYDRATE", "1"),
        ("CRAB_REPLICA_LIVE_AZURE_HYDRATE_PRIMARY_ACCOUNT", "primary"),
        (
            "CRAB_REPLICA_LIVE_AZURE_HYDRATE_PRIMARY_CONTAINER",
            "source",
        ),
        ("CRAB_REPLICA_LIVE_AZURE_HYDRATE_REPLICA_ACCOUNT", "replica"),
        ("CRAB_REPLICA_LIVE_AZURE_HYDRATE_REPLICA_CONTAINER", "dest"),
        ("CRAB_REPLICA_LIVE_AZURE_HYDRATE_REGION", "westus2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-east-1"),
        ("CRAB_REPLICA_LIVE_SPANNER", "1"),
        ("CRAB_REPLICA_LIVE_SPANNER_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_SPANNER_REGION", "nam3"),
        ("CRAB_REPLICA_LIVE_SPANNER_FAILOVER_REGION", "us-west2"),
        ("CRAB_REPLICA_LIVE_COSMOSDB", "1"),
        ("CRAB_REPLICA_LIVE_COSMOSDB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_COSMOSDB_REGION", "westus2"),
        ("CRAB_REPLICA_LIVE_COSMOSDB_FAILOVER_REGION", "eastus"),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/dynamodb-provider-log.json",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/spanner-provider-log.json",
        ),
        (
            "CRAB_REPLICA_LIVE_COSMOSDB_PROVIDER_LOG_EVIDENCE",
            "https://evidence.example/cosmosdb-provider-log.json",
        ),
        ("CRAB_REPLICA_LIVE_CROSS_REGION", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_A_URL",
            "s3://writer-a/dynamodb/repo",
        ),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_A_REGION",
            "us-west-2",
        ),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_B_URL",
            "s3://writer-b/dynamodb/repo",
        ),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_B_REGION",
            "us-east-1",
        ),
        (
            "CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_URL",
            "gs://writer-a/spanner/repo",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_REGION",
            "us-west2",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_URL",
            "gs://writer-b/spanner/repo",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_REGION",
            "us-east4",
        ),
        (
            "CRAB_REPLICA_LIVE_SPANNER_SMOKE_COORDINATOR_URL",
            "spanner://crab-replica-test/repo-state",
        ),
        (
            "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_URL",
            "az://writer-a/cosmos/repo",
        ),
        (
            "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_REGION",
            "westus2",
        ),
        (
            "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_URL",
            "az://writer-b/cosmos/repo",
        ),
        ("CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_REGION", "eastus"),
        (
            "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_URL",
            "cosmosdb://crab-replica-test/repo-state",
        ),
        (
            "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE",
            "https://evidence.example/repair-worker-deployment.json",
        ),
    ]
}

fn cross_region_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-east-1"),
        ("CRAB_REPLICA_LIVE_CROSS_REGION", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
        (
            "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE",
            "https://evidence.example/repair-worker-deployment.json",
        ),
    ]
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn live_evidence_workflow_artifact_name_is_run_attempt_bound() {
    let body = workflow_body(".github/workflows/replica-live-evidence.yml");

    assert!(
        body.contains("name: replica-live-evidence-${{ github.run_id }}-${{ github.run_attempt }}")
    );
}

#[test]
fn live_evidence_workflow_exports_provider_log_evidence_refs() {
    let body = workflow_body(".github/workflows/replica-live-evidence.yml");

    for name in [
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "CRAB_REPLICA_LIVE_GCS_PROVIDER_LOG_EVIDENCE",
        "CRAB_REPLICA_LIVE_AZURE_PROVIDER_LOG_EVIDENCE",
        "CRAB_REPLICA_LIVE_DYNAMODB_PROVIDER_LOG_EVIDENCE",
        "CRAB_REPLICA_LIVE_SPANNER_PROVIDER_LOG_EVIDENCE",
        "CRAB_REPLICA_LIVE_COSMOSDB_PROVIDER_LOG_EVIDENCE",
    ] {
        assert!(body.contains(name), "{name}");
    }
}

#[test]
fn release_workflow_default_artifact_name_is_run_attempt_bound() {
    let body = workflow_body(".github/workflows/release.yml");

    assert!(body.contains("artifact=\"replica-live-evidence-${run_id}-${attempt}\""));
    assert!(!body.contains("artifact=\"replica-live-evidence-$run_id\""));
}

#[test]
fn preflight_rejects_status_profile_for_mutating_control_plane_evidence() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "control-plane",
            "--storage-provider",
            "s3",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "control-plane-status",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("evidence profile 'control-plane-mutate'"));
}

#[test]
fn release_evidence_verifier_requires_expected_run_attempt_id() {
    let tmp = tempfile::tempdir().unwrap();
    let evidence_dir = tmp.path().to_str().unwrap();

    let output = run_release_evidence(&[evidence_dir], &[]);

    assert!(
        !output.status.success(),
        "release evidence unexpectedly passed"
    );
    assert!(stderr(&output).contains("expected run-attempt ID is required"));
}

#[test]
fn release_evidence_verifier_rejects_malformed_expected_run_attempt_id() {
    let tmp = tempfile::tempdir().unwrap();
    let evidence_dir = tmp.path().to_str().unwrap();

    let output = run_release_evidence(&[evidence_dir, "out.json", "replica-live-dev"], &[]);

    assert!(
        !output.status.success(),
        "release evidence unexpectedly passed"
    );
    assert!(
        stderr(&output)
            .contains("expected run-attempt ID must match replica-live-<github-run-id>-<attempt>")
    );
}

#[test]
fn preflight_accepts_mutating_control_plane_profile() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "control-plane",
            "--storage-provider",
            "s3",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "control-plane-mutate",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_rejects_non_hydrate_profile_for_hydrate_evidence() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3_HYDRATE", "1"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_PRIMARY_BUCKET", "primary"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REPLICA_BUCKET", "replica"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REGION", "us-west-2"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "hydrate",
            "--hydrate-provider",
            "s3",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "control-plane-status",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("evidence profile 'provider-hydrate'"));
}

#[test]
fn preflight_accepts_active_active_smoke_with_repair_worker_deployment_evidence() {
    let mut envs = base_env();
    envs.extend(cross_region_env());

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_accepts_active_active_smoke_with_crab_writer_urls() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL");
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL",
        "crab://writer-a/repo",
    ));
    envs.push((
        "CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL",
        "crab://writer-b/repo",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn cross_region_matrix_uses_generic_coordinator_regions_for_single_provider() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let cargo = tmp.path().join("cargo");
    let capture = tmp.path().join("capture");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
set -eu
{
    printf '%s\n' "$CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION"
    printf '%s\n' "$CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION"
    printf '%s\n' "$*"
} >> "$CRAB_MATRIX_CAPTURE"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cargo, permissions).unwrap();

    let cargo = cargo.to_string_lossy().into_owned();
    let capture = capture.to_string_lossy().into_owned();
    let output = run_cross_region_matrix(&[
        ("CARGO", cargo.as_str()),
        ("CRAB_MATRIX_CAPTURE", capture.as_str()),
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
    ]);

    assert!(
        output.status.success(),
        "matrix runner failed: {}",
        stderr(&output)
    );
    let captured = std::fs::read_to_string(capture).unwrap();
    assert!(captured.contains("us-west-2\nus-east-1\n"));
    assert!(captured.contains("live_active_active_cross_region_push_fetch_hydrate_smoke"));
    assert!(!captured.contains("live_active_active_production_load_evidence"));
}

#[cfg(unix)]
#[test]
fn cross_region_matrix_runs_load_when_enterprise_load_enabled() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let cargo = tmp.path().join("cargo");
    let capture = tmp.path().join("capture");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$CRAB_MATRIX_CAPTURE"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cargo, permissions).unwrap();

    let cargo = cargo.to_string_lossy().into_owned();
    let capture = capture.to_string_lossy().into_owned();
    let output = run_cross_region_matrix(&[
        ("CARGO", cargo.as_str()),
        ("CRAB_MATRIX_CAPTURE", capture.as_str()),
        ("CRAB_REPLICA_LIVE_PRODUCTION_LOAD", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
    ]);

    assert!(
        output.status.success(),
        "matrix runner failed: {}",
        stderr(&output)
    );
    let captured = std::fs::read_to_string(capture).unwrap();
    assert!(captured.contains("live_active_active_cross_region_push_fetch_hydrate_smoke"));
    assert!(captured.contains("live_active_active_production_load_evidence"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_one_writer_region() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION");
    envs.push(("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-west-2"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer regions for dynamodb"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_invalid_load_budget() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.push(("CRAB_REPLICA_LIVE_LOAD_PUSH_LATENCY_BUDGET_MS", "0"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output).contains("CRAB_REPLICA_LIVE_LOAD_PUSH_LATENCY_BUDGET_MS positive integer")
    );
}

#[test]
fn preflight_rejects_active_active_smoke_with_production_load_enabled() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.push(("CRAB_REPLICA_LIVE_PRODUCTION_LOAD", "1"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output).contains("CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1 requires --suite enterprise")
    );
}

#[test]
fn preflight_rejects_active_active_smoke_with_one_writer_url() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL");
    envs.push(("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-a/repo"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer URLs for dynamodb"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_unsupported_writer_url() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL",
        "postgres://writer-a/repo",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer URL for dynamodb"));
    assert!(stderr(&output).contains("crab://, s3://, gs://, az://, or azure://"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_wrong_coordinator_url_provider() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.extend([
        ("CRAB_REPLICA_LIVE_SPANNER", "1"),
        ("CRAB_REPLICA_LIVE_SPANNER_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_SPANNER_REGION", "nam3"),
        ("CRAB_REPLICA_LIVE_SPANNER_FAILOVER_REGION", "us-west2"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "spanner",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke coordinator URL for spanner"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_unsupported_coordinator_url() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
        "postgres://coordinator",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("dynamodb://, spanner://, or cosmosdb://"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_one_coordinator_region() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION");
    envs.push(("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-west-2"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke coordinator regions for dynamodb"));
}

#[test]
fn preflight_accepts_repair_service_template_selector() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.push(("CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE", " kubernetes "));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_rejects_unsupported_repair_service_template_selector() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.push(("CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE", "cron"));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE"));
}

#[test]
fn preflight_rejects_active_active_smoke_without_repair_worker_deployment_evidence() {
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE");

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_absolute_repair_worker_artifact_ref() {
    std::fs::write(
        "/tmp/crab-replica-preflight-repair-worker-proof.txt",
        "deployment ok",
    )
    .unwrap();
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE",
        "/tmp/crab-replica-preflight-repair-worker-proof.txt",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR"));
}

#[test]
fn preflight_rejects_active_active_smoke_with_directory_repair_worker_artifact_ref() {
    std::fs::create_dir_all("/tmp/crab-replica-evidence/repair-worker-proof-dir").unwrap();
    let mut envs = base_env();
    envs.extend(cross_region_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE",
        "repair-worker-proof-dir",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "cross-region",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "active-active-smoke",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output).contains("existing relative file inside CRAB_REPLICA_LIVE_EVIDENCE_DIR")
    );
}

#[test]
fn preflight_accepts_enterprise_suite_profile() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_rejects_enterprise_profile_without_provider_log_evidence() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SPANNER_PROVIDER_LOG_EVIDENCE");

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_SPANNER_PROVIDER_LOG_EVIDENCE"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_absolute_provider_log_artifact_ref() {
    std::fs::write(
        "/tmp/crab-replica-preflight-provider-log.json",
        "{\"provider\":\"s3\"}",
    )
    .unwrap();
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "/tmp/crab-replica-preflight-provider-log.json",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_directory_provider_log_artifact_ref() {
    std::fs::create_dir_all("/tmp/crab-replica-evidence/provider-log-dir").unwrap();
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "provider-log-dir",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output).contains("existing relative file inside CRAB_REPLICA_LIVE_EVIDENCE_DIR")
    );
}

#[test]
fn preflight_rejects_enterprise_profile_with_dot_segment_provider_log_artifact_ref() {
    std::fs::create_dir_all("/tmp/crab-replica-evidence").unwrap();
    std::fs::write(
        "/tmp/crab-replica-evidence/s3-provider-log.json",
        "{\"provider\":\"s3\"}",
    )
    .unwrap();
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "./s3-provider-log.json",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_http_provider_log_artifact_ref() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "http://evidence.example/s3-provider-log.json",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_incomplete_provider_log_artifact_uri() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "s3://provider-log-bucket",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("complete secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_query_provider_log_artifact_uri() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "https://evidence.example/s3-provider-log.json?download=1",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("complete secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_prefix_provider_log_artifact_uri() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "s3://provider-log-bucket/logs/",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("complete secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_empty_segment_provider_log_artifact_uri() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "https://evidence.example//s3-provider-log.json",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("complete secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_parent_segment_provider_log_artifact_uri() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE");
    envs.push((
        "CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE",
        "https://evidence.example/logs/../s3-provider-log.json",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("complete secure artifact URI"));
}

#[test]
fn preflight_rejects_enterprise_without_cloud_credentials_when_required() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--require-cloud-credentials",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    let stderr = stderr(&output);
    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr.contains("AWS credentials"), "{stderr}");
    assert!(stderr.contains("Google Cloud credentials"), "{stderr}");
    assert!(stderr.contains("Azure credentials"), "{stderr}");
    assert!(stderr.contains("AZURE_SUBSCRIPTION_ID"), "{stderr}");
    assert!(stderr.contains("AZURE_RESOURCE_GROUP"), "{stderr}");
}

#[test]
fn preflight_accepts_enterprise_with_cloud_credentials_when_required() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.extend(credential_env());

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--require-cloud-credentials",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_accepts_aws_profile_for_s3_credentials_when_required() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
        ("AWS_PROFILE", "replica-live"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "control-plane",
            "--storage-provider",
            "s3",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--require-cloud-credentials",
            "--evidence-profile",
            "control-plane-mutate",
        ],
        &envs,
    );

    assert!(
        output.status.success(),
        "preflight failed: {}",
        stderr(&output)
    );
}

#[test]
fn preflight_rejects_control_plane_with_one_coordinator_region() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-west-2"),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "control-plane",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "control-plane-mutate",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("coordinator regions for dynamodb"));
}

#[test]
fn preflight_rejects_partial_enterprise_provider_matrix() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE", "1"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_PRIMARY_BUCKET", "primary"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REPLICA_BUCKET", "replica"),
        ("CRAB_REPLICA_LIVE_S3_HYDRATE_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-east-1"),
        ("CRAB_REPLICA_LIVE_CROSS_REGION", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "s3",
            "--coordinator",
            "dynamodb",
            "--hydrate-provider",
            "s3",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output)
            .contains("enterprise evidence requires storage control-plane provider 'gcs'")
    );
}

#[test]
fn preflight_rejects_enterprise_profile_without_run_id() {
    let mut envs = base_env();
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_RUN_ID");
    envs.extend(enterprise_matrix_env());

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_RUN_ID"));
}

#[test]
fn preflight_rejects_enterprise_profile_without_production_load_enabled() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_PRODUCTION_LOAD");

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1"));
}

#[test]
fn preflight_rejects_enterprise_profile_with_malformed_run_attempt_id() {
    let mut envs = base_env();
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_RUN_ID");
    envs.push(("CRAB_REPLICA_LIVE_RUN_ID", "test-live-run"));
    envs.extend(enterprise_matrix_env());

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(
        stderr(&output)
            .contains("CRAB_REPLICA_LIVE_RUN_ID matching replica-live-<github-run-id>-<attempt>")
    );
}

#[test]
fn preflight_rejects_enterprise_profile_without_repair_worker_deployment_evidence() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE");

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE"));
}

#[test]
fn preflight_rejects_enterprise_matrix_without_provider_specific_smoke_writer() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_URL");

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_URL"));
}

#[test]
fn preflight_rejects_enterprise_matrix_with_one_writer_region_for_provider() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_REGION");
    envs.push((
        "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_REGION",
        "westus2",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer regions for cosmosdb"));
}

#[test]
fn preflight_rejects_enterprise_matrix_with_one_writer_url_for_provider() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_URL",
        "gs://writer-a/spanner/repo",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer URLs for spanner"));
}

#[test]
fn preflight_rejects_enterprise_matrix_with_unsupported_writer_url_for_provider() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_URL",
        "postgres://writer-a/cosmos/repo",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke writer URL for cosmosdb"));
    assert!(stderr(&output).contains("crab://, s3://, gs://, az://, or azure://"));
}

#[test]
fn preflight_rejects_enterprise_matrix_with_wrong_coordinator_url_provider() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_URL");
    envs.push((
        "CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_URL",
        "spanner://crab-replica-test/repo-state",
    ));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("active-active smoke coordinator URL for cosmosdb"));
}

#[test]
fn preflight_rejects_enterprise_matrix_with_one_coordinator_region_for_provider() {
    let mut envs = base_env();
    envs.extend(enterprise_matrix_env());
    envs.retain(|(name, _)| *name != "CRAB_REPLICA_LIVE_SPANNER_FAILOVER_REGION");
    envs.push(("CRAB_REPLICA_LIVE_SPANNER_FAILOVER_REGION", "nam3"));

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "all",
            "--coordinator",
            "all",
            "--hydrate-provider",
            "all",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("coordinator regions for spanner"));
}

#[test]
fn preflight_rejects_enterprise_profile_without_hydrate_provider() {
    let mut envs = base_env();
    envs.extend([
        ("CRAB_REPLICA_LIVE_S3", "1"),
        ("CRAB_REPLICA_LIVE_S3_PRIMARY", "s3://primary/repo"),
        ("CRAB_REPLICA_LIVE_S3_REPLICA", "s3://replica/repo"),
        ("CRAB_REPLICA_LIVE_S3_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB", "1"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_NAME", "crab-replica-test"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION", "us-east-1"),
        ("CRAB_REPLICA_LIVE_CROSS_REGION", "1"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL", "s3://writer-a/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION", "us-west-2"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL", "s3://writer-b/repo"),
        ("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION", "us-east-1"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL",
            "dynamodb://crab-replica-test",
        ),
        ("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION", "us-west-2"),
        (
            "CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION",
            "us-east-1",
        ),
    ]);

    let output = run_preflight(
        &[
            "--suite",
            "enterprise",
            "--storage-provider",
            "s3",
            "--coordinator",
            "dynamodb",
            "--mutate",
            "--require-evidence",
            "--require-redacted",
            "--evidence-profile",
            "enterprise",
        ],
        &envs,
    );

    assert!(!output.status.success(), "preflight unexpectedly passed");
    assert!(stderr(&output).contains("at least one --hydrate-provider for hydrate"));
}
