//! Retained real-provider qualification for Crab's storage contract.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test harness"
)]

use std::collections::BTreeMap;
use std::env;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crab_metadata::manifests::PackManifestEntry;
use crab_metadata::pack_origin::{record_verified_pack_origin, verify_pack_origin};
use crab_metadata::receipts::OriginReceipt;
use crab_storage::{
    RetryPolicy, StorageError, StorageProviderKind, Store, StoreLayout, build_static_env_store,
    retry,
};
use futures_util::stream::{self, StreamExt as _};
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SCHEMA: &str = "crab.provider-qualification";
const SCHEMA_VERSION: u32 = 1;
const MULTIPART_PART_BYTES: usize = 6 * 1024 * 1024;
const MULTIPART_BODY_BYTES: usize = MULTIPART_PART_BYTES * 2 + 79;
const MAX_PARALLEL_OBJECT_WRITES: usize = 64;

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    duration_ms: u128,
    details: Value,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RequestMetrics {
    logical_read_requests: u64,
    logical_read_bytes: u64,
    logical_write_requests: u64,
    logical_write_bytes: u64,
    listed_objects: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    schema: &'static str,
    schema_version: u32,
    status: &'static str,
    provider: &'static str,
    service: String,
    region: String,
    bucket: String,
    isolated_prefix: String,
    source_sha: String,
    workflow_run_id: String,
    workflow_run_attempt: String,
    object_store_version: &'static str,
    started_unix_ms: u128,
    finished_unix_ms: u128,
    commands: Vec<String>,
    request_metrics: RequestMetrics,
    checks: Vec<Check>,
}

#[derive(Default)]
struct Metrics {
    reads: AtomicU64,
    read_bytes: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
    listed_objects: AtomicU64,
}

struct Harness {
    store: Store,
    provider: StorageProviderKind,
    prefix: String,
    metrics: Arc<Metrics>,
}

impl Harness {
    fn path(&self, suffix: &str) -> ObjectPath {
        ObjectPath::from(format!("{}/{suffix}", self.prefix))
    }

    async fn create(&self, path: &ObjectPath, body: Bytes) -> Result<crab_storage::ETag, String> {
        self.metrics.writes.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .write_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        self.store
            .create_strict_with_etag(path, body)
            .await
            .map_err(|error| error.to_string())
    }

    fn pagination_object_count(&self) -> usize {
        match self.provider {
            StorageProviderKind::Azure => 5_005,
            StorageProviderKind::S3 | StorageProviderKind::Gcs => 1_005,
            StorageProviderKind::Local => 0,
        }
    }

    async fn cleanup(&self) -> Result<Value, String> {
        if !self.prefix.starts_with("crab-provider-qualification/")
            || self.prefix.split('/').count() != 2
        {
            return Err(format!(
                "refusing cleanup outside generated qualification prefix: {}",
                self.prefix
            ));
        }
        let prefix = ObjectPath::from(self.prefix.clone());
        let objects = self
            .store
            .list_prefix(&prefix)
            .await
            .map_err(|error| error.to_string())?;
        let count = objects.len();
        let store = self.store.clone();
        let results = stream::iter(objects.into_iter().map(|meta| {
            let store = store.clone();
            async move { store.delete(&meta.location).await }
        }))
        .buffer_unordered(MAX_PARALLEL_OBJECT_WRITES)
        .collect::<Vec<_>>()
        .await;
        if let Some(error) = results.into_iter().find_map(Result::err) {
            return Err(error.to_string());
        }
        let remaining = self
            .store
            .list_prefix_bounded(&prefix, 0)
            .await
            .map_err(|error| error.to_string())?;
        if remaining.is_none() {
            return Err("qualification prefix cleanup left objects behind".to_owned());
        }
        Ok(json!({"deleted_objects": count}))
    }
}

async fn run_check<F, Fut>(checks: &mut Vec<Check>, name: &'static str, operation: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let started = Instant::now();
    match operation().await {
        Ok(details) => checks.push(Check {
            name,
            ok: true,
            duration_ms: started.elapsed().as_millis(),
            details,
            error: None,
        }),
        Err(error) => checks.push(Check {
            name,
            ok: false,
            duration_ms: started.elapsed().as_millis(),
            details: Value::Null,
            error: Some(error),
        }),
    }
}

fn require_version_token(token: &crab_storage::ETag, context: &str) -> Result<(), String> {
    if token.e_tag.is_none() && token.version.is_none() {
        return Err(format!("{context} returned no ETag or object version"));
    }
    Ok(())
}

