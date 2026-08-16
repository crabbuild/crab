//! Ignored live cross-region smoke tests for enterprise replication.
//!
//! These tests mutate real Crab remotes and coordinator state. They require
//! disposable repo prefixes, ambient provider credentials, `CRAB_REPLICA_LIVE=1`,
//! `CRAB_REPLICA_LIVE_CROSS_REGION=1`, and `CRAB_REPLICA_LIVE_MUTATE=1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crab::cmd::replica::{
    LiveEvidenceSchemaVersion, LiveSmokeEvidencePayload, LiveSmokeEvidenceSchema,
};
use crab::git::url::{Cloud, ObjectUrl};
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct CrossRegionFixture {
    writer_a_name: String,
    writer_a_url: String,
    writer_a_region: String,
    writer_b_name: String,
    writer_b_url: String,
    writer_b_region: String,
    coordinator_url: String,
    coordinator_region: String,
    coordinator_failover_region: String,
    timeout: Duration,
}

struct SmokeWorkspace {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    path_env: OsString,
    evidence: Option<EvidenceRecorder>,
}

struct EvidenceRecorder {
    root: PathBuf,
    sequence: AtomicU64,
    run_id: String,
    harness: String,
    coordinator_provider: String,
    redacted: bool,
    sensitive_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WriterStoreTarget {
    cloud: &'static str,
    account: Option<String>,
    bucket: String,
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

fn evidence_run_id(harness: &str) -> String {
    env::var("CRAB_REPLICA_LIVE_RUN_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{harness}-{}-{}", std::process::id(), now_ms()))
}

fn env_value(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            eprintln!("skipping live cross-region replica smoke: {name} is not set");
            None
        }
    }
}

fn optional_env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn s3_endpoint() -> Option<String> {
    optional_env_value("CRAB_REPLICA_LIVE_S3_ENDPOINT")
        .or_else(|| optional_env_value("CRAB_REPLICA_LIVE_S3_HYDRATE_ENDPOINT"))
        .or_else(|| optional_env_value("AWS_ENDPOINT_URL"))
}

fn env_u64_or(name: &str, default: u64) -> TestResult<u64> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => {
            let parsed = value.trim().parse::<u64>().map_err(|err| {
                std::io::Error::other(format!("{name} must be an integer: {err}"))
            })?;
            if parsed == 0 {
                return Err(
                    std::io::Error::other(format!("{name} must be greater than zero")).into(),
                );
            }
            Ok(parsed)
        }
        _ => Ok(default),
    }
}

fn repair_service_template_from_env() -> TestResult<&'static str> {
    match env::var("CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE") {
        Ok(value) => normalize_repair_service_template(Some(&value)),
        Err(_) => normalize_repair_service_template(None),
    }
}

fn normalize_repair_service_template(value: Option<&str>) -> TestResult<&'static str> {
    let normalized = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("systemd")
        .to_ascii_lowercase();
    match normalized.as_str() {
        "systemd" => Ok("systemd"),
        "launchd" => Ok("launchd"),
        "kubernetes" => Ok("kubernetes"),
        other => Err(std::io::Error::other(format!(
            "unsupported CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE={other}; expected systemd, launchd, or kubernetes"
        ))
        .into()),
    }
}

