//! Ignored live tests for enterprise replication control planes.
//!
//! These tests are intentionally inert in normal CI. They require disposable
//! provider resources, ambient cloud credentials, `CRAB_REPLICA_LIVE=1`, and a
//! provider-specific flag. Cloud mutations additionally require
//! `CRAB_REPLICA_LIVE_MUTATE=1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crab::cmd::replica::{
    LiveControlPlaneEvidencePayload, LiveControlPlaneEvidenceSchema, LiveEvidenceSchemaVersion,
};
use crab::replication::{
    ControlPlaneCheckState, ControlPlaneStatus, ReplicaConfig, ReplicationProviderKind,
    ReplicationRpo, apply_control_plane_plan, control_plane_plan, control_plane_remove_plan,
    inspect_control_plane_plan_default, remove_control_plane_plan,
};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct StorageFixture {
    name: String,
    provider: ReplicationProviderKind,
    primary: String,
    replica: String,
    region: String,
    rpo: ReplicationRpo,
    backfill: bool,
}

#[derive(Debug)]
struct CoordinatorFixture {
    provider: &'static str,
    provider_flag: &'static str,
    name_env: &'static str,
    region_env: &'static str,
    failover_env: &'static str,
}

#[derive(Debug)]
struct EvidenceRecorder {
    root: PathBuf,
    sequence: AtomicU64,
    run_id: String,
    provider: String,
    redacted: bool,
    sensitive_values: Vec<String>,
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

fn enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn live_enabled(provider_flag: &str) -> bool {
    enabled("CRAB_REPLICA_LIVE") && enabled(provider_flag)
}

fn mutate_enabled() -> bool {
    enabled("CRAB_REPLICA_LIVE_MUTATE")
}

fn env_value(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            eprintln!("skipping live replica test: {name} is not set");
            None
        }
    }
}

fn env_bool(name: &str) -> bool {
    enabled(name)
}

fn env_rpo(name: &str) -> TestResult<ReplicationRpo> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(ReplicationRpo::parse(&value)?),
        _ => Ok(ReplicationRpo::Standard),
    }
}

impl EvidenceRecorder {
    fn from_storage_fixture(fixture: &StorageFixture) -> TestResult<Option<Self>> {
        Self::from_sensitive_values(fixture.provider.as_str(), storage_sensitive_values(fixture))
    }

    fn from_coordinator_target(
        fixture: &CoordinatorFixture,
        name: &str,
        _region: &str,
        _failover_region: &str,
    ) -> TestResult<Option<Self>> {
        let mut values = Vec::new();
        collect_sensitive_value(&mut values, name);
        Self::from_sensitive_values(fixture.provider, values)
    }

    fn from_sensitive_values(
        provider: impl Into<String>,
        mut sensitive_values: Vec<String>,
    ) -> TestResult<Option<Self>> {
        let Some(root) = env::var_os("CRAB_REPLICA_LIVE_EVIDENCE_DIR") else {
            return Ok(None);
        };
        sensitive_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        sensitive_values.dedup();

        let provider = provider.into();
        let run_id = evidence_run_id("replica-live-control-plane");
        let root = evidence_artifact_root(
            PathBuf::from(root),
            &run_id,
            "replica-live-control-plane",
            &provider,
        );
        std::fs::create_dir_all(&root)?;
        Ok(Some(Self {
            root,
            sequence: AtomicU64::new(1),
            run_id,
            provider,
            redacted: enabled("CRAB_REPLICA_LIVE_EVIDENCE_REDACT"),
            sensitive_values,
        }))
    }

    fn record_json(&self, label: &str, args: &[String], value: &Value) -> TestResult {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let payload = LiveControlPlaneEvidencePayload {
            schema: LiveControlPlaneEvidenceSchema::ReplicaLiveControlPlaneEvidence,
            version: LiveEvidenceSchemaVersion::V1,
            collected_at_ms: now_ms(),
            harness: Some("replica-live-control-plane".to_owned()),
            run_id: Some(self.run_id.clone()),
            sequence: Some(sequence),
            label: label.to_owned(),
            provider: Some(self.provider.clone()),
            redacted: self.redacted,
            args: args.to_vec(),
            result: value.clone(),
        };
        let mut payload = serde_json::to_value(payload)?;
        self.redact_value(&mut payload);
        self.write(label, sequence, &payload)
    }