fn deterministic_body(size: usize, seed: u8) -> Bytes {
    let mut body = Vec::with_capacity(size);
    for index in 0..size {
        body.push(seed.wrapping_add((index % 251) as u8));
    }
    Bytes::from(body)
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} is required for provider qualification"))
}

fn parse_provider(value: &str) -> StorageProviderKind {
    match value {
        "s3" => StorageProviderKind::S3,
        "gcs" => StorageProviderKind::Gcs,
        "azure" => StorageProviderKind::Azure,
        other => panic!("unsupported qualification provider {other:?}"),
    }
}

fn provider_name(provider: StorageProviderKind) -> &'static str {
    match provider {
        StorageProviderKind::S3 => "s3",
        StorageProviderKind::Gcs => "gcs",
        StorageProviderKind::Azure => "azure",
        StorageProviderKind::Local => "local",
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires an isolated real object-store bucket/container"]
async fn provider_contracts() {
    let started_unix_ms = unix_millis();
    let provider = parse_provider(&required_env("CRAB_PROVIDER_QUALIFICATION_PROVIDER"));
    let bucket = required_env("CRAB_PROVIDER_QUALIFICATION_BUCKET");
    let run_id = required_env("CRAB_PROVIDER_QUALIFICATION_RUN_ID");
    assert!(
        run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')),
        "qualification run id contains unsafe path characters"
    );
    let prefix = format!("crab-provider-qualification/{run_id}");
    let report_path = PathBuf::from(required_env("CRAB_PROVIDER_QUALIFICATION_REPORT"));
    let service = required_env("CRAB_PROVIDER_QUALIFICATION_SERVICE");
    let region = required_env("CRAB_PROVIDER_QUALIFICATION_REGION");
    let source_sha = required_env("CRAB_PROVIDER_QUALIFICATION_SOURCE_SHA");
    let workflow_run_id = required_env("CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ID");
    let workflow_run_attempt = required_env("CRAB_PROVIDER_QUALIFICATION_WORKFLOW_RUN_ATTEMPT");

    let metrics = Arc::new(Metrics::default());
    let read_metrics = Arc::clone(&metrics);
    let read_request_metrics = Arc::clone(&metrics);
    let store = build_static_env_store(&bucket, provider)
        .unwrap_or_else(|error| panic!("build {provider:?} store: {error}"))
        .with_read_byte_observer(Arc::new(move |bytes| {
            read_metrics.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        }))
        .with_read_request_observer(Arc::new(move |_| {
            read_request_metrics.reads.fetch_add(1, Ordering::Relaxed);
        }));
    let harness = Harness {
        store,
        provider,
        prefix,
        metrics,
    };
    let mut checks = Vec::new();

    run_check(&mut checks, "isolated_prefix_preflight", || async {
        let prefix = ObjectPath::from(harness.prefix.clone());
        match harness
            .store
            .list_prefix_bounded(&prefix, 0)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(objects) if objects.is_empty() => Ok(json!({"empty": true})),
            _ => Err(format!(
                "qualification prefix {} is not empty; choose a new run id",
                harness.prefix
            )),
        }
    })
    .await;

    run_check(&mut checks, "create_only", || async {
        let path = harness.path("conditional/create-only");
        let token = harness
            .create(&path, Bytes::from_static(b"first-writer"))
            .await?;
        require_version_token(&token, "create-only write")?;
        harness.metrics.writes.fetch_add(1, Ordering::Relaxed);
        let conflict = harness
            .store
            .create_strict(&path, Bytes::from_static(b"second-writer"))
            .await
            .expect_err("second create must conflict");
        if !matches!(conflict, StorageError::StateConflict { .. }) {
            return Err(format!(
                "second create mapped to {conflict}, expected conflict"
            ));
        }
        let (body, read_token) = harness
            .store
            .get_with_etag(&path)
            .await
            .map_err(|error| error.to_string())?;
        if body != Bytes::from_static(b"first-writer") {
            return Err("create-only conflict changed the stored body".to_owned());
        }
        require_version_token(&read_token, "create-only read")?;
        Ok(json!({
            "etag": read_token.e_tag.is_some(),
            "version": read_token.version.is_some(),
        }))
    })
    .await;

    run_check(&mut checks, "match_token_and_identity", || async {
        let path = harness.path("conditional/match-token");
        let created = harness
            .create(&path, Bytes::from_static(b"generation-0"))
            .await?;
        require_version_token(&created, "initial CAS write")?;
        harness.metrics.writes.fetch_add(1, Ordering::Relaxed);
        let updated = harness
            .store
            .update(&path, Bytes::from_static(b"generation-1"), created.clone())
            .await
            .map_err(|error| error.to_string())?;
        require_version_token(&updated, "CAS update")?;
        if updated == created {
            return Err("CAS token did not change after body replacement".to_owned());
        }
        harness.metrics.writes.fetch_add(1, Ordering::Relaxed);
        let stale = harness
            .store
            .update(&path, Bytes::from_static(b"stale-writer"), created)
            .await
            .expect_err("stale CAS token must conflict");
        if !matches!(stale, StorageError::StateConflict { .. }) {
            return Err(format!("stale update mapped to {stale}, expected conflict"));
        }
        let (body, current) = harness
            .store
            .get_with_etag(&path)
            .await
            .map_err(|error| error.to_string())?;
        if body != Bytes::from_static(b"generation-1") || current != updated {
            return Err("stale CAS changed the current object identity".to_owned());
        }
        Ok(json!({
            "etag": current.e_tag.is_some(),
            "version": current.version.is_some(),
        }))
    })
    .await;

    run_check(&mut checks, "multipart_complete", || async {
        let path = harness.path("multipart/completed");
        let body = deterministic_body(MULTIPART_BODY_BYTES, 11);
        let completed = Arc::new(AtomicU64::new(0));
        let completed_observer = Arc::clone(&completed);
        let progress = move |bytes| {
            completed_observer.fetch_add(bytes, Ordering::Relaxed);
        };
        harness.metrics.writes.fetch_add(1, Ordering::Relaxed);
        harness
            .metrics
            .write_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        harness
            .store
            .put_multipart_retry(
                &path,
                body.clone(),
                MULTIPART_PART_BYTES,
                &CancellationToken::new(),
                Some(&progress),
            )
            .await
            .map_err(|error| error.to_string())?;
        if completed.load(Ordering::Relaxed) != body.len() as u64 {
            return Err("multipart progress did not cover the complete payload".to_owned());
        }
        harness
            .store
            .verify_size_and_hash(&path, body.len() as u64, blake3::hash(&body).as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "bytes": body.len(),
            "part_bytes": MULTIPART_PART_BYTES,
            "parts": 3,
        }))
    })
    .await;

    run_check(&mut checks, "multipart_abort", || async {
        let path = harness.path("multipart/aborted");
        let mut upload = harness
            .store
            .create_multipart_upload(&path)
            .await
            .map_err(|error| error.to_string())?;
        upload
            .put_part(deterministic_body(MULTIPART_PART_BYTES, 17).into())
            .await
            .map_err(|error| error.to_string())?;
        upload.abort().await.map_err(|error| error.to_string())?;
        match harness.store.head(&path).await {
            Err(StorageError::NotFound { .. }) => Ok(json!({"aborted": true})),
            Ok(_) => Err("aborted multipart upload published an object".to_owned()),
            Err(error) => Err(format!("aborted multipart HEAD mapped to {error}")),
        }
    })
    .await;

    run_check(&mut checks, "file_backed_staged_multipart", || async {
        let canonical = harness.path("multipart/file-backed-canonical");
        let staging_prefix = format!("{}/staged-writes", harness.prefix);
        let staging = harness
            .store
            .clone()
            .with_staging_writes(staging_prefix.clone());
        let temp = TempDir::new().map_err(|error| error.to_string())?;
        let source = temp.path().join("payload.xorb");
        let body = deterministic_body(MULTIPART_BODY_BYTES, 29);
        tokio::fs::write(&source, &body)
            .await
            .map_err(|error| error.to_string())?;
        let expected_hash = *blake3::hash(&body).as_bytes();
        harness.metrics.writes.fetch_add(1, Ordering::Relaxed);
        harness
            .metrics
            .write_bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        staging
            .put_multipart_file_retry(
                &canonical,
                &source,
                body.len() as u64,
                expected_hash,
                MULTIPART_PART_BYTES,
                &CancellationToken::new(),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        if !matches!(
            harness.store.head(&canonical).await,
            Err(StorageError::NotFound { .. })
        ) {
            return Err("staged multipart wrote the canonical key before flush".to_owned());
        }
        let mappings = staging.staged_writes();
        if mappings.len() != 1
            || mappings[0].canonical_key != canonical.as_ref()
            || !mappings[0].staged_key.starts_with(&staging_prefix)
        {
            return Err(format!("unexpected staged multipart mapping: {mappings:?}"));
        }
        let flushed = staging
            .flush_staged_writes(1)
            .await
            .map_err(|error| error.to_string())?;
        if flushed != mappings {
            return Err("staged multipart flush returned a different publication set".to_owned());
        }
        let staged_path = ObjectPath::from(mappings[0].staged_key.clone());
        harness
            .store
            .verify_size_and_hash(&staged_path, body.len() as u64, &expected_hash)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "bytes": body.len(),
            "staged_key": mappings[0].staged_key,
            "materialized_body": false,
            "published_canonical": false,
        }))
    })
    .await;

    run_check(&mut checks, "exact_range_read", || async {
        let path = harness.path("reads/range");
        let body = deterministic_body(8 * 1024 + 31, 43);
        harness.create(&path, body.clone()).await?;
        let range = 317_u64..4_901_u64;
        let expected = body.slice(range.start as usize..range.end as usize);
        let got = harness
            .store
            .range_get(&path, range.clone())
            .await
            .map_err(|error| error.to_string())?;
        if got != expected {
            return Err("bounded range response differs from requested bytes".to_owned());
        }
        Ok(json!({"start": range.start, "end": range.end, "bytes": got.len()}))
    })
    .await;

    run_check(&mut checks, "provider_pagination", || async {
        let prefix = harness.path("pagination");
        let count = harness.pagination_object_count();
        let writes = stream::iter((0..count).map(|index| {
            let store = harness.store.clone();
            let path = ObjectPath::from(format!("{prefix}/{index:08}"));
            let metrics = Arc::clone(&harness.metrics);
            async move {
                metrics.writes.fetch_add(1, Ordering::Relaxed);
                metrics.write_bytes.fetch_add(1, Ordering::Relaxed);
                store.create_strict(&path, Bytes::from_static(b"p")).await
            }
        }))
        .buffer_unordered(MAX_PARALLEL_OBJECT_WRITES)
        .collect::<Vec<_>>()
        .await;
        if let Some(error) = writes.into_iter().find_map(Result::err) {
            return Err(error.to_string());
        }
        let objects = harness
            .store
            .list_prefix(&prefix)
            .await
            .map_err(|error| error.to_string())?;
        harness
            .metrics
            .listed_objects
            .fetch_add(objects.len() as u64, Ordering::Relaxed);
        if objects.len() != count {
            return Err(format!(
                "provider list returned {} of {count} objects",
                objects.len()
            ));
        }
        let unique = objects
            .iter()
            .map(|meta| meta.location.as_ref())
            .collect::<std::collections::BTreeSet<_>>();
        if unique.len() != count {
            return Err("provider pagination returned duplicate object keys".to_owned());
        }
        Ok(json!({
            "objects": count,
            "crosses_default_provider_page": true,
        }))
    })
    .await;

    run_check(&mut checks, "retry_and_error_mapping", || async {
        let missing = harness.path("errors/missing");
        if !matches!(
            harness.store.get_with_etag(&missing).await,
            Err(StorageError::NotFound { .. })
        ) {
            return Err("provider missing-object response did not map to NotFound".to_owned());
        }
        let attempts = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&attempts);
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        };
        let value = retry(&policy, move || {
            let observed = Arc::clone(&observed);
            async move {
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(StorageError::NetworkTransient {
                        source: object_store::Error::Generic {
                            store: "provider-qualification",
                            source: Box::<dyn std::error::Error + Send + Sync>::from(
                                "injected connection reset",
                            ),
                        },
                    })
                } else {
                    Ok(1_u8)
                }
            }
        })
        .await
        .map_err(|error| error.to_string())?;
        if value != 1 || attempts.load(Ordering::SeqCst) != 2 {
            return Err("transient retry contract did not retry exactly once".to_owned());
        }
        Ok(json!({
            "provider_not_found": "classified",
            "provider_conflict": "classified by create/CAS checks",
            "injected_transient_attempts": 2,
        }))
    })
    .await;

    run_check(&mut checks, "multipart_cancellation", || async {
        let path = harness.path("multipart/cancelled");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = harness
            .store
            .put_multipart_retry(
                &path,
                deterministic_body(MULTIPART_BODY_BYTES, 53),
                MULTIPART_PART_BYTES,
                &cancel,
                None,
            )
            .await
            .expect_err("pre-cancelled multipart upload must fail");
        if !matches!(error, StorageError::Cancelled) {
            return Err(format!("cancelled multipart mapped to {error}"));
        }
        if !matches!(
            harness.store.head(&path).await,
            Err(StorageError::NotFound { .. })
        ) {
            return Err("cancelled multipart upload published an object".to_owned());
        }
        Ok(json!({"cancelled": true, "published": false}))
    })
    .await;

    run_check(&mut checks, "origin_receipt", || async {
        let repo_prefix = format!("{}/origin-receipt", harness.prefix);
        let router = StoreLayout::new(harness.store.clone(), repo_prefix.clone());
        let body = deterministic_body(256 * 1024 + 17, 71);
        let hash = blake3::hash(&body).to_hex().to_string();
        let pack = PackManifestEntry {
            pack_id: hash.clone(),
            size: body.len() as u64,
            content_hash: hash.clone(),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        };
        let pack_path = router.pack_path(&hash);
        harness.create(&pack_path, body).await?;
        record_verified_pack_origin(&harness.store, &repo_prefix, &pack)
            .await
            .map_err(|error| error.to_string())?;
        let first = verify_pack_origin(&harness.store, &repo_prefix, &pack)
            .await
            .map_err(|error| error.to_string())?;
        let second = verify_pack_origin(&harness.store, &repo_prefix, &pack)
            .await
            .map_err(|error| error.to_string())?;
        if first || second {
            return Err("version-bound receipt did not avoid repeated pack hashing".to_owned());
        }
        let receipt_path = router.pack_origin_receipt_path(&hash);
        let (receipt_bytes, _) = harness
            .store
            .get_with_etag(&receipt_path)
            .await
            .map_err(|error| error.to_string())?;
        let receipt: OriginReceipt = serde_json::from_slice(&receipt_bytes)
            .map_err(|error| format!("parse origin receipt: {error}"))?;
        let meta = harness
            .store
            .head(&pack_path)
            .await
            .map_err(|error| error.to_string())?;
        if receipt.schema_version != 1
            || receipt.etag != meta.e_tag
            || receipt.object_version != meta.version
        {
            return Err("origin receipt is not bound to current provider identity".to_owned());
        }
        Ok(json!({
            "schema_version": receipt.schema_version,
            "etag": receipt.etag.is_some(),
            "version": receipt.object_version.is_some(),
            "second_hash_avoided": true,
        }))
    })
    .await;

    run_check(&mut checks, "isolated_prefix_cleanup", || harness.cleanup()).await;

    let ok = checks.iter().all(|check| check.ok);
    let mut commands = vec![
        "cargo test -p crab --test provider_qualification --locked -- --ignored --exact provider_contracts --nocapture".to_owned(),
    ];
    if let Ok(command) = env::var("CRAB_PROVIDER_QUALIFICATION_COMMAND") {
        commands.push(command);
    }
    let request_metrics = RequestMetrics {
        logical_read_requests: harness.metrics.reads.load(Ordering::Relaxed),
        logical_read_bytes: harness.metrics.read_bytes.load(Ordering::Relaxed),
        logical_write_requests: harness.metrics.writes.load(Ordering::Relaxed),
        logical_write_bytes: harness.metrics.write_bytes.load(Ordering::Relaxed),
        listed_objects: harness.metrics.listed_objects.load(Ordering::Relaxed),
    };
    let report = Report {
        schema: SCHEMA,
        schema_version: SCHEMA_VERSION,
        status: if ok { "ok" } else { "failed" },
        provider: provider_name(provider),
        service,
        region,
        bucket,
        isolated_prefix: harness.prefix.clone(),
        source_sha,
        workflow_run_id,
        workflow_run_attempt,
        object_store_version: "0.14.1",
        started_unix_ms,
        finished_unix_ms: unix_millis(),
        commands,
        request_metrics,
        checks,
    };
    let parent = report_path.parent().expect("report path has parent");
    std::fs::create_dir_all(parent).expect("create report parent");
    std::fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("serialize report"),
    )
    .expect("write qualification report");

    if !ok {
        let failures = report
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| {
                (
                    check.name,
                    check.error.as_deref().unwrap_or("unknown error"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        panic!("provider qualification failed: {failures:?}");
    }
}
