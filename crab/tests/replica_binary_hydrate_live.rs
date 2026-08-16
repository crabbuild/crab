//! Ignored provider-backed binary hydrate proof for live object storage.
//!
//! These tests mutate disposable S3/GCS/Azure buckets or containers. They drive
//! the real `crab` binary through init, add, push, replica setup, read cutover,
//! and hydrate. The primary's newly uploaded xorb objects are deleted after the
//! replica is read-ready so a primary-routed hydrate fails while a
//! selected-replica hydrate succeeds.

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
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crab::cmd::replica::{
    LiveEvidenceSchemaVersion, LiveSmokeEvidencePayload, LiveSmokeEvidenceSchema,
};
use futures_util::TryStreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const REPLICA_NAME: &str = "west";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveProvider {
    S3,
    Gcs,
    Azure,
}

#[derive(Debug, Clone)]
struct Harness {
    provider: LiveProvider,
    primary: ObjectTarget,
    replica: ObjectTarget,
    region: String,
}

#[derive(Debug, Clone)]
struct ObjectTarget {
    account: Option<String>,
    bucket: String,
}

struct Workspace {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    path_env: OsString,
    push_cache: PathBuf,
    hydrate_cache: PathBuf,
}

struct EvidenceRecorder {
    root: PathBuf,
    sequence: AtomicU64,
    run_id: String,
    provider: LiveProvider,
    redacted: bool,
    sensitive_values: Vec<String>,
}

struct LiveStores {
    primary: Arc<dyn ObjectStore>,
    replica: Arc<dyn ObjectStore>,
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn evidence_run_id(harness: &str) -> String {
    env::var("CRAB_REPLICA_LIVE_RUN_ID")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{harness}-{}-{}", std::process::id(), now_ms()))
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| env_value(name))
}

fn env_bool(name: &str) -> Option<bool> {
    env_value(name).and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    })
}

fn require_live(provider: LiveProvider) -> bool {
    let flag = provider.env("HYDRATE");
    if enabled("CRAB_REPLICA_LIVE") && enabled("CRAB_REPLICA_LIVE_MUTATE") && enabled(&flag) {
        return true;
    }
    eprintln!(
        "skipping {} binary hydrate replica proof: set CRAB_REPLICA_LIVE=1, CRAB_REPLICA_LIVE_MUTATE=1, and {flag}=1",
        provider.label()
    );
    false
}

impl LiveProvider {
    fn label(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::Gcs => "GCS",
            Self::Azure => "Azure",
        }
    }

    fn config_provider(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
        }
    }

    fn replica_scheme(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gs",
            Self::Azure => "azure",
        }
    }

    fn env(self, suffix: &str) -> String {
        format!(
            "CRAB_REPLICA_LIVE_{}_{}",
            self.label().to_ascii_uppercase(),
            suffix
        )
    }
}

fn harness(provider: LiveProvider) -> Option<Harness> {
    if !require_live(provider) {
        return None;
    }

    let primary = target_from_env(provider, "PRIMARY")?;
    let replica = target_from_env(provider, "REPLICA")?;
    let region = env_value(&provider.env("HYDRATE_REGION"))
        .or_else(|| env_value("AWS_REGION"))
        .or_else(|| env_value("GOOGLE_CLOUD_REGION"))
        .or_else(|| env_value("AZURE_REGION"))
        .unwrap_or_else(|| "global".to_owned());

    Some(Harness {
        provider,
        primary,
        replica,
        region,
    })
}

fn target_from_env(provider: LiveProvider, role: &str) -> Option<ObjectTarget> {
    let label = provider.label().to_ascii_uppercase();
    let bucket = match provider {
        LiveProvider::Azure => env_any(&[
            &format!("CRAB_REPLICA_LIVE_{label}_HYDRATE_{role}_CONTAINER"),
            &format!("CRAB_REPLICA_LIVE_{label}_HYDRATE_{role}_BUCKET"),
        ]),
        LiveProvider::S3 | LiveProvider::Gcs => {
            env_value(&format!("CRAB_REPLICA_LIVE_{label}_HYDRATE_{role}_BUCKET"))
        }
    };
    let Some(bucket) = bucket else {
        let noun = if provider == LiveProvider::Azure {
            "CONTAINER"
        } else {
            "BUCKET"
        };
        eprintln!(
            "skipping {} binary hydrate replica proof: set CRAB_REPLICA_LIVE_{label}_HYDRATE_{role}_{noun}",
            provider.label()
        );
        return None;
    };

    let account = if provider == LiveProvider::Azure {
        env_any(&[
            &format!("CRAB_REPLICA_LIVE_{label}_HYDRATE_{role}_ACCOUNT"),
            "AZURE_STORAGE_ACCOUNT",
        ])
    } else {
        None
    };

    if provider == LiveProvider::Azure && account.is_none() {
        eprintln!(
            "skipping Azure binary hydrate replica proof: set CRAB_REPLICA_LIVE_AZURE_HYDRATE_{role}_ACCOUNT or AZURE_STORAGE_ACCOUNT"
        );
        return None;
    }

    Some(ObjectTarget { account, bucket })
}