    fn record_provider_log(&self, label: &str, scope: &str) -> TestResult {
        let Some(artifact_ref) = provider_log_ref(&self.provider) else {
            return Ok(());
        };
        let args = vec![
            "provider-log".to_owned(),
            self.provider.clone(),
            artifact_ref.clone(),
        ];
        let value = serde_json::json!({
            "schema": "replica.live-provider-log",
            "data": {
                "provider": self.provider,
                "scope": scope,
                "artifact_ref": artifact_ref
            }
        });
        self.record_json(label, &args, &value)
    }

    fn write(&self, label: &str, sequence: u64, payload: &Value) -> TestResult {
        let path = self
            .root
            .join(format!("{sequence:03}-{}.json", evidence_slug(label)));
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::other(format!("evidence path {} has no parent", path.display()))
        })?;
        std::fs::create_dir_all(parent)?;
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(temp.as_file(), payload)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|err| err.error)?;
        Ok(())
    }

    fn redact_value(&self, value: &mut Value) {
        if !self.redacted {
            return;
        }
        match value {
            Value::String(text) => redact_text(text, &self.sensitive_values),
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    self.redact_value(value);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

fn evidence_artifact_root(
    base: PathBuf,
    run_id: &str,
    harness: &str,
    discriminator: &str,
) -> PathBuf {
    base.join(evidence_slug(run_id))
        .join(harness)
        .join(evidence_slug(discriminator))
}

fn evidence_run_id(harness: &str) -> String {
    env::var("CRAB_REPLICA_LIVE_RUN_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{harness}-{}-{}", std::process::id(), now_ms()))
}

fn record_evidence(
    evidence: &Option<EvidenceRecorder>,
    label: &str,
    args: &[String],
    value: Value,
) -> TestResult {
    if let Some(evidence) = evidence.as_ref() {
        evidence.record_json(label, args, &value)?;
    }
    Ok(())
}

fn record_provider_log_evidence(
    evidence: &Option<EvidenceRecorder>,
    label: &str,
    scope: &str,
) -> TestResult {
    if let Some(evidence) = evidence.as_ref() {
        evidence.record_provider_log(label, scope)?;
    }
    Ok(())
}

fn provider_log_ref(provider: &str) -> Option<String> {
    let name = format!(
        "CRAB_REPLICA_LIVE_{}_PROVIDER_LOG_EVIDENCE",
        provider.to_ascii_uppercase().replace('-', "_")
    );
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn storage_sensitive_values(fixture: &StorageFixture) -> Vec<String> {
    let mut values = Vec::new();
    collect_sensitive_value(&mut values, &fixture.name);
    collect_sensitive_url(&mut values, &fixture.primary);
    collect_sensitive_url(&mut values, &fixture.replica);
    values
}

fn collect_sensitive_url(values: &mut Vec<String>, url: &str) {
    collect_sensitive_value(values, url);
    for part in
        url.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/')))
    {
        collect_sensitive_value(values, part.trim_matches('/'));
    }
    for part in url.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        collect_sensitive_value(values, part);
    }
}

fn collect_sensitive_value(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.len() >= 4 {
        values.push(trimmed.to_owned());
    }
}

fn redact_text(text: &mut String, sensitive_values: &[String]) {
    for sensitive in sensitive_values {
        if text == sensitive {
            text.clear();
            text.push_str("<redacted>");
            continue;
        }
        *text = text.replace(sensitive, "<redacted>");
    }
}

fn evidence_slug(label: &str) -> String {
    let mut slug = String::new();
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 80 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "artifact".to_owned()
    } else {
        slug.to_owned()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn storage_fixture(
    provider_name: &str,
    provider_flag: &'static str,
    provider: ReplicationProviderKind,
) -> TestResult<Option<StorageFixture>> {
    if !live_enabled(provider_flag) {
        eprintln!(
            "skipping live {provider_name} replica test: set CRAB_REPLICA_LIVE=1 and {provider_flag}=1"
        );
        return Ok(None);
    }

    let prefix = format!("CRAB_REPLICA_LIVE_{provider_name}");
    let Some(primary) = env_value(&format!("{prefix}_PRIMARY")) else {
        return Ok(None);
    };
    let Some(replica) = env_value(&format!("{prefix}_REPLICA")) else {
        return Ok(None);
    };
    let Some(region) = env_value(&format!("{prefix}_REGION")) else {
        return Ok(None);
    };

    let name = env::var(format!("{prefix}_NAME")).unwrap_or_else(|_| {
        format!(
            "live-{}",
            provider_name.to_ascii_lowercase().replace('_', "-")
        )
    });
    let rpo = env_rpo(&format!("{prefix}_RPO"))?;
    let backfill = env_bool(&format!("{prefix}_BACKFILL"));

    Ok(Some(StorageFixture {
        name,
        provider,
        primary,
        replica,
        region,
        rpo,
        backfill,
    }))
}

fn assert_storage_status_queried(status: &ControlPlaneStatus, fixture: &StorageFixture) {
    assert_eq!(status.provider, fixture.provider);
    assert_eq!(status.replica_name, fixture.name);
    assert_eq!(status.primary, fixture.primary);
    assert_eq!(status.replica, fixture.replica);
    assert!(
        status.backend_available,
        "{} control-plane backend should be available",
        fixture.provider
    );
    assert!(
        status.checked_drift,
        "{} control-plane status should inspect drift",
        fixture.provider
    );
    assert!(
        !status.checks.is_empty(),
        "{} control-plane status should include resource checks",
        fixture.provider
    );
    assert!(
        status
            .checks
            .iter()
            .all(|check| check.state != ControlPlaneCheckState::Unknown),
        "{} control-plane status should not leave checks unknown: {:?}",
        fixture.provider,
        status.checks
    );
}

async fn run_storage_control_plane_live(fixture: StorageFixture) -> TestResult {
    let evidence = EvidenceRecorder::from_storage_fixture(&fixture)?;
    let plan = control_plane_plan(
        &fixture.name,
        fixture.provider,
        &fixture.primary,
        &fixture.replica,
        &fixture.region,
        fixture.rpo,
        fixture.backfill,
    );
    record_evidence(
        &evidence,
        "storage-plan",
        &["control-plane-plan".to_owned()],
        serde_json::to_value(&plan)?,
    )?;

    let status = inspect_control_plane_plan_default(&plan).await;
    record_evidence(
        &evidence,
        "storage-status",
        &["control-plane-status".to_owned()],
        serde_json::to_value(&status)?,
    )?;
    assert_storage_status_queried(&status, &fixture);

    if !mutate_enabled() {
        record_provider_log_evidence(&evidence, "storage-provider-log", "storage-control-plane")?;
        eprintln!(
            "live {} status passed; set CRAB_REPLICA_LIVE_MUTATE=1 to run apply/remove",
            fixture.provider
        );
        return Ok(());
    }

    let apply = apply_control_plane_plan(&plan).await?;
    record_evidence(
        &evidence,
        "storage-apply",
        &["control-plane-apply".to_owned()],
        serde_json::to_value(&apply)?,
    )?;
    let after_apply = inspect_control_plane_plan_default(&plan).await;
    record_evidence(
        &evidence,
        "storage-status-after-apply",
        &["control-plane-status".to_owned()],
        serde_json::to_value(&after_apply)?,
    )?;
    let replica = ReplicaConfig {
        name: fixture.name.clone(),
        provider: fixture.provider,
        url: fixture.replica.clone(),
        region: fixture.region.clone(),
        backfill: fixture.backfill,
        read: false,
        rpo: fixture.rpo,
    };
    let remove_plan = control_plane_remove_plan(&replica, &fixture.primary);
    record_evidence(
        &evidence,
        "storage-remove-plan",
        &["control-plane-remove-plan".to_owned()],
        serde_json::to_value(&remove_plan)?,
    )?;
    let remove = remove_control_plane_plan(&remove_plan).await?;
    record_evidence(
        &evidence,
        "storage-remove",
        &["control-plane-remove".to_owned()],
        serde_json::to_value(&remove)?,
    )?;

    assert!(apply.applied, "apply should report a mutation result");
    assert!(apply.checked_drift, "apply should inspect drift first");
    assert_storage_status_queried(&after_apply, &fixture);
    assert!(remove.applied, "remove should report a mutation result");
    assert!(remove.checked_drift, "remove should inspect drift first");
    record_provider_log_evidence(&evidence, "storage-provider-log", "storage-control-plane")?;

    Ok(())
}

#[ignore = "requires live S3 buckets, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn live_s3_replication_control_plane_status_and_optional_apply_remove() -> TestResult {
    if let Some(fixture) =
        storage_fixture("S3", "CRAB_REPLICA_LIVE_S3", ReplicationProviderKind::S3)?
    {
        run_storage_control_plane_live(fixture).await?;
    }
    Ok(())
}

#[ignore = "requires live GCS buckets, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn live_gcs_replication_control_plane_status_and_optional_apply_remove() -> TestResult {
    if let Some(fixture) =
        storage_fixture("GCS", "CRAB_REPLICA_LIVE_GCS", ReplicationProviderKind::Gcs)?
    {
        run_storage_control_plane_live(fixture).await?;
    }
    Ok(())
}

