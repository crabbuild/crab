//! Opt-in end-to-end test for `crab import` against a real
//! flat object-storage bucket.
//!
//! This test is gated behind `CRAB_E2E=1`. Without the env var it
//! logs a skip notice and returns successfully, keeping default
//! `cargo test` runs fast and hermetic.
//!
//! Expected environment when `CRAB_E2E=1`:
//!
//! - standard AWS credential env vars or provider-chain config
//! - `CRAB_E2E_FLAT_BUCKET`: bucket the test can write under
//! - `CRAB_E2E_FLAT_PREFIX`: optional scratch prefix
//! - `CRAB_E2E_S3_ENDPOINT`: optional S3-compatible endpoint
//!
//! For local S3-compatible stores, also set the usual object_store
//! endpoint overrides such as `AWS_ALLOW_HTTP=true` and
//! `AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crab_types::pointer::Pointer;
use futures_util::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use serde_json::Value;

const GATE_ENV: &str = "CRAB_E2E";
const BUCKET_ENV: &str = "CRAB_E2E_FLAT_BUCKET";
const PREFIX_ENV: &str = "CRAB_E2E_FLAT_PREFIX";
const ENDPOINT_ENV: &str = "CRAB_E2E_S3_ENDPOINT";
const DEFAULT_PREFIX: &str = "crab-e2e/import-flat";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct SeedObject {
    rel_path: &'static str,
    contents: Vec<u8>,
}