impl Workspace {
    fn new() -> TestResult<Self> {
        let tmp = tempfile::tempdir()?;
        let helper_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&helper_dir)?;
        install_remote_helper(&helper_dir)?;

        let mut paths = vec![helper_dir];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        let path_env = env::join_paths(paths)?;

        let push_cache = tmp.path().join("push-cache");
        let hydrate_cache = tmp.path().join("hydrate-cache");
        std::fs::create_dir_all(&push_cache)?;
        std::fs::create_dir_all(&hydrate_cache)?;

        Ok(Self {
            root: tmp.path().to_path_buf(),
            _tmp: tmp,
            path_env,
            push_cache,
            hydrate_cache,
        })
    }

    fn command(&self, program: &str, harness: &Harness) -> Command {
        let mut command = Command::new(program);
        self.apply_env(&mut command, harness, &self.push_cache);
        command
    }

    fn crab(&self, harness: &Harness, cache: &Path) -> Command {
        let mut command = Command::new(bin());
        self.apply_env(&mut command, harness, cache);
        command
    }

    fn apply_env(&self, command: &mut Command, harness: &Harness, cache: &Path) {
        command
            .env("PATH", &self.path_env)
            .env("CRAB_STORAGE_PROVIDER", harness.provider.config_provider())
            .env("CRAB_CACHE_DIR", cache);

        match harness.provider {
            LiveProvider::S3 => {
                if harness.region != "global" {
                    command.env("AWS_REGION", &harness.region);
                }
                apply_s3_child_env(command);
            }
            LiveProvider::Gcs => {}
            LiveProvider::Azure => {
                if let Some(account) = harness.primary.account.as_deref() {
                    command.env("AZURE_STORAGE_ACCOUNT", account);
                }
            }
        }
    }
}

impl EvidenceRecorder {
    fn from_env(harness: &Harness) -> TestResult<Option<Self>> {
        let Some(root) = env::var_os("CRAB_REPLICA_LIVE_EVIDENCE_DIR") else {
            return Ok(None);
        };
        let run_id = evidence_run_id("replica-binary-hydrate-live");
        let root = evidence_artifact_root(
            PathBuf::from(root),
            &run_id,
            "replica-binary-hydrate-live",
            harness.provider.config_provider(),
        );
        std::fs::create_dir_all(&root)?;
        Ok(Some(Self {
            root,
            sequence: AtomicU64::new(1),
            run_id,
            provider: harness.provider,
            redacted: enabled("CRAB_REPLICA_LIVE_EVIDENCE_REDACT"),
            sensitive_values: evidence_sensitive_values(harness),
        }))
    }