#[ignore = "requires live Azure Storage accounts, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn live_azure_replication_control_plane_status_and_optional_apply_remove() -> TestResult {
    if let Some(fixture) = storage_fixture(
        "AZURE",
        "CRAB_REPLICA_LIVE_AZURE",
        ReplicationProviderKind::Azure,
    )? {
        run_storage_control_plane_live(fixture).await?;
    }
    Ok(())
}

fn coordinator_fixtures() -> [CoordinatorFixture; 3] {
    [
        CoordinatorFixture {
            provider: "dynamodb",
            provider_flag: "CRAB_REPLICA_LIVE_DYNAMODB",
            name_env: "CRAB_REPLICA_LIVE_DYNAMODB_NAME",
            region_env: "CRAB_REPLICA_LIVE_DYNAMODB_REGION",
            failover_env: "CRAB_REPLICA_LIVE_DYNAMODB_FAILOVER_REGION",
        },
        CoordinatorFixture {
            provider: "spanner",
            provider_flag: "CRAB_REPLICA_LIVE_SPANNER",
            name_env: "CRAB_REPLICA_LIVE_SPANNER_NAME",
            region_env: "CRAB_REPLICA_LIVE_SPANNER_REGION",
            failover_env: "CRAB_REPLICA_LIVE_SPANNER_FAILOVER_REGION",
        },
        CoordinatorFixture {
            provider: "cosmosdb",
            provider_flag: "CRAB_REPLICA_LIVE_COSMOSDB",
            name_env: "CRAB_REPLICA_LIVE_COSMOSDB_NAME",
            region_env: "CRAB_REPLICA_LIVE_COSMOSDB_REGION",
            failover_env: "CRAB_REPLICA_LIVE_COSMOSDB_FAILOVER_REGION",
        },
    ]
}