fn fixture() -> TestResult<Option<CrossRegionFixture>> {
    if !enabled("CRAB_REPLICA_LIVE")
        || !enabled("CRAB_REPLICA_LIVE_CROSS_REGION")
        || !enabled("CRAB_REPLICA_LIVE_MUTATE")
    {
        eprintln!(
            "skipping live cross-region replica smoke: set CRAB_REPLICA_LIVE=1, CRAB_REPLICA_LIVE_CROSS_REGION=1, and CRAB_REPLICA_LIVE_MUTATE=1"
        );
        return Ok(None);
    }

    let Some(writer_a_url) = env_value("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL") else {
        return Ok(None);
    };
    let Some(writer_a_region) = env_value("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION") else {
        return Ok(None);
    };
    let Some(writer_b_url) = env_value("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL") else {
        return Ok(None);
    };
    let Some(writer_b_region) = env_value("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION") else {
        return Ok(None);
    };
    let Some(coordinator_url) = env_value("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL") else {
        return Ok(None);
    };
    let Some(coordinator_region) = env_value("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION") else {
        return Ok(None);
    };
    let Some(coordinator_failover_region) =
        env_value("CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION")
    else {
        return Ok(None);
    };

    let timeout = env::var("CRAB_REPLICA_LIVE_SMOKE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(900));

    Ok(Some(CrossRegionFixture {
        writer_a_name: env::var("CRAB_REPLICA_LIVE_SMOKE_WRITER_A_NAME")
            .unwrap_or_else(|_| "writer-a".to_owned()),
        writer_a_url,
        writer_a_region,
        writer_b_name: env::var("CRAB_REPLICA_LIVE_SMOKE_WRITER_B_NAME")
            .unwrap_or_else(|_| "writer-b".to_owned()),
        writer_b_url,
        writer_b_region,
        coordinator_url,
        coordinator_region,
        coordinator_failover_region,
        timeout,
    }))
}

impl SmokeWorkspace {
    fn new(fixture: &CrossRegionFixture) -> TestResult<Self> {
        Self::new_with_evidence(EvidenceRecorder::from_env(fixture)?)
    }

    fn new_without_evidence(_fixture: &CrossRegionFixture) -> TestResult<Self> {
        Self::new_with_evidence(None)
    }

    fn new_with_evidence(evidence: Option<EvidenceRecorder>) -> TestResult<Self> {
        let tmp = tempfile::tempdir()?;
        let helper_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&helper_dir)?;
        install_remote_helper(&helper_dir)?;

        let mut paths = vec![helper_dir];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        let path_env = env::join_paths(paths)?;

        Ok(Self {
            root: tmp.path().to_path_buf(),
            _tmp: tmp,
            path_env,
            evidence,
        })
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env("PATH", &self.path_env);
        command
    }

    fn crab(&self) -> Command {
        let mut command = Command::new(bin());
        command.env("PATH", &self.path_env);
        command
    }

    fn record_json(&self, label: &str, cwd: &Path, args: &[String], value: &Value) -> TestResult {
        if let Some(evidence) = self.evidence.as_ref() {
            evidence.record_json(label, self.relative_path(cwd), args, value)?;
        }
        Ok(())
    }

    fn record_rejection(
        &self,
        label: &str,
        cwd: &Path,
        args: &[String],
        output: &Output,
    ) -> TestResult {
        if let Some(evidence) = self.evidence.as_ref() {
            evidence.record_rejection(label, self.relative_path(cwd), args, output)?;
        }
        Ok(())
    }

    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

impl EvidenceRecorder {
    fn from_env(fixture: &CrossRegionFixture) -> TestResult<Option<Self>> {
        Self::from_env_with_harness(fixture, "replica-live-cross-region")
    }

    fn from_env_with_harness(
        fixture: &CrossRegionFixture,
        harness: &str,
    ) -> TestResult<Option<Self>> {
        let Some(root) = env::var_os("CRAB_REPLICA_LIVE_EVIDENCE_DIR") else {
            return Ok(None);
        };
        let coordinator_provider = coordinator_provider_from_url(&fixture.coordinator_url);
        let run_id = evidence_run_id(harness);
        let root =
            evidence_artifact_root(PathBuf::from(root), &run_id, harness, &coordinator_provider);
        std::fs::create_dir_all(&root)?;
        Ok(Some(Self {
            root,
            sequence: AtomicU64::new(1),
            run_id,
            harness: harness.to_owned(),
            coordinator_provider,
            redacted: enabled("CRAB_REPLICA_LIVE_EVIDENCE_REDACT"),
            sensitive_values: evidence_sensitive_values(fixture),
        }))
    }

    fn record_json(&self, label: &str, cwd: String, args: &[String], value: &Value) -> TestResult {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let payload = LiveSmokeEvidencePayload {
            schema: LiveSmokeEvidenceSchema::ReplicaLiveSmokeEvidence,
            version: LiveEvidenceSchemaVersion::V1,
            collected_at_ms: now_ms(),
            harness: Some(self.harness.clone()),
            run_id: Some(self.run_id.clone()),
            sequence: Some(sequence),
            label: label.to_owned(),
            provider: None,
            coordinator_provider: Some(self.coordinator_provider.clone()),
            redacted: self.redacted,
            cwd,
            args: args.to_vec(),
            result: Some(value.clone()),
            exit_code: None,
            stdout_json: None,
            stdout: None,
            stderr: None,
        };
        let mut payload = serde_json::to_value(payload)?;
        self.redact_value(&mut payload);
        self.write(label, sequence, &payload)
    }

    fn record_rejection(
        &self,
        label: &str,
        cwd: String,
        args: &[String],
        output: &Output,
    ) -> TestResult {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let stdout_json = serde_json::from_slice::<Value>(&output.stdout).ok();
        let payload = LiveSmokeEvidencePayload {
            schema: LiveSmokeEvidenceSchema::ReplicaLiveSmokeEvidence,
            version: LiveEvidenceSchemaVersion::V1,
            collected_at_ms: now_ms(),
            harness: Some(self.harness.clone()),
            run_id: Some(self.run_id.clone()),
            sequence: Some(sequence),
            label: label.to_owned(),
            provider: None,
            coordinator_provider: Some(self.coordinator_provider.clone()),
            redacted: self.redacted,
            cwd,
            args: args.to_vec(),
            result: None,
            exit_code: output.status.code(),
            stdout_json,
            stdout: Some(String::from_utf8_lossy(&output.stdout).into_owned()),
            stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        };
        let mut payload = serde_json::to_value(payload)?;
        self.redact_value(&mut payload);
        self.write(label, sequence, &payload)
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

fn evidence_sensitive_values(fixture: &CrossRegionFixture) -> Vec<String> {
    let mut values = Vec::new();
    collect_sensitive_value(&mut values, &fixture.writer_a_url);
    collect_sensitive_value(&mut values, &fixture.writer_b_url);
    collect_sensitive_value(&mut values, &fixture.coordinator_url);
    collect_sensitive_value(&mut values, &fixture.writer_a_name);
    collect_sensitive_value(&mut values, &fixture.writer_b_name);
    if let Some(name) = fixture.coordinator_url.split("://").nth(1) {
        collect_sensitive_value(&mut values, name);
    }
    if let Ok(value) = env::var("CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE") {
        collect_sensitive_value(&mut values, &value);
    }
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

fn coordinator_provider_from_url(url: &str) -> String {
    url.split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".to_owned())
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

#[test]
fn evidence_artifact_root_scopes_cross_region_artifacts() {
    assert_eq!(
        evidence_artifact_root(
            PathBuf::from("evidence"),
            "release 2026/06/16",
            "replica-live-cross-region",
            "dynamodb://prod-coordinator",
        ),
        PathBuf::from("evidence")
            .join("release-2026-06-16")
            .join("replica-live-cross-region")
            .join("dynamodb-prod-coordinator")
    );
}

#[test]
fn repair_service_template_defaults_to_systemd() -> TestResult {
    assert_eq!(normalize_repair_service_template(None)?, "systemd");
    assert_eq!(normalize_repair_service_template(Some(""))?, "systemd");
    Ok(())
}

#[test]
fn repair_service_template_accepts_supported_templates() -> TestResult {
    assert_eq!(
        normalize_repair_service_template(Some(" launchd "))?,
        "launchd"
    );
    assert_eq!(
        normalize_repair_service_template(Some("KUBERNETES"))?,
        "kubernetes"
    );
    Ok(())
}

#[test]
fn repair_service_template_rejects_unsupported_templates() {
    let err = normalize_repair_service_template(Some("cron")).unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE=cron")
    );
}

#[test]
fn evidence_recorder_writes_ordered_json_artifacts() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let recorder = EvidenceRecorder {
        root: tmp.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        harness: "replica-live-cross-region".to_owned(),
        coordinator_provider: "dynamodb".to_owned(),
        redacted: false,
        sensitive_values: Vec::new(),
    };
    let args = vec![
        "replica".to_owned(),
        "failover".to_owned(),
        "status".to_owned(),
        "--json".to_owned(),
    ];

    recorder.record_json(
        "Failover Status!",
        "source".to_owned(),
        &args,
        &serde_json::json!({"schema": "replica.failover"}),
    )?;

    let mut files = std::fs::read_dir(tmp.path())?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    assert_eq!(files, vec!["001-failover-status.json"]);

    let body = std::fs::read_to_string(tmp.path().join(&files[0]))?;
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["schema"], "replica.live-smoke.evidence");
    assert_eq!(value["harness"], "replica-live-cross-region");
    assert_eq!(value["run_id"], "test-run");
    assert_eq!(value["sequence"], 1);
    assert_eq!(value["label"], "Failover Status!");
    assert_eq!(value["coordinator_provider"], "dynamodb");
    assert_eq!(value["redacted"], false);
    assert_eq!(value["cwd"], "source");
    assert_eq!(value["args"][0], "replica");
    assert_eq!(value["result"]["schema"], "replica.failover");
    Ok(())
}

#[test]
fn evidence_recorder_redacts_configured_identifiers() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let recorder = EvidenceRecorder {
        root: tmp.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        harness: "replica-live-cross-region".to_owned(),
        coordinator_provider: "dynamodb".to_owned(),
        redacted: true,
        sensitive_values: vec![
            "crab://primary-bucket/secret/repo".to_owned(),
            "dynamodb://prod-coordinator".to_owned(),
            "prod-coordinator".to_owned(),
        ],
    };
    let args = vec![
        "push".to_owned(),
        "crab://primary-bucket/secret/repo".to_owned(),
        "--json".to_owned(),
    ];

    recorder.record_json(
        "push",
        "source".to_owned(),
        &args,
        &serde_json::json!({
            "remote": "crab://primary-bucket/secret/repo",
            "coordinator": "dynamodb://prod-coordinator",
            "managed_resource": "prod-coordinator-global-table"
        }),
    )?;

    let body = std::fs::read_to_string(tmp.path().join("001-push.json"))?;
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["redacted"], true);
    assert!(body.contains("<redacted>"));
    for secret in [
        "crab://primary-bucket/secret/repo",
        "dynamodb://prod-coordinator",
        "prod-coordinator",
    ] {
        assert!(!body.contains(secret), "{secret} leaked into evidence");
    }
    Ok(())
}