    fn record_json(&self, label: &str, cwd: &Path, args: &[String], value: &Value) -> TestResult {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let payload = LiveSmokeEvidencePayload {
            schema: LiveSmokeEvidenceSchema::ReplicaLiveSmokeEvidence,
            version: LiveEvidenceSchemaVersion::V1,
            collected_at_ms: now_ms().try_into().unwrap_or(u64::MAX),
            harness: Some("replica-binary-hydrate-live".to_owned()),
            run_id: Some(self.run_id.clone()),
            sequence: Some(sequence),
            label: label.to_owned(),
            provider: Some(self.provider.config_provider().to_owned()),
            coordinator_provider: None,
            redacted: self.redacted,
            cwd: cwd.display().to_string(),
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

fn evidence_sensitive_values(harness: &Harness) -> Vec<String> {
    let mut values = Vec::new();
    collect_sensitive_value(&mut values, &harness.primary.bucket);
    collect_sensitive_value(&mut values, &harness.replica.bucket);
    if let Some(account) = harness.primary.account.as_deref() {
        collect_sensitive_value(&mut values, account);
    }
    if let Some(account) = harness.replica.account.as_deref() {
        collect_sensitive_value(&mut values, account);
    }
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
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

fn record_evidence(
    evidence: &Option<EvidenceRecorder>,
    label: &str,
    cwd: &Path,
    args: &[String],
    value: Value,
) -> TestResult {
    if let Some(evidence) = evidence.as_ref() {
        evidence.record_json(label, cwd, args, &value)?;
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

fn apply_s3_child_env(command: &mut Command) {
    if let Some(endpoint) = s3_endpoint() {
        command
            .env("AWS_ENDPOINT_URL", &endpoint)
            .env("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", "false");
        if endpoint.starts_with("http://") {
            command.env("AWS_ALLOW_HTTP", "true");
        }
    }
}

fn s3_endpoint() -> Option<String> {
    env_value("CRAB_REPLICA_LIVE_S3_HYDRATE_ENDPOINT").or_else(|| env_value("AWS_ENDPOINT_URL"))
}

fn build_store(provider: LiveProvider, target: &ObjectTarget) -> TestResult<Arc<dyn ObjectStore>> {
    match provider {
        LiveProvider::S3 => {
            let mut builder =
                object_store::aws::AmazonS3Builder::from_env().with_bucket_name(&target.bucket);
            if let Some(endpoint) = s3_endpoint() {
                builder = builder.with_endpoint(endpoint);
                if env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST").unwrap_or(false) {
                    builder = builder.with_virtual_hosted_style_request(true);
                } else {
                    builder = builder.with_virtual_hosted_style_request(false);
                }
                if env_bool("AWS_ALLOW_HTTP").unwrap_or_else(|| {
                    s3_endpoint().is_some_and(|endpoint| endpoint.starts_with("http://"))
                }) {
                    builder = builder.with_allow_http(true);
                }
            }
            Ok(Arc::new(builder.build()?))
        }
        LiveProvider::Gcs => {
            let store = object_store::gcp::GoogleCloudStorageBuilder::from_env()
                .with_bucket_name(&target.bucket)
                .build()?;
            Ok(Arc::new(store))
        }
        LiveProvider::Azure => {
            let mut builder = object_store::azure::MicrosoftAzureBuilder::from_env()
                .with_container_name(&target.bucket);
            if let Some(account) = target.account.as_deref() {
                builder = builder.with_account(account);
            }
            Ok(Arc::new(builder.build()?))
        }
    }
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

fn run_crab_json(
    workspace: &Workspace,
    harness: &Harness,
    cache: &Path,
    cwd: &Path,
    args: &[String],
) -> TestResult<Value> {
    let mut command = workspace.crab(harness, cache);
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

fn run_crab(
    workspace: &Workspace,
    harness: &Harness,
    cache: &Path,
    cwd: &Path,
    args: &[&str],
) -> TestResult {
    let mut command = workspace.crab(harness, cache);
    command.current_dir(cwd).args(args);
    run(command, &format!("crab {}", args.join(" ")))?;
    Ok(())
}

fn run_git(workspace: &Workspace, harness: &Harness, cwd: &Path, args: &[&str]) -> TestResult {
    let mut command = workspace.command("git", harness);
    command.current_dir(cwd).args(args);
    run(command, &format!("git {}", args.join(" ")))?;
    Ok(())
}

fn require_git(workspace: &Workspace, harness: &Harness) -> TestResult<bool> {
    let mut command = workspace.command("git", harness);
    command.arg("--version");
    let output = command.output()?;
    if output.status.success() {
        return Ok(true);
    }
    eprintln!(
        "skipping {} binary hydrate replica proof: git is not available",
        harness.provider.label()
    );
    Ok(false)
}

fn write_replication_config(
    repo: &Path,
    harness: &Harness,
    primary_url: &str,
    replica_url: &str,
) -> TestResult {
    let config = format!(
        r#"# Crab project configuration

[remote]
url = "{primary_url}"

[replication]
primary = "{primary_url}"

[[replication.replicas]]
name = "{REPLICA_NAME}"
provider = "{}"
url = "{replica_url}"
region = "{}"
backfill = false
read = false
rpo = "standard"
"#,
        harness.provider.config_provider(),
        harness.region
    );
    std::fs::write(repo.join(".crab.toml"), config)?;
    Ok(())
}

fn primary_url(harness: &Harness, repo_prefix: &str) -> String {
    format!("crab://{}/{}", harness.primary.bucket, repo_prefix)
}

fn replica_url(harness: &Harness, repo_prefix: &str) -> String {
    match harness.provider {
        LiveProvider::S3 | LiveProvider::Gcs => format!(
            "{}://{}/{}",
            harness.provider.replica_scheme(),
            harness.replica.bucket,
            repo_prefix
        ),
        LiveProvider::Azure => format!(
            "{}://{}/{}/{}",
            harness.provider.replica_scheme(),
            harness
                .replica
                .account
                .as_deref()
                .expect("Azure replica account checked by harness"),
            harness.replica.bucket,
            repo_prefix
        ),
    }
}

async fn list_keys(store: &dyn ObjectStore) -> TestResult<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let mut stream = store.list(None);
    while let Some(object) = stream.try_next().await? {
        keys.insert(object.location.as_ref().to_owned());
    }
    Ok(keys)
}

fn new_uploaded_keys(before: BTreeSet<String>, after: BTreeSet<String>) -> Vec<String> {
    after.difference(&before).cloned().collect()
}

fn primary_xorb_keys(keys: &[String]) -> Vec<String> {
    keys.iter()
        .filter(|key| key.starts_with(".crab/xorbs/"))
        .cloned()
        .collect()
}

async fn copy_keys(
    source: &dyn ObjectStore,
    destination: &dyn ObjectStore,
    keys: &[String],
) -> TestResult {
    for key in keys {
        let path = ObjectPath::from(key.as_str());
        let bytes = source.get(&path).await?.bytes().await?;
        destination
            .put(&path, PutPayload::from(Bytes::from(bytes.to_vec())))
            .await?;
    }
    Ok(())
}

async fn delete_keys(store: &dyn ObjectStore, keys: &[String]) {
    for key in keys {
        let path = ObjectPath::from(key.as_str());
        let _ = store.delete(&path).await;
    }
}

async fn cleanup_created_keys(stores: &LiveStores, uploaded: &[String], primary_xorbs: &[String]) {
    delete_keys(stores.replica.as_ref(), uploaded).await;
    delete_keys(stores.primary.as_ref(), uploaded).await;
    delete_keys(stores.primary.as_ref(), primary_xorbs).await;
}

async fn run_binary_hydrate_proof(
    workspace: &Workspace,
    harness: &Harness,
    stores: &LiveStores,
    evidence: &Option<EvidenceRecorder>,
    before: BTreeSet<String>,
    uploaded_out: &mut Vec<String>,
    primary_xorbs_out: &mut Vec<String>,
) -> TestResult {
    if !require_git(workspace, harness)? {
        return Ok(());
    }

    let suffix = format!("{}-{}", std::process::id(), now_ms());
    let repo_prefix = format!("replica-binary-hydrate/{suffix}");
    let primary_url = primary_url(harness, &repo_prefix);
    let replica_url = replica_url(harness, &repo_prefix);
    let repo = workspace.root.join("source");
    std::fs::create_dir_all(&repo)?;

    let init_args = ["init".to_owned(), primary_url.clone(), "--json".to_owned()];
    let init = run_crab_json(workspace, harness, &workspace.push_cache, &repo, &init_args)?;
    record_evidence(evidence, "provider-hydrate-init", &repo, &init_args, init)?;
    run_git(
        workspace,
        harness,
        &repo,
        &["config", "user.email", "replica-hydrate@example.com"],
    )?;
    run_git(
        workspace,
        harness,
        &repo,
        &["config", "user.name", "Replica Hydrate"],
    )?;
    run_git(
        workspace,
        harness,
        &repo,
        &["config", "commit.gpgsign", "false"],
    )?;
    write_replication_config(&repo, harness, &primary_url, &replica_url)?;

    run_crab(
        workspace,
        harness,
        &workspace.push_cache,
        &repo,
        &["track", "*.bin"],
    )?;
    let file_name = "model.bin";
    let payload = format!(
        "{} provider-backed binary hydrate replica proof {suffix}\n",
        harness.provider.label()
    );
    std::fs::write(repo.join(file_name), &payload)?;
    let add_args = ["add".to_owned(), file_name.to_owned(), "--json".to_owned()];
    run_crab_json(workspace, harness, &workspace.push_cache, &repo, &add_args)?;
    run_git(
        workspace,
        harness,
        &repo,
        &["add", ".gitattributes", ".crab.toml"],
    )?;
    run_git(
        workspace,
        harness,
        &repo,
        &["commit", "-m", "add replica hydrate fixture"],
    )?;
    let push_args = [
        "push".to_owned(),
        "origin".to_owned(),
        "refs/heads/main:refs/heads/main".to_owned(),
        "--json".to_owned(),
    ];
    let push = run_crab_json(workspace, harness, &workspace.push_cache, &repo, &push_args)?;
    record_evidence(evidence, "provider-hydrate-push", &repo, &push_args, push)?;

    let after = list_keys(stores.primary.as_ref()).await?;
    let uploaded = new_uploaded_keys(before, after);
    assert!(
        !uploaded.is_empty(),
        "push should upload provider objects to primary"
    );
    *uploaded_out = uploaded.clone();
    copy_keys(stores.primary.as_ref(), stores.replica.as_ref(), &uploaded).await?;
    record_evidence(
        evidence,
        "provider-hydrate-copy",
        &repo,
        &["copy-primary-to-replica".to_owned()],
        serde_json::json!({
            "schema": "replica.live-hydrate",
            "data": {
                "provider": harness.provider.config_provider(),
                "copied_objects": uploaded.len()
            }
        }),
    )?;

    run_crab_json(
        workspace,
        harness,
        &workspace.push_cache,
        &repo,
        &[
            "dehydrate".to_owned(),
            "--all".to_owned(),
            "--json".to_owned(),
        ],
    )?;
    assert_ne!(
        std::fs::read_to_string(repo.join(file_name))?,
        payload,
        "dehydrate should replace the file with a pointer"
    );

    let wait_args = [
        "replica".to_owned(),
        "wait".to_owned(),
        REPLICA_NAME.to_owned(),
        "--enable-read".to_owned(),
        "--json".to_owned(),
    ];
    let wait = run_crab_json(
        workspace,
        harness,
        &workspace.hydrate_cache,
        &repo,
        &wait_args,
    )?;
    record_evidence(
        evidence,
        "provider-hydrate-read-enabled",
        &repo,
        &wait_args,
        wait,
    )?;

    let primary_xorbs = primary_xorb_keys(&uploaded);
    assert!(
        !primary_xorbs.is_empty(),
        "push should upload at least one primary xorb"
    );
    *primary_xorbs_out = primary_xorbs.clone();
    delete_keys(stores.primary.as_ref(), &primary_xorbs).await;
    record_evidence(
        evidence,
        "provider-hydrate-primary-xorbs-deleted",
        &repo,
        &["delete-primary-xorbs".to_owned()],
        serde_json::json!({
            "schema": "replica.live-hydrate",
            "data": {
                "provider": harness.provider.config_provider(),
                "deleted_xorbs": primary_xorbs.len()
            }
        }),
    )?;

    let mut hydrate = workspace.crab(harness, &workspace.hydrate_cache);
    hydrate
        .current_dir(&repo)
        .env(
            "CRAB_REPLICA_READ_POLICY",
            format!("replica:{REPLICA_NAME}"),
        )
        .args(["hydrate", "--all", "--json"]);
    let hydrate_output = run(hydrate, "crab hydrate --all --json")?;
    let hydrate_value: Value = serde_json::from_slice(&hydrate_output.stdout)?;
    if !hydrate_value["error"].is_null() {
        return Err(std::io::Error::other(format!(
            "crab hydrate --all --json returned error envelope: {hydrate_value}"
        ))
        .into());
    }
    record_evidence(
        evidence,
        "provider-hydrate-selected-replica",
        &repo,
        &[
            "hydrate".to_owned(),
            "--all".to_owned(),
            "--json".to_owned(),
        ],
        hydrate_value,
    )?;

    assert_eq!(
        std::fs::read_to_string(repo.join(file_name))?,
        payload,
        "hydrate should reconstruct bytes from the selected replica after primary data objects are removed"
    );

    Ok(())
}

async fn run_provider_test(provider: LiveProvider) -> TestResult {
    let Some(harness) = harness(provider) else {
        return Ok(());
    };
    let workspace = Workspace::new()?;
    let stores = LiveStores {
        primary: build_store(provider, &harness.primary)?,
        replica: build_store(provider, &harness.replica)?,
    };
    let evidence = EvidenceRecorder::from_env(&harness)?;
    let before = list_keys(stores.primary.as_ref()).await?;
    let mut uploaded = Vec::new();
    let mut primary_xorbs = Vec::new();

    let result = run_binary_hydrate_proof(
        &workspace,
        &harness,
        &stores,
        &evidence,
        before,
        &mut uploaded,
        &mut primary_xorbs,
    )
    .await;
    cleanup_created_keys(&stores, &uploaded, &primary_xorbs).await;
    result
}

#[ignore = "requires disposable S3 buckets or S3-compatible endpoint, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn binary_hydrate_uses_selected_s3_replica_after_primary_data_loss() -> TestResult {
    run_provider_test(LiveProvider::S3).await
}

#[ignore = "requires disposable GCS buckets, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn binary_hydrate_uses_selected_gcs_replica_after_primary_data_loss() -> TestResult {
    run_provider_test(LiveProvider::Gcs).await
}

#[ignore = "requires disposable Azure containers, ambient credentials, and explicit env flags"]
#[tokio::test(flavor = "multi_thread")]
async fn binary_hydrate_uses_selected_azure_replica_after_primary_data_loss() -> TestResult {
    run_provider_test(LiveProvider::Azure).await
}

#[test]
fn evidence_artifact_root_scopes_hydrate_artifacts() {
    assert_eq!(
        evidence_artifact_root(
            PathBuf::from("evidence"),
            "release 2026/06/16",
            "replica-binary-hydrate-live",
            "azure://storage-account/container",
        ),
        PathBuf::from("evidence")
            .join("release-2026-06-16")
            .join("replica-binary-hydrate-live")
            .join("azure-storage-account-container")
    );
}

#[test]
fn evidence_recorder_writes_ordered_hydrate_json_artifacts() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let recorder = EvidenceRecorder {
        root: tmp.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        provider: LiveProvider::S3,
        redacted: false,
        sensitive_values: Vec::new(),
    };
    let args = vec![
        "hydrate".to_owned(),
        "--all".to_owned(),
        "--json".to_owned(),
    ];

    recorder.record_json(
        "provider-hydrate-selected-replica",
        Path::new("source"),
        &args,
        &serde_json::json!({"schema": "hydrate", "data": {"hydrated": 1}}),
    )?;

    let mut files = std::fs::read_dir(tmp.path())?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    assert_eq!(files, vec!["001-provider-hydrate-selected-replica.json"]);

    let body = std::fs::read_to_string(tmp.path().join(&files[0]))?;
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["schema"], "replica.live-smoke.evidence");
    assert_eq!(value["harness"], "replica-binary-hydrate-live");
    assert_eq!(value["run_id"], "test-run");
    assert_eq!(value["sequence"], 1);
    assert_eq!(value["label"], "provider-hydrate-selected-replica");
    assert_eq!(value["provider"], "s3");
    assert_eq!(value["result"]["schema"], "hydrate");
    assert_eq!(value["result"]["data"]["hydrated"], 1);
    Ok(())
}

#[test]
fn evidence_recorder_redacts_hydrate_identifiers() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let recorder = EvidenceRecorder {
        root: tmp.path().to_path_buf(),
        sequence: AtomicU64::new(1),
        run_id: "test-run".to_owned(),
        provider: LiveProvider::Azure,
        redacted: true,
        sensitive_values: vec![
            "source-bucket".to_owned(),
            "replica-bucket".to_owned(),
            "storage-account".to_owned(),
        ],
    };

    recorder.record_json(
        "provider-hydrate-init",
        Path::new("source"),
        &[
            "init".to_owned(),
            "crab://source-bucket/disposable/repo".to_owned(),
            "--json".to_owned(),
        ],
        &serde_json::json!({
            "schema": "init",
            "data": {
                "primary": "crab://source-bucket/disposable/repo",
                "replica": "azure://storage-account/replica-bucket/disposable/repo"
            }
        }),
    )?;

    let body = std::fs::read_to_string(tmp.path().join("001-provider-hydrate-init.json"))?;
    let value: Value = serde_json::from_str(&body)?;
    assert_eq!(value["redacted"], true);
    assert!(body.contains("<redacted>"));
    for secret in ["source-bucket", "replica-bucket", "storage-account"] {
        assert!(!body.contains(secret), "{secret} leaked into evidence");
    }
    Ok(())
}