fn coordinator_fixture(provider: &str) -> Option<CoordinatorFixture> {
    coordinator_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider == provider)
}

fn run_crab_json(args: &[String]) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let output = Command::new(bin()).args(args).output()?;
    parse_success_json(output, args)
}

fn parse_success_json(
    output: Output,
    args: &[String],
) -> Result<Value, Box<dyn Error + Send + Sync>> {
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Box::new(std::io::Error::other(format!(
            "crab {} failed\nstdout:\n{stdout}\nstderr:\n{stderr}",
            args.join(" ")
        ))));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn coordinator_args(
    command: &str,
    fixture: &CoordinatorFixture,
    name: &str,
    region: &str,
    failover_region: &str,
    apply: bool,
) -> Vec<String> {
    let mut args = vec![
        "replica".to_owned(),
        "coordinator".to_owned(),
        command.to_owned(),
        "--provider".to_owned(),
        fixture.provider.to_owned(),
        "--name".to_owned(),
        name.to_owned(),
        "--region".to_owned(),
        region.to_owned(),
        "--failover-region".to_owned(),
        failover_region.to_owned(),
        "--json".to_owned(),
    ];
    if apply {
        args.push("--apply".to_owned());
    } else if command == "add" {
        args.push("--dry-run".to_owned());
    }
    args
}