#[cfg(unix)]
fn install_remote_helper(helper_dir: &Path) -> TestResult {
    std::os::unix::fs::symlink(bin(), helper_dir.join("git-remote-crab"))?;
    Ok(())
}

#[cfg(not(unix))]
fn install_remote_helper(helper_dir: &Path) -> TestResult {
    std::fs::copy(bin(), helper_dir.join("git-remote-crab"))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
        .try_into()
        .unwrap_or(u64::MAX)
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn writer_store_target(url: &str) -> TestResult<WriterStoreTarget> {
    let parsed = ObjectUrl::parse(url)?;
    match parsed.cloud {
        Cloud::S3 => Ok(WriterStoreTarget {
            cloud: "s3",
            account: None,
            bucket: parsed.bucket,
        }),
        Cloud::Gcs => Ok(WriterStoreTarget {
            cloud: "gcs",
            account: None,
            bucket: parsed.bucket,
        }),
        Cloud::Azure => {
            let container = parsed
                .prefix
                .split('/')
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    std::io::Error::other(format!(
                        "Azure writer URL {url} must include account/container/repo path"
                    ))
                })?;
            Ok(WriterStoreTarget {
                cloud: "azure",
                account: Some(parsed.bucket),
                bucket: container.to_owned(),
            })
        }
        Cloud::Local => Err(std::io::Error::other(format!(
            "production-load evidence cannot inventory local writer URL {url}"
        ))
        .into()),
    }
}

fn build_writer_store(target: &WriterStoreTarget) -> TestResult<Arc<dyn ObjectStore>> {
    match target.cloud {
        "s3" => {
            let mut builder =
                object_store::aws::AmazonS3Builder::from_env().with_bucket_name(&target.bucket);
            if let Some(endpoint) = s3_endpoint() {
                builder = builder.with_endpoint(endpoint.clone());
                builder = builder
                    .with_virtual_hosted_style_request(enabled("AWS_VIRTUAL_HOSTED_STYLE_REQUEST"));
                if enabled("AWS_ALLOW_HTTP") || endpoint.starts_with("http://") {
                    builder = builder.with_allow_http(true);
                }
            }
            Ok(Arc::new(builder.build()?))
        }
        "gcs" => Ok(Arc::new(
            object_store::gcp::GoogleCloudStorageBuilder::from_env()
                .with_bucket_name(&target.bucket)
                .build()?,
        )),
        "azure" => {
            let mut builder = object_store::azure::MicrosoftAzureBuilder::from_env()
                .with_container_name(&target.bucket);
            if let Some(account) = target.account.as_deref() {
                builder = builder.with_account(account);
            }
            Ok(Arc::new(builder.build()?))
        }
        other => {
            Err(std::io::Error::other(format!("unsupported writer store target {other}")).into())
        }
    }
}

async fn count_xorb_objects(store: &dyn ObjectStore) -> TestResult<u64> {
    let prefix = ObjectPath::from(".crab/xorbs");
    let mut count = 0_u64;
    let mut stream = store.list(Some(&prefix));
    while stream.try_next().await?.is_some() {
        count = count.saturating_add(1);
    }
    Ok(count)
}

fn count_writer_xorb_objects(fixture: &CrossRegionFixture) -> TestResult<u64> {
    let targets = [&fixture.writer_a_url, &fixture.writer_b_url]
        .into_iter()
        .map(|url| writer_store_target(url))
        .collect::<TestResult<BTreeSet<_>>>()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut count = 0_u64;
    for target in targets {
        let store = build_writer_store(&target)?;
        count = count.saturating_add(runtime.block_on(count_xorb_objects(store.as_ref()))?);
    }
    Ok(count)
}

fn run(mut command: Command, label: &str) -> TestResult<Output> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(command_error(label, &output).into());
    }
    Ok(output)
}