struct Workspace {
    root: PathBuf,
    path_env: OsString,
    push_cache: PathBuf,
    hydrate_cache: PathBuf,
    _tmp: tempfile::TempDir,
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

fn e2e_gate_enabled() -> bool {
    env::var(GATE_ENV)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_import_flat_bucket_roundtrip() -> TestResult {
    if !e2e_gate_enabled() {
        eprintln!(
            "skipping e2e_import_flat_bucket_roundtrip: {GATE_ENV} not set. \
             Export {GATE_ENV}=1 plus {BUCKET_ENV} and AWS credentials to run."
        );
        return Ok(());
    }

    let bucket = required_env(BUCKET_ENV)?;
    let base_prefix = env::var(PREFIX_ENV).unwrap_or_else(|_| DEFAULT_PREFIX.to_owned());
    let run_id = run_id("flat-import");
    let source_prefix = join_key(&base_prefix, &format!("source/{run_id}"));
    let target_prefix = join_key(&base_prefix, &format!("repos/{run_id}"));
    let export_prefix = join_key(&base_prefix, &format!("export/{run_id}"));
    let store = build_s3_store(&bucket)?;
    let workspace = Workspace::new()?;
    let seed_objects = seed_objects();

    cleanup_prefix(&store, &bucket, &source_prefix).await?;
    cleanup_prefix(&store, &bucket, &target_prefix).await?;
    cleanup_prefix(&store, &bucket, &export_prefix).await?;
    seed_source_objects(&store, &source_prefix, &seed_objects).await?;

    let source_url = format!("s3://{bucket}/{source_prefix}/");
    let target_url = format!("crab://{bucket}/{target_prefix}");
    let export_url = format!("s3://{bucket}/{export_prefix}/");
    let import_dir = workspace.root.join("import-worktree");
    let clone_dir = workspace.root.join("clone");

    let mut import = workspace.crab(&workspace.push_cache);
    import.args([
        "import",
        "--from",
        &source_url,
        "--to",
        &target_url,
        "--into",
    ]);
    import.arg(&import_dir);
    import.args(["--versions", "off", "--yes", "--json"]);

    let import_output = run(import, "crab import")?;
    assert_import_summary(&import_output, &source_url, &target_url, &seed_objects)?;

    let mut clone = workspace.git(&workspace.hydrate_cache);
    clone.args(["clone", &target_url]);
    clone.arg(&clone_dir);
    run(clone, "git clone")?;

    assert_git_clean(&workspace, &clone_dir, "after clone")?;
    assert_pre_hydrate_pointer(&clone_dir, &seed_objects)?;

    let mut hydrate = workspace.crab(&workspace.hydrate_cache);
    hydrate.current_dir(&clone_dir).args(["hydrate", "--all"]);
    run(hydrate, "crab hydrate --all")?;

    assert_hydrated_bytes(&clone_dir, &seed_objects)?;
    assert_git_clean(&workspace, &clone_dir, "after hydrate")?;

    let mut export = workspace.crab(&workspace.hydrate_cache);
    export.args([
        "export",
        &target_url,
        "--to",
        &export_url,
        "crab/large-files/",
        "--json",
    ]);
    let export_output = run(export, "crab export")?;
    assert_export_summary(&export_output, &target_url, &export_url, &seed_objects)?;
    assert_exported_bytes(&store, &export_prefix, &seed_objects).await?;

    cleanup_prefix(&store, &bucket, &export_prefix).await?;
    cleanup_prefix(&store, &bucket, &target_prefix).await?;
    cleanup_prefix(&store, &bucket, &source_prefix).await?;
    Ok(())
}

impl Workspace {
    fn new() -> TestResult<Self> {
        let tmp = tempfile::tempdir()?;
        let helper_dir = tmp.path().join("bin");
        fs::create_dir_all(&helper_dir)?;
        install_remote_helper(&helper_dir)?;

        let mut paths = vec![helper_dir];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        let path_env = env::join_paths(paths)?;
        let push_cache = tmp.path().join("push-cache");
        let hydrate_cache = tmp.path().join("hydrate-cache");
        fs::create_dir_all(&push_cache)?;
        fs::create_dir_all(&hydrate_cache)?;

        Ok(Self {
            root: tmp.path().to_path_buf(),
            path_env,
            push_cache,
            hydrate_cache,
            _tmp: tmp,
        })
    }

    fn crab(&self, cache: &Path) -> Command {
        let mut command = Command::new(bin());
        self.apply_env(&mut command, cache);
        command
    }

    fn git(&self, cache: &Path) -> Command {
        let mut command = Command::new("git");
        self.apply_env(&mut command, cache);
        command
    }

    fn apply_env(&self, command: &mut Command, cache: &Path) {
        command
            .env("PATH", &self.path_env)
            .env("CRAB_STORAGE_PROVIDER", "s3")
            .env("CRAB_CACHE_DIR", cache)
            .env("AWS_REGION", aws_region())
            .env("AWS_EC2_METADATA_DISABLED", "true");

        if let Some(endpoint) = s3_endpoint() {
            command.env("AWS_ENDPOINT_URL", &endpoint);
            if endpoint.starts_with("http://") {
                command.env("AWS_ALLOW_HTTP", "true");
            }
            if env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST").is_none() {
                command.env("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", "false");
            }
        }
    }
}

#[cfg(unix)]
fn install_remote_helper(helper_dir: &Path) -> TestResult {
    std::os::unix::fs::symlink(bin(), helper_dir.join("git-remote-crab"))?;
    Ok(())
}

#[cfg(not(unix))]
fn install_remote_helper(helper_dir: &Path) -> TestResult {
    fs::copy(bin(), helper_dir.join("git-remote-crab"))?;
    Ok(())
}

fn build_s3_store(bucket: &str) -> TestResult<Arc<dyn ObjectStore>> {
    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_region(aws_region());

    if let Some(endpoint) = s3_endpoint() {
        let allow_http =
            endpoint.starts_with("http://") || env_bool("AWS_ALLOW_HTTP") == Some(true);
        builder = builder
            .with_endpoint(endpoint)
            .with_virtual_hosted_style_request(
                env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST").unwrap_or(false),
            );
        if allow_http {
            builder = builder.with_allow_http(true);
        }
    } else if env_bool("AWS_ALLOW_HTTP") == Some(true) {
        builder = builder.with_allow_http(true);
    }

    Ok(Arc::new(builder.build()?))
}

async fn seed_source_objects(
    store: &Arc<dyn ObjectStore>,
    source_prefix: &str,
    seed_objects: &[SeedObject],
) -> TestResult {
    for object in seed_objects {
        let key = join_key(source_prefix, object.rel_path);
        store
            .put(
                &ObjectPath::from(key),
                Bytes::copy_from_slice(&object.contents).into(),
            )
            .await?;
    }
    Ok(())
}

async fn cleanup_prefix(store: &Arc<dyn ObjectStore>, bucket: &str, prefix: &str) -> TestResult {
    let prefix = normalize_prefix(prefix);
    if prefix.is_empty() {
        return Ok(());
    }

    let locations = store
        .list(Some(&ObjectPath::from(prefix.clone())))
        .map_ok(|meta| meta.location)
        .boxed();
    let results = store.delete_stream(locations).collect::<Vec<_>>().await;

    for result in results {
        result?;
    }
    cleanup_versioned_s3_prefix(bucket, &prefix).await?;
    Ok(())
}

#[cfg(feature = "tier-s3")]
async fn cleanup_versioned_s3_prefix(bucket: &str, prefix: &str) -> TestResult {
    use aws_sdk_s3::types::{BucketVersioningStatus, Delete, ObjectIdentifier};

    let client = build_s3_client().await;
    let versioning = match client.get_bucket_versioning().bucket(bucket).send().await {
        Ok(versioning) => versioning,
        Err(_) => return Ok(()),
    };
    if versioning.status() != Some(&BucketVersioningStatus::Enabled) {
        return Ok(());
    }

    loop {
        let page = client
            .list_object_versions()
            .bucket(bucket)
            .prefix(format!("{prefix}/"))
            .send()
            .await?;
        let mut objects = Vec::new();

        for version in page.versions() {
            if let (Some(key), Some(version_id)) = (version.key(), version.version_id()) {
                objects.push(
                    ObjectIdentifier::builder()
                        .key(key)
                        .version_id(version_id)
                        .build()?,
                );
            }
        }
        for marker in page.delete_markers() {
            if let (Some(key), Some(version_id)) = (marker.key(), marker.version_id()) {
                objects.push(
                    ObjectIdentifier::builder()
                        .key(key)
                        .version_id(version_id)
                        .build()?,
                );
            }
        }

        if objects.is_empty() {
            break;
        }

        for chunk in objects.chunks(1000) {
            let delete = Delete::builder()
                .set_objects(Some(chunk.to_vec()))
                .quiet(true)
                .build()?;
            client
                .delete_objects()
                .bucket(bucket)
                .delete(delete)
                .send()
                .await?;
        }
    }

    Ok(())
}

#[cfg(not(feature = "tier-s3"))]
async fn cleanup_versioned_s3_prefix(_bucket: &str, _prefix: &str) -> TestResult {
    Ok(())
}

#[cfg(feature = "tier-s3")]
async fn build_s3_client() -> aws_sdk_s3::Client {
    let endpoint = s3_endpoint();
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(aws_region()));
    if let Some(endpoint) = &endpoint {
        loader = loader.endpoint_url(endpoint.clone());
    }
    let config = loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&config)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());
    if let Some(endpoint) = endpoint {
        builder = builder.endpoint_url(endpoint);
        builder = builder.force_path_style(true);
    } else if let Some(virtual_hosted) = env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST") {
        builder = builder.force_path_style(!virtual_hosted);
    }
    aws_sdk_s3::Client::from_conf(builder.build())
}