fn assert_coordinator_status_queried(value: &Value, fixture: &CoordinatorFixture, name: &str) {
    assert_eq!(value["schema"], "replica.coordinator.status");
    let status = &value["data"]["status"];
    assert_eq!(status["provider"], fixture.provider);
    assert_eq!(status["name"], name);
    assert_eq!(status["backend_available"], true);
    assert_eq!(status["checked_drift"], true);
    let checks = status["checks"]
        .as_array()
        .expect("coordinator checks should be an array");
    assert!(
        !checks.is_empty(),
        "{} coordinator should report resource checks",
        fixture.provider
    );
    assert!(
        checks.iter().all(|check| check["state"] != "unknown"),
        "{} coordinator checks should not remain unknown: {checks:?}",
        fixture.provider
    );
}

fn assert_apply_envelope(value: &Value, schema: &str) {
    assert_eq!(value["schema"], schema);
    assert_eq!(value["data"]["applied"], true);
    assert_eq!(value["data"]["apply_status"]["applied"], true);
    assert_eq!(value["data"]["apply_status"]["checked_drift"], true);
}

fn run_coordinator_control_plane_live(provider: &str) -> TestResult {
    let fixture = coordinator_fixture(provider).expect("test provider should be registered");
    if !live_enabled(fixture.provider_flag) {
        eprintln!(
            "skipping live {} coordinator test: set CRAB_REPLICA_LIVE=1 and {}=1",
            fixture.provider, fixture.provider_flag
        );
        return Ok(());
    }

    let Some(name) = env_value(fixture.name_env) else {
        return Ok(());
    };
    let Some(region) = env_value(fixture.region_env) else {
        return Ok(());
    };
    let Some(failover_region) = env_value(fixture.failover_env) else {
        return Ok(());
    };
    let evidence =
        EvidenceRecorder::from_coordinator_target(&fixture, &name, &region, &failover_region)?;

    let add_plan_args = coordinator_args("add", &fixture, &name, &region, &failover_region, false);
    let add_plan = run_crab_json(&add_plan_args)?;
    record_evidence(
        &evidence,
        "coordinator-plan",
        &add_plan_args,
        add_plan.clone(),
    )?;
    assert_eq!(add_plan["schema"], "replica.coordinator");
    assert_eq!(add_plan["data"]["plan"]["provider"], fixture.provider);

    let status_args = coordinator_args("status", &fixture, &name, &region, &failover_region, false);
    let status = run_crab_json(&status_args)?;
    record_evidence(
        &evidence,
        "coordinator-status",
        &status_args,
        status.clone(),
    )?;
    assert_coordinator_status_queried(&status, &fixture, &name);

    if !mutate_enabled() {
        record_provider_log_evidence(
            &evidence,
            "coordinator-provider-log",
            "coordinator-control-plane",
        )?;
        eprintln!(
            "live {} coordinator status passed; set CRAB_REPLICA_LIVE_MUTATE=1 to run apply/remove",
            fixture.provider
        );
        return Ok(());
    }

    let add_args = coordinator_args("add", &fixture, &name, &region, &failover_region, true);
    let add = run_crab_json(&add_args)?;
    record_evidence(&evidence, "coordinator-apply", &add_args, add.clone())?;
    let status_after_add_args =
        coordinator_args("status", &fixture, &name, &region, &failover_region, false);
    let status_after_add = run_crab_json(&status_after_add_args)?;
    record_evidence(
        &evidence,
        "coordinator-status-after-apply",
        &status_after_add_args,
        status_after_add.clone(),
    )?;
    let remove_args = coordinator_args("remove", &fixture, &name, &region, &failover_region, true);
    let remove = run_crab_json(&remove_args)?;
    record_evidence(
        &evidence,
        "coordinator-remove",
        &remove_args,
        remove.clone(),
    )?;

    assert_apply_envelope(&add, "replica.coordinator");
    assert_coordinator_status_queried(&status_after_add, &fixture, &name);
    assert_apply_envelope(&remove, "replica.coordinator.remove");
    record_provider_log_evidence(
        &evidence,
        "coordinator-provider-log",
        "coordinator-control-plane",
    )?;

    Ok(())
}