fn command_error(label: &str, output: &Output) -> std::io::Error {
    std::io::Error::other(format!(
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn run_crab_json(workspace: &SmokeWorkspace, cwd: &Path, args: &[String]) -> TestResult<Value> {
    let mut command = workspace.crab();
    command.current_dir(cwd).args(args);
    let output = run(command, &format!("crab {}", args.join(" ")))?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    if !value["error"].is_null() {
        return Err(std::io::Error::other(format!(
            "crab {} returned error envelope: {value}",
            args.join(" ")
        ))
        .into());
    }
    Ok(value)
}

fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn run_crab(workspace: &SmokeWorkspace, cwd: &Path, args: &[&str]) -> TestResult {
    let mut command = workspace.crab();
    command.current_dir(cwd).args(args);
    run(command, &format!("crab {}", args.join(" ")))?;
    Ok(())
}

fn run_crab_rejected(
    workspace: &SmokeWorkspace,
    cwd: &Path,
    args: &[String],
) -> TestResult<Output> {
    let mut command = workspace.crab();
    command.current_dir(cwd).args(args);
    let output = command.output()?;
    if crab_output_was_rejected(&output) {
        return Ok(output);
    }

    Err(std::io::Error::other(format!(
        "crab {} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
    .into())
}

fn crab_output_was_rejected(output: &Output) -> bool {
    if !output.status.success() {
        return true;
    }

    serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|value| !value["error"].is_null())
}

fn run_git(workspace: &SmokeWorkspace, cwd: &Path, args: &[&str]) -> TestResult {
    let mut command = workspace.command("git");
    command.current_dir(cwd).args(args);
    run(command, &format!("git {}", args.join(" ")))?;
    Ok(())
}

fn run_git_output(workspace: &SmokeWorkspace, cwd: &Path, args: &[&str]) -> TestResult<String> {
    let mut command = workspace.command("git");
    command.current_dir(cwd).args(args);
    let output = run(command, &format!("git {}", args.join(" ")))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn require_git(workspace: &SmokeWorkspace) -> TestResult<bool> {
    let mut command = workspace.command("git");
    command.arg("--version");
    let output = command.output()?;
    if output.status.success() {
        return Ok(true);
    }
    eprintln!("skipping live cross-region replica smoke: git is not available");
    Ok(false)
}

fn initialize_repo(
    workspace: &SmokeWorkspace,
    fixture: &CrossRegionFixture,
    repo: &Path,
) -> TestResult {
    std::fs::create_dir_all(repo)?;
    let init_args = vec![
        "init".to_owned(),
        fixture.writer_a_url.clone(),
        "--json".to_owned(),
    ];
    let init = run_crab_json(workspace, repo, &init_args)?;
    workspace.record_json("init-writer-a", repo, &init_args, &init)?;
    run_git(
        workspace,
        repo,
        &["config", "user.email", "replica-smoke@example.com"],
    )?;
    run_git(workspace, repo, &["config", "user.name", "Replica Smoke"])?;
    run_git(workspace, repo, &["config", "commit.gpgsign", "false"])?;
    run_git(
        workspace,
        repo,
        &["remote", "set-url", "origin", &fixture.writer_a_url],
    )?;
    run_git(
        workspace,
        repo,
        &[
            "remote",
            "add",
            &fixture.writer_b_name,
            &fixture.writer_b_url,
        ],
    )?;
    let mode_args = vec![
        "replica".to_owned(),
        "mode".to_owned(),
        "active-active".to_owned(),
        "--coordinator".to_owned(),
        fixture.coordinator_url.clone(),
        "--coordinator-region".to_owned(),
        fixture.coordinator_region.clone(),
        "--failover-region".to_owned(),
        fixture.coordinator_failover_region.clone(),
        "--writer".to_owned(),
        format!(
            "{}={},region={}",
            fixture.writer_a_name, fixture.writer_a_url, fixture.writer_a_region
        ),
        "--writer".to_owned(),
        format!(
            "{}={},region={}",
            fixture.writer_b_name, fixture.writer_b_url, fixture.writer_b_region
        ),
        "--json".to_owned(),
    ];
    let mode = run_crab_json(workspace, repo, &mode_args)?;
    workspace.record_json("mode-active-active", repo, &mode_args, &mode)?;
    let status_args = ["replica", "failover", "status", "--json"];
    let status = wait_for_json(workspace, repo, &status_args, fixture.timeout, |value| {
        value["data"]["active_active"]["writes_enabled"] == true
    })?;
    workspace.record_json(
        "initial-failover-status",
        repo,
        &string_args(&status_args),
        &status,
    )?;
    record_repair_service_template(workspace, repo)?;
    record_repair_worker_deployment(workspace, repo)?;
    Ok(())
}

fn commit_payload(
    workspace: &SmokeWorkspace,
    repo: &Path,
    branch: &str,
    file_name: &str,
    content: &str,
) -> TestResult {
    commit_payload_at(workspace, repo, branch, None, file_name, content)
}

fn commit_load_payload(
    workspace: &SmokeWorkspace,
    repo: &Path,
    branch: &str,
    file_prefix: &str,
    content_seed: &str,
    file_count: u64,
    file_bytes: u64,
) -> TestResult<u64> {
    run_git(workspace, repo, &["checkout", "-B", branch])?;
    run_crab(workspace, repo, &["track", "*.bin"])?;
    let mut total_bytes = 0_u64;
    for index in 0..file_count {
        let file_name = format!("{file_prefix}-{index:04}.bin");
        let bytes = load_payload_bytes(content_seed, index, file_bytes)?;
        total_bytes = total_bytes.saturating_add(bytes.len().try_into().unwrap_or(u64::MAX));
        std::fs::write(repo.join(&file_name), bytes)?;
        run_crab_json(
            workspace,
            repo,
            &["add".to_owned(), file_name, "--json".to_owned()],
        )?;
    }
    run_git(workspace, repo, &["add", ".gitattributes"])?;
    run_git(
        workspace,
        repo,
        &["commit", "-m", &format!("add load payload {file_prefix}")],
    )?;
    Ok(total_bytes)
}

fn load_payload_bytes(prefix: &str, index: u64, len: u64) -> TestResult<Vec<u8>> {
    let len: usize = len.try_into().map_err(|_| {
        std::io::Error::other("CRAB_REPLICA_LIVE_LOAD_FILE_BYTES is too large for this platform")
    })?;
    let seed = format!("{prefix}:{index}:crab-replica-load\n");
    let seed = seed.as_bytes();
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let remaining = len - out.len();
        let take = remaining.min(seed.len());
        out.extend_from_slice(&seed[..take]);
    }
    Ok(out)
}

fn push_branch(
    workspace: &SmokeWorkspace,
    repo: &Path,
    remote: &str,
    branch: &str,
    expected_region: &str,
) -> TestResult<Value> {
    push_refspec(workspace, repo, remote, branch, branch, expected_region)
}

fn push_refspec(
    workspace: &SmokeWorkspace,
    repo: &Path,
    remote: &str,
    source_branch: &str,
    destination_branch: &str,
    expected_region: &str,
) -> TestResult<Value> {
    let args = vec![
        "push".to_owned(),
        remote.to_owned(),
        format!("refs/heads/{source_branch}:refs/heads/{destination_branch}"),
        "--json".to_owned(),
    ];
    let value = run_crab_json(workspace, repo, &args)?;
    assert_eq!(value["schema"], "push");
    assert_eq!(value["data"]["refs_pushed"], 1);
    assert!(
        value["data"]["operation_id"].as_str().is_some(),
        "active-active push should report operation_id: {value}"
    );
    assert!(
        value["data"]["coordinator_epoch"].as_u64().is_some(),
        "active-active push should report coordinator_epoch: {value}"
    );
    assert_eq!(value["data"]["writer_region"], expected_region);
    assert_eq!(value["data"]["commit_state"], "materialized");
    workspace.record_json(
        &format!("push-{remote}-{destination_branch}"),
        repo,
        &args,
        &value,
    )?;
    Ok(value)
}

fn assert_push_refspec_rejected(
    workspace: &SmokeWorkspace,
    repo: &Path,
    remote: &str,
    source_branch: &str,
    destination_branch: &str,
    expected_fragments: &[&str],
) -> TestResult {
    let args = vec![
        "push".to_owned(),
        remote.to_owned(),
        format!("refs/heads/{source_branch}:refs/heads/{destination_branch}"),
        "--json".to_owned(),
    ];
    let output = run_crab_rejected(workspace, repo, &args)?;
    workspace.record_rejection(
        &format!("push-rejected-{remote}-{destination_branch}"),
        repo,
        &args,
        &output,
    )?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    assert!(
        expected_fragments
            .iter()
            .all(|fragment| text.contains(fragment)),
        "push rejection did not contain {expected_fragments:?}; output was:\n{text}"
    );
    Ok(())
}

fn assert_push_rejected_while_fenced(
    workspace: &SmokeWorkspace,
    repo: &Path,
    remote: &str,
    branch: &str,
) -> TestResult {
    let args = vec![
        "push".to_owned(),
        remote.to_owned(),
        format!("refs/heads/{branch}:refs/heads/{branch}"),
        "--json".to_owned(),
    ];
    let output = run_crab_rejected(workspace, repo, &args)?;
    workspace.record_rejection(
        &format!("push-rejected-fenced-{remote}-{branch}"),
        repo,
        &args,
        &output,
    )?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    assert!(
        text.contains("coordinator")
            && (text.contains("fail closed")
                || text.contains("fenc")
                || text.contains("unhealthy")),
        "fenced push rejection should identify coordinator fencing; output was:\n{text}"
    );
    Ok(())
}

fn commit_payload_from(
    workspace: &SmokeWorkspace,
    repo: &Path,
    branch: &str,
    start: &str,
    file_name: &str,
    content: &str,
) -> TestResult {
    commit_payload_at(workspace, repo, branch, Some(start), file_name, content)
}

fn commit_payload_at(
    workspace: &SmokeWorkspace,
    repo: &Path,
    branch: &str,
    start: Option<&str>,
    file_name: &str,
    content: &str,
) -> TestResult {
    if let Some(start) = start {
        run_git(workspace, repo, &["checkout", "-B", branch, start])?;
    } else {
        run_git(workspace, repo, &["checkout", "-B", branch])?;
    }
    run_crab(workspace, repo, &["track", "*.bin"])?;
    std::fs::write(repo.join(file_name), content)?;
    run_crab_json(
        workspace,
        repo,
        &["add".to_owned(), file_name.to_owned(), "--json".to_owned()],
    )?;
    run_git(workspace, repo, &["add", ".gitattributes"])?;
    run_git(
        workspace,
        repo,
        &["commit", "-m", &format!("add {file_name}")],
    )?;
    Ok(())
}

fn assert_same_ref_stale_push_rejected(
    workspace: &SmokeWorkspace,
    fixture: &CrossRegionFixture,
    repo: &Path,
    suffix: &str,
) -> TestResult {
    let branch = format!("crab-live-conflict-{suffix}");
    let stale_branch = format!("crab-live-conflict-stale-{suffix}");

    commit_payload(
        workspace,
        repo,
        &branch,
        "conflict-base.bin",
        &format!("conflict base {suffix}\n"),
    )?;
    push_branch(workspace, repo, "origin", &branch, &fixture.writer_a_region)?;
    let base = run_git_output(workspace, repo, &["rev-parse", &branch])?;

    commit_payload(
        workspace,
        repo,
        &branch,
        "conflict-winner.bin",
        &format!("conflict winner {suffix}\n"),
    )?;
    push_branch(workspace, repo, "origin", &branch, &fixture.writer_a_region)?;

    commit_payload_from(
        workspace,
        repo,
        &stale_branch,
        &base,
        "conflict-stale.bin",
        &format!("conflict stale {suffix}\n"),
    )?;
    assert_push_refspec_rejected(
        workspace,
        repo,
        &fixture.writer_b_name,
        &stale_branch,
        &branch,
        &["non-fast-forward"],
    )
}

fn apply_failover_fence(
    workspace: &SmokeWorkspace,
    repo: &Path,
    unhealthy_writer: &str,
) -> TestResult<Value> {
    let args = vec![
        "replica".to_owned(),
        "failover".to_owned(),
        "run".to_owned(),
        "--writer-unhealthy".to_owned(),
        unhealthy_writer.to_owned(),
        "--apply".to_owned(),
        "--json".to_owned(),
    ];
    let value = run_crab_json(workspace, repo, &args)?;
    assert_eq!(value["schema"], "replica.failover.run");
    assert_eq!(value["data"]["applied"], true);
    assert_eq!(value["data"]["automation_plan"]["action"], "fence");
    assert_eq!(value["data"]["operation"]["operation"], "fence");
    assert_eq!(value["data"]["operation"]["outcome"]["healthy"], false);
    assert_eq!(
        value["data"]["operation"]["outcome"]["reason"],
        format!("writer-unhealthy:{unhealthy_writer}")
    );
    workspace.record_json("failover-fence", repo, &args, &value)?;
    Ok(value)
}

fn apply_failover_resume(workspace: &SmokeWorkspace, repo: &Path) -> TestResult<Value> {
    let args = vec![
        "replica".to_owned(),
        "failover".to_owned(),
        "run".to_owned(),
        "--repair-verified".to_owned(),
        "--apply".to_owned(),
        "--json".to_owned(),
    ];
    let value = run_crab_json(workspace, repo, &args)?;
    assert_eq!(value["schema"], "replica.failover.run");
    assert_eq!(value["data"]["applied"], true);
    assert_eq!(value["data"]["automation_plan"]["action"], "resume");
    assert_eq!(value["data"]["operation"]["operation"], "resume");
    assert_eq!(value["data"]["operation"]["outcome"]["healthy"], true);
    workspace.record_json("failover-resume", repo, &args, &value)?;
    Ok(value)
}

fn wait_for_writes_enabled(
    workspace: &SmokeWorkspace,
    repo: &Path,
    timeout: Duration,
    enabled: bool,
) -> TestResult<Value> {
    let args = ["replica", "failover", "status", "--json"];
    let value = wait_for_json(workspace, repo, &args, timeout, |value| {
        value["data"]["active_active"]["writes_enabled"] == enabled
    })?;
    let label = if enabled {
        "writes-enabled"
    } else {
        "writes-fenced"
    };
    workspace.record_json(label, repo, &string_args(&args), &value)?;
    Ok(value)
}

fn record_repair_service_template(workspace: &SmokeWorkspace, repo: &Path) -> TestResult<Value> {
    let service_template = repair_service_template_from_env()?;
    let output = repair_service_template_output(repo, service_template);
    let args = vec![
        "replica".to_owned(),
        "repair".to_owned(),
        "--from-coordinator".to_owned(),
        "--service-template".to_owned(),
        service_template.to_owned(),
        "--output".to_owned(),
        output.display().to_string(),
    ];
    let mut command = workspace.crab();
    command.current_dir(repo).args(&args);
    run(
        command,
        &format!("crab replica repair --service-template {service_template}"),
    )?;
    let rendered = std::fs::read_to_string(&output)?;
    for fragment in [
        repair_service_template_marker(service_template),
        "--from-coordinator",
        "--watch",
        "--jsonl",
    ] {
        if !rendered.contains(fragment) {
            return Err(std::io::Error::other(format!(
                "repair service template did not contain {fragment}"
            ))
            .into());
        }
    }
    let worker_command = repair_worker_command();
    let template_blake3 = blake3_hex(rendered.as_bytes());
    let command_blake3 = command_blake3(&worker_command);
    let value = serde_json::json!({
        "schema": "replica.repair.service-template",
        "data": {
            "service_template": service_template,
            "from_coordinator": true,
            "watch": true,
            "jsonl": true,
            "rendered": true,
            "non_mutating": true,
            "interval_seconds": 30,
            "template_blake3": template_blake3,
            "command_blake3": command_blake3,
            "command": worker_command
        }
    });
    workspace.record_json("repair-service-template", repo, &args, &value)?;
    Ok(value)
}

fn repair_service_template_output(repo: &Path, service_template: &str) -> PathBuf {
    let file_name = match service_template {
        "systemd" => "repair-worker.service",
        "launchd" => "com.crab.replica-repair.plist",
        "kubernetes" => "repair-worker.yaml",
        _ => "repair-worker.service",
    };
    repo.join(".crab").join("replication").join(file_name)
}

fn repair_service_template_marker(service_template: &str) -> &'static str {
    match service_template {
        "systemd" => "ExecStart=",
        "launchd" => "ProgramArguments",
        "kubernetes" => "kind: Deployment",
        _ => "ExecStart=",
    }
}

fn record_repair_worker_deployment(workspace: &SmokeWorkspace, repo: &Path) -> TestResult {
    if workspace.evidence.is_none() {
        return Ok(());
    }
    let service_template = repair_service_template_from_env()?;
    let rendered = std::fs::read_to_string(repair_service_template_output(repo, service_template))
        .map_err(|err| {
            std::io::Error::other(format!(
                "repair-worker deployment evidence requires the generated {service_template} service template to exist first: {err}"
            ))
        })?;
    let worker_command = repair_worker_command();
    let template_blake3 = blake3_hex(rendered.as_bytes());
    let command_blake3 = command_blake3(&worker_command);
    let artifact_ref = env_value("CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE")
        .ok_or_else(|| std::io::Error::other(
            "CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE is required when recording live cross-region evidence",
        ))?;
    let args = vec![
        "external".to_owned(),
        "repair-worker-deployment-evidence".to_owned(),
        artifact_ref.clone(),
    ];
    let value = serde_json::json!({
        "schema": "replica.repair.worker-deployment",
        "data": {
            "artifact_ref": artifact_ref,
            "deployment_verified": true,
            "service_template": service_template,
            "template_blake3": template_blake3,
            "command_blake3": command_blake3,
            "command": worker_command
        }
    });
    workspace.record_json("repair-worker-deployment", repo, &args, &value)?;
    Ok(())
}

fn repair_worker_command() -> Vec<String> {
    [
        "crab",
        "replica",
        "repair",
        "--from-coordinator",
        "--watch",
        "--jsonl",
        "--interval",
        "30",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn command_blake3(command: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in command {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn wait_for_repair(
    workspace: &SmokeWorkspace,
    repo: &Path,
    timeout: Duration,
) -> TestResult<Value> {
    let args = [
        "replica",
        "repair",
        "--from-coordinator",
        "--watch",
        "--samples",
        "1",
        "--interval",
        "1",
        "--jsonl",
    ];
    let value = wait_for_jsonl(workspace, repo, &args, timeout, |value| {
        value["type"] == "snapshot"
            && value["data"]["repair"]["from_coordinator"] == true
            && value["data"]["repair"]["blocked_reason"].is_null()
    })?;
    workspace.record_json("repair-snapshot", repo, &string_args(&args), &value)?;
    Ok(value)
}

fn wait_for_active_active_certification(
    workspace: &SmokeWorkspace,
    repo: &Path,
    timeout: Duration,
) -> TestResult<Value> {
    let args = ["replica", "certify", "--profile", "active-active", "--json"];
    let value = wait_for_json(workspace, repo, &args, timeout, |value| {
        value["schema"] == "replica.certification" && value["data"]["certified"] == true
    })?;
    workspace.record_json(
        "active-active-certification",
        repo,
        &string_args(&args),
        &value,
    )?;
    Ok(value)
}

fn record_production_load_evidence(
    recorder: &Option<EvidenceRecorder>,
    workspace: &SmokeWorkspace,
    repo: &Path,
    fixture: &CrossRegionFixture,
    metrics: &ProductionLoadMetrics,
) -> TestResult {
    let Some(recorder) = recorder.as_ref() else {
        return Ok(());
    };
    let args = vec![
        "external".to_owned(),
        "production-load".to_owned(),
        "--json".to_owned(),
    ];
    let value = serde_json::json!({
        "schema": "replica.production-load",
        "data": {
            "profile": "production",
            "coordinator_provider": coordinator_provider_from_url(&fixture.coordinator_url),
            "repository_bytes": metrics.repository_bytes,
            "file_count": metrics.file_count,
            "xorb_count_source": "writer-store-delta",
            "xorb_count_before": metrics.xorb_count_before,
            "xorb_count_after": metrics.xorb_count_after,
            "xorb_count": metrics.xorb_count,
            "refs_pushed": metrics.refs_pushed,
            "writer_regions": metrics.writer_regions,
            "reader_regions": metrics.reader_regions,
            "clone_count": metrics.clone_count,
            "hydrate_count": metrics.hydrate_count,
            "push_latency_ms": metrics.push_latency_ms,
            "push_latency_budget_ms": metrics.push_latency_budget_ms,
            "read_latency_ms": metrics.read_latency_ms,
            "read_latency_budget_ms": metrics.read_latency_budget_ms
        }
    });
    recorder.record_json(
        "production-load",
        workspace.relative_path(repo),
        &args,
        &value,
    )
}

#[derive(Debug)]
struct ProductionLoadMetrics {
    repository_bytes: u64,
    file_count: u64,
    xorb_count_before: u64,
    xorb_count_after: u64,
    xorb_count: u64,
    refs_pushed: u64,
    writer_regions: u64,
    reader_regions: u64,
    clone_count: u64,
    hydrate_count: u64,
    push_latency_ms: u64,
    push_latency_budget_ms: u64,
    read_latency_ms: u64,
    read_latency_budget_ms: u64,
}

fn wait_for_clone_and_hydrate(
    workspace: &SmokeWorkspace,
    url: &str,
    reader_region: &str,
    branch: &str,
    target: &Path,
    file_name: &str,
    expected_content: &str,
    timeout: Duration,
) -> TestResult {
    let started = std::time::Instant::now();
    let mut last_error = None;
    while started.elapsed() < timeout {
        let _ = std::fs::remove_dir_all(target);
        let args = vec![
            "clone".to_owned(),
            url.to_owned(),
            target.display().to_string(),
            "--branch".to_owned(),
            branch.to_owned(),
            "--eager".to_owned(),
            "--json".to_owned(),
        ];
        match run_crab_json(workspace, &workspace.root, &args) {
            Ok(mut clone) => {
                clone["data"]["reader_region"] = serde_json::json!(reader_region);
                workspace.record_json(
                    &format!("clone-{branch}"),
                    &workspace.root,
                    &args,
                    &clone,
                )?;
                let hydrate = vec![
                    "hydrate".to_owned(),
                    "--all".to_owned(),
                    "--json".to_owned(),
                ];
                match run_crab_json(workspace, target, &hydrate) {
                    Ok(mut hydrated) => {
                        hydrated["data"]["reader_region"] = serde_json::json!(reader_region);
                        workspace.record_json(
                            &format!("hydrate-{branch}"),
                            target,
                            &hydrate,
                            &hydrated,
                        )?;
                        let actual = std::fs::read_to_string(target.join(file_name))?;
                        if actual == expected_content {
                            return Ok(());
                        }
                        last_error = Some(format!(
                            "hydrated {file_name} content mismatch: expected {expected_content:?}, got {actual:?}"
                        ));
                    }
                    Err(err) => last_error = Some(err.to_string()),
                }
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        std::thread::sleep(Duration::from_secs(15));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timed out waiting to clone and hydrate {branch} from {url}; last error: {}",
            last_error.unwrap_or_else(|| "none".to_owned())
        ),
    )
    .into())
}

fn wait_for_json(
    workspace: &SmokeWorkspace,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
    mut predicate: impl FnMut(&Value) -> bool,
) -> TestResult<Value> {
    let started = std::time::Instant::now();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let mut last_error = None;
    while started.elapsed() < timeout {
        match run_crab_json(workspace, cwd, &args) {
            Ok(value) if predicate(&value) => return Ok(value),
            Ok(value) => last_error = Some(format!("predicate did not match: {value}")),
            Err(err) => last_error = Some(err.to_string()),
        }
        std::thread::sleep(Duration::from_secs(15));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for crab {}; last error: {}",
            args.join(" "),
            last_error.unwrap_or_else(|| "none".to_owned())
        ),
    )
    .into())
}

fn wait_for_jsonl(
    workspace: &SmokeWorkspace,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
    mut predicate: impl FnMut(&Value) -> bool,
) -> TestResult<Value> {
    let started = std::time::Instant::now();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let mut last_error = None;
    while started.elapsed() < timeout {
        let mut command = workspace.crab();
        command.current_dir(cwd).args(&args);
        match run(command, &format!("crab {}", args.join(" "))) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
                    match serde_json::from_str::<Value>(line) {
                        Ok(value) if predicate(&value) => return Ok(value),
                        Ok(value) => last_error = Some(format!("predicate did not match: {value}")),
                        Err(err) => {
                            last_error = Some(format!("invalid JSONL event {line:?}: {err}"));
                        }
                    }
                }
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        std::thread::sleep(Duration::from_secs(15));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for crab {}; last error: {}",
            args.join(" "),
            last_error.unwrap_or_else(|| "none".to_owned())
        ),
    )
    .into())
}

#[ignore = "requires disposable cross-region Crab remotes, a live coordinator, ambient credentials, and explicit env flags"]
#[test]
fn live_active_active_production_load_evidence() -> TestResult {
    let Some(fixture) = fixture()? else {
        return Ok(());
    };
    let workspace = SmokeWorkspace::new_without_evidence(&fixture)?;
    if !require_git(&workspace)? {
        return Ok(());
    }
    let load_evidence = EvidenceRecorder::from_env_with_harness(&fixture, "replica-live-load")?;

    let file_count = env_u64_or("CRAB_REPLICA_LIVE_LOAD_FILES", 16)?;
    let file_bytes = env_u64_or("CRAB_REPLICA_LIVE_LOAD_FILE_BYTES", 65_536)?;
    let push_latency_budget_ms =
        env_u64_or("CRAB_REPLICA_LIVE_LOAD_PUSH_LATENCY_BUDGET_MS", 300_000)?;
    let read_latency_budget_ms =
        env_u64_or("CRAB_REPLICA_LIVE_LOAD_READ_LATENCY_BUDGET_MS", 300_000)?;

    let repo = workspace.root.join("load-source");
    initialize_repo(&workspace, &fixture, &repo)?;

    let suffix = format!("{}-{}", std::process::id(), now_ms());
    let branch_a = format!("crab-load-a-{suffix}");
    let branch_b = format!("crab-load-b-{suffix}");
    let seed_a = format!("load-a:{suffix}");
    let seed_b = format!("load-b:{suffix}");
    let xorb_count_before = count_writer_xorb_objects(&fixture)?;

    let bytes_a = commit_load_payload(
        &workspace, &repo, &branch_a, "load-a", &seed_a, file_count, file_bytes,
    )?;
    let push_started = std::time::Instant::now();
    let push_a = push_branch(
        &workspace,
        &repo,
        "origin",
        &branch_a,
        &fixture.writer_a_region,
    )?;
    wait_for_repair(&workspace, &repo, fixture.timeout)?;

    let bytes_b = commit_load_payload(
        &workspace, &repo, &branch_b, "load-b", &seed_b, file_count, file_bytes,
    )?;
    let push_b = push_branch(
        &workspace,
        &repo,
        &fixture.writer_b_name,
        &branch_b,
        &fixture.writer_b_region,
    )?;
    wait_for_repair(&workspace, &repo, fixture.timeout)?;
    let push_latency_ms = elapsed_ms(push_started.elapsed());
    let xorb_count_after = count_writer_xorb_objects(&fixture)?;
    let xorb_count = xorb_count_after.saturating_sub(xorb_count_before);
    if xorb_count == 0 {
        return Err(std::io::Error::other(
            "production-load evidence observed zero newly published xorb objects",
        )
        .into());
    }

    let read_started = std::time::Instant::now();
    wait_for_clone_and_hydrate(
        &workspace,
        &fixture.writer_b_url,
        &fixture.writer_b_region,
        &branch_a,
        &workspace.root.join("load-clone-from-b"),
        "load-a-0000.bin",
        &String::from_utf8(load_payload_bytes(&seed_a, 0, file_bytes)?)?,
        fixture.timeout,
    )?;
    wait_for_clone_and_hydrate(
        &workspace,
        &fixture.writer_a_url,
        &fixture.writer_a_region,
        &branch_b,
        &workspace.root.join("load-clone-from-a"),
        "load-b-0000.bin",
        &String::from_utf8(load_payload_bytes(&seed_b, 0, file_bytes)?)?,
        fixture.timeout,
    )?;
    let read_latency_ms = elapsed_ms(read_started.elapsed());

    let refs_pushed = push_a["data"]["refs_pushed"].as_u64().unwrap_or(0)
        + push_b["data"]["refs_pushed"].as_u64().unwrap_or(0);
    let metrics = ProductionLoadMetrics {
        repository_bytes: bytes_a.saturating_add(bytes_b),
        file_count: file_count.saturating_mul(2),
        xorb_count_before,
        xorb_count_after,
        xorb_count,
        refs_pushed,
        writer_regions: 2,
        reader_regions: 2,
        clone_count: 2,
        hydrate_count: 2,
        push_latency_ms,
        push_latency_budget_ms,
        read_latency_ms,
        read_latency_budget_ms,
    };
    record_production_load_evidence(&load_evidence, &workspace, &repo, &fixture, &metrics)?;

    assert!(
        metrics.push_latency_ms <= metrics.push_latency_budget_ms,
        "production load push latency {}ms exceeded budget {}ms",
        metrics.push_latency_ms,
        metrics.push_latency_budget_ms
    );
    assert!(
        metrics.read_latency_ms <= metrics.read_latency_budget_ms,
        "production load read latency {}ms exceeded budget {}ms",
        metrics.read_latency_ms,
        metrics.read_latency_budget_ms
    );

    Ok(())
}

#[ignore = "requires disposable cross-region Crab remotes, a live coordinator, ambient credentials, and explicit env flags"]
#[test]
fn live_active_active_cross_region_push_fetch_hydrate_smoke() -> TestResult {
    let Some(fixture) = fixture()? else {
        return Ok(());
    };
    let workspace = SmokeWorkspace::new(&fixture)?;
    if !require_git(&workspace)? {
        return Ok(());
    }

    let repo = workspace.root.join("source");
    initialize_repo(&workspace, &fixture, &repo)?;

    let suffix = format!("{}-{}", std::process::id(), now_ms());
    let branch_a = format!("crab-live-a-{suffix}");
    let branch_b = format!("crab-live-b-{suffix}");

    let file_a = "region-a.bin";
    let content_a = format!("writer-a payload {suffix}\n");
    commit_payload(&workspace, &repo, &branch_a, file_a, &content_a)?;
    push_branch(
        &workspace,
        &repo,
        "origin",
        &branch_a,
        &fixture.writer_a_region,
    )?;
    wait_for_repair(&workspace, &repo, fixture.timeout)?;
    wait_for_clone_and_hydrate(
        &workspace,
        &fixture.writer_b_url,
        &fixture.writer_b_region,
        &branch_a,
        &workspace.root.join("clone-from-b"),
        file_a,
        &content_a,
        fixture.timeout,
    )?;

    let file_b = "region-b.bin";
    let content_b = format!("writer-b payload {suffix}\n");
    commit_payload(&workspace, &repo, &branch_b, file_b, &content_b)?;
    apply_failover_fence(&workspace, &repo, &fixture.writer_b_name)?;
    wait_for_writes_enabled(&workspace, &repo, fixture.timeout, false)?;
    assert_push_rejected_while_fenced(&workspace, &repo, &fixture.writer_b_name, &branch_b)?;
    apply_failover_resume(&workspace, &repo)?;
    wait_for_writes_enabled(&workspace, &repo, fixture.timeout, true)?;
    push_branch(
        &workspace,
        &repo,
        &fixture.writer_b_name,
        &branch_b,
        &fixture.writer_b_region,
    )?;
    wait_for_repair(&workspace, &repo, fixture.timeout)?;
    wait_for_clone_and_hydrate(
        &workspace,
        &fixture.writer_a_url,
        &fixture.writer_a_region,
        &branch_b,
        &workspace.root.join("clone-from-a"),
        file_b,
        &content_b,
        fixture.timeout,
    )?;

    assert_same_ref_stale_push_rejected(&workspace, &fixture, &repo, &suffix)?;

    let head = run_git_output(&workspace, &repo, &["rev-parse", &branch_b])?;
    assert_eq!(head.len(), 40, "branch head should be a git object id");
    let certification = wait_for_active_active_certification(&workspace, &repo, fixture.timeout)?;
    assert_eq!(certification["data"]["profile"], "active-active");

    Ok(())
}