fn assert_import_summary(
    output: &Output,
    source_url: &str,
    target_url: &str,
    seed_objects: &[SeedObject],
) -> TestResult {
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    let data = envelope
        .get("data")
        .ok_or("import JSON summary missing data field")?;
    let expected_bytes = seed_objects
        .iter()
        .map(|object| object.contents.len() as u64)
        .sum::<u64>();

    assert_eq!(envelope["schema"], "import.summary");
    assert_eq!(data["source_url"], source_url);
    assert_eq!(data["target_url"], target_url);
    assert_eq!(
        data["files_imported"].as_u64(),
        Some(seed_objects.len() as u64)
    );
    assert_eq!(data["bytes_source"].as_u64(), Some(expected_bytes));
    assert_eq!(data["same_bucket"].as_bool(), Some(true));
    assert!(
        data["commits_created"].as_u64().unwrap_or_default() >= 1,
        "import must create at least one commit"
    );
    Ok(())
}

fn assert_pre_hydrate_pointer(clone_dir: &Path, seed_objects: &[SeedObject]) -> TestResult {
    let attrs = fs::read_to_string(clone_dir.join(".gitattributes"))?;
    assert!(
        attrs.contains("*.bin filter=crab"),
        "large .bin import should synthesize a crab filter attribute"
    );

    let large = seed_objects
        .iter()
        .find(|object| object.rel_path.ends_with("large.bin"))
        .expect("seed objects include large.bin");
    let pointer_bytes = fs::read(clone_dir.join(large.rel_path))?;
    let pointer = Pointer::parse(&pointer_bytes)?;
    assert_eq!(pointer.size, large.contents.len() as u64);
    assert_eq!(pointer.file_hash, *blake3::hash(&large.contents).as_bytes());
    Ok(())
}

fn assert_hydrated_bytes(clone_dir: &Path, seed_objects: &[SeedObject]) -> TestResult {
    for object in seed_objects {
        let actual = fs::read(clone_dir.join(object.rel_path))?;
        assert_eq!(
            actual, object.contents,
            "hydrated bytes differed for {}",
            object.rel_path
        );
    }
    Ok(())
}