#[ignore = "requires a live DynamoDB coordinator target, ambient credentials, and explicit env flags"]
#[test]
fn live_dynamodb_coordinator_control_plane_status_and_optional_apply_remove() -> TestResult {
    run_coordinator_control_plane_live("dynamodb")
}

#[ignore = "requires a live Spanner coordinator target, ambient credentials, and explicit env flags"]
#[test]
fn live_spanner_coordinator_control_plane_status_and_optional_apply_remove() -> TestResult {
    run_coordinator_control_plane_live("spanner")
}

#[ignore = "requires a live Cosmos DB coordinator target, ambient credentials, and explicit env flags"]
#[test]
fn live_cosmosdb_coordinator_control_plane_status_and_optional_apply_remove() -> TestResult {
    run_coordinator_control_plane_live("cosmosdb")
}

#[test]
fn evidence_artifact_root_scopes_control_plane_artifacts() {
    assert_eq!(
        evidence_artifact_root(
            PathBuf::from("evidence"),
            "release 2026/06/16",
            "replica-live-control-plane",
            "dynamodb://prod-coordinator",
        ),
        PathBuf::from("evidence")
            .join("release-2026-06-16")
            .join("replica-live-control-plane")
            .join("dynamodb-prod-coordinator")
    );
}

#[test]
fn evidence_recorder_writes_ordered_control_plane_json_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = EvidenceRecorder {
        root: dir.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        provider: "dynamodb".to_owned(),
        redacted: false,
        sensitive_values: Vec::new(),
    };

    recorder
        .record_json(
            "coordinator status",
            &["replica".to_owned(), "coordinator".to_owned()],
            &serde_json::json!({"schema": "replica.coordinator.status"}),
        )
        .unwrap();

    let files = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(files, vec!["001-coordinator-status.json"]);

    let body = std::fs::read_to_string(dir.path().join("001-coordinator-status.json")).unwrap();
    let value = serde_json::from_str::<Value>(&body).unwrap();
    assert_eq!(value["schema"], "replica.live-control-plane.evidence");
    assert_eq!(value["harness"], "replica-live-control-plane");
    assert_eq!(value["run_id"], "test-run");
    assert_eq!(value["sequence"], 1);
    assert_eq!(value["provider"], "dynamodb");
    assert_eq!(value["result"]["schema"], "replica.coordinator.status");
}

#[test]
fn evidence_recorder_redacts_control_plane_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = EvidenceRecorder {
        root: dir.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        provider: "dynamodb".to_owned(),
        redacted: true,
        sensitive_values: vec![
            "crab://primary-bucket/disposable/repo".to_owned(),
            "primary-bucket".to_owned(),
            "crab-coordinator".to_owned(),
        ],
    };

    recorder
        .record_json(
            "storage status",
            &[
                "--primary".to_owned(),
                "crab://primary-bucket/disposable/repo".to_owned(),
            ],
            &serde_json::json!({
                "primary": "crab://primary-bucket/disposable/repo",
                "message": "bucket primary-bucket is verified",
                "coordinator": "dynamodb://crab-coordinator"
            }),
        )
        .unwrap();

    let body = std::fs::read_to_string(dir.path().join("001-storage-status.json")).unwrap();
    let value = serde_json::from_str::<Value>(&body).unwrap();
    assert_eq!(value["redacted"], true);
    assert!(body.contains("<redacted>"));
    for secret in [
        "crab://primary-bucket/disposable/repo",
        "primary-bucket",
        "crab-coordinator",
    ] {
        assert!(!body.contains(secret), "{secret} leaked into evidence");
    }
}