fn assert_export_summary(
    output: &Output,
    repo_url: &str,
    export_url: &str,
    seed_objects: &[SeedObject],
) -> TestResult {
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    let data = envelope
        .get("data")
        .ok_or("export JSON summary missing data field")?;
    let expected_bytes = seed_objects
        .iter()
        .map(|object| object.contents.len() as u64)
        .sum::<u64>();

    assert_eq!(envelope["schema"], "export.summary");
    assert_eq!(data["repo"], repo_url);
    assert_eq!(data["target_url"], export_url);
    assert_eq!(
        data["files_planned"].as_u64(),
        Some(seed_objects.len() as u64)
    );
    assert_eq!(
        data["files_exported"].as_u64(),
        Some(seed_objects.len() as u64)
    );
    assert_eq!(data["files_conflicted"].as_u64(), Some(0));
    assert_eq!(data["bytes_planned"].as_u64(), Some(expected_bytes));
    assert_eq!(data["bytes_exported"].as_u64(), Some(expected_bytes));
    assert_eq!(data["dry_run"].as_bool(), Some(false));
    Ok(())
}

async fn assert_exported_bytes(
    store: &Arc<dyn ObjectStore>,
    export_prefix: &str,
    seed_objects: &[SeedObject],
) -> TestResult {
    for object in seed_objects {
        let key = join_key(export_prefix, object.rel_path);
        let actual = store.get(&ObjectPath::from(key)).await?.bytes().await?;
        assert_eq!(
            actual.as_ref(),
            object.contents.as_slice(),
            "exported bytes differed for {}",
            object.rel_path
        );
    }
    Ok(())
}

fn assert_git_clean(workspace: &Workspace, repo: &Path, label: &str) -> TestResult {
    let mut status = workspace.git(&workspace.hydrate_cache);
    status.current_dir(repo).args(["status", "--porcelain=v1"]);
    let output = run(status, &format!("git status {label}"))?;
    assert!(
        output.stdout.is_empty(),
        "git status dirty {label}: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let mut diff = workspace.git(&workspace.hydrate_cache);
    diff.current_dir(repo).args(["diff", "--exit-code"]);
    run(diff, &format!("git diff {label}"))?;
    Ok(())
}

fn seed_objects() -> Vec<SeedObject> {
    vec![
        SeedObject {
            rel_path: "crab/large-files/large.bin",
            contents: deterministic_bytes(2 * 1024 * 1024 + 17, 0x41),
        },
        SeedObject {
            rel_path: "crab/large-files/nested/part-0001.txt",
            contents: b"part one\n".repeat(4096),
        },
        SeedObject {
            rel_path: "crab/large-files/nested/part-0002.txt",
            contents: b"part two\n".repeat(2048),
        },
        SeedObject {
            rel_path: "crab/large-files/manifest.json",
            contents: br#"{"name":"import-demo","kind":"flat-e2e"}"#.to_vec(),
        },
    ]
}

fn deterministic_bytes(len: usize, seed: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    for index in 0..len {
        let byte = seed
            .wrapping_add((index as u8).wrapping_mul(31))
            .wrapping_add((index / 8191) as u8);
        data.push(byte);
    }
    data
}

fn required_env(key: &str) -> TestResult<String> {
    let value = env::var(key)?;
    if value.trim().is_empty() {
        Err(format!("{key} must not be empty").into())
    } else {
        Ok(value)
    }
}

fn aws_region() -> String {
    env::var("AWS_REGION")
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_owned())
}

fn s3_endpoint() -> Option<String> {
    env_value(ENDPOINT_ENV)
        .or_else(|| env_value("AWS_ENDPOINT_URL"))
        .or_else(|| env_value("AWS_ENDPOINT"))
        .or_else(|| env_value("ENDPOINT_URL"))
        .or_else(|| env_value("ENDPOINT"))
}

fn env_value(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_bool(key: &str) -> Option<bool> {
    let value = env_value(key)?.to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn normalize_prefix(prefix: &str) -> String {
    prefix.trim_matches('/').to_owned()
}

fn join_key(prefix: &str, suffix: &str) -> String {
    let prefix = normalize_prefix(prefix);
    let suffix = suffix.trim_matches('/');
    if prefix.is_empty() {
        suffix.to_owned()
    } else if suffix.is_empty() {
        prefix
    } else {
        format!("{prefix}/{suffix}")
    }
}

fn run_id(label: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX_EPOCH")
        .as_millis();
    format!("{label}-{millis}-{}", std::process::id())
}

fn run(mut command: Command, label: &str) -> TestResult<Output> {
    let output = command.output()?;
    if !output.status.success() {
        panic!(
            "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}
