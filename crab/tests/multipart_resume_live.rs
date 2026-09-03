//! Opt-in interrupted-upload qualification against real S3 and GCS stores.
//!
//! Run an ignored test with its provider flag and scratch bucket set:
//!
//! - `CRAB_MULTIPART_LIVE_S3=1`, `CRAB_MULTIPART_LIVE_S3_BUCKET`, and AWS credentials
//! - `CRAB_MULTIPART_LIVE_GCS=1`, `CRAB_MULTIPART_LIVE_GCS_BUCKET`, and GCS credentials
//!
//! The buckets must be disposable test targets. Each run uses and deletes one
//! unique object beneath `.crab/e2e/multipart-resume/`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crab::storage::store::{MultipartJournal, Store};
use crab_storage::multipart::ResumableUploadOutcome;
use crab_types::storage::StorageProviderKind;
use futures_util::FutureExt as _;
use object_store::path::Path as ObjectPath;
use tokio_util::sync::CancellationToken;

const PART_SIZE: usize = 8 * 1024 * 1024;
const PAYLOAD_SIZE: usize = PART_SIZE * 5 + 17;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[ignore = "requires a writable live S3 bucket and ambient credentials"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_s3_interrupted_multipart_resumes_and_verifies() -> TestResult {
    run_provider(
        "CRAB_MULTIPART_LIVE_S3",
        "CRAB_MULTIPART_LIVE_S3_BUCKET",
        StorageProviderKind::S3,
    )
    .await
}

#[ignore = "requires a writable live GCS bucket and ambient credentials"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_gcs_interrupted_multipart_resumes_and_verifies() -> TestResult {
    run_provider(
        "CRAB_MULTIPART_LIVE_GCS",
        "CRAB_MULTIPART_LIVE_GCS_BUCKET",
        StorageProviderKind::Gcs,
    )
    .await
}

async fn run_provider(
    gate_env: &str,
    bucket_env: &str,
    provider: StorageProviderKind,
) -> TestResult {
    if !enabled(gate_env) {
        eprintln!("skipping live multipart qualification: {gate_env} is not enabled");
        return Ok(());
    }
    let bucket = env::var(bucket_env)
        .map_err(|_| std::io::Error::other(format!("{bucket_env} must name a scratch bucket")))?;
    let storage = crab_storage::build_static_env_store(&bucket, provider)?;
    let store = Store::from_storage(storage);
    let temp = tempfile::tempdir()?;
    let journal_path = temp.path().join("multipart.sqlite3");
    let file_path = temp.path().join("payload.bin");
    let payload = vec![0x5a; PAYLOAD_SIZE];
    tokio::fs::write(&file_path, &payload).await?;
    let hash = *blake3::hash(&payload).as_bytes();
    drop(payload);
    let path = ObjectPath::from(format!(
        ".crab/e2e/multipart-resume/{}-{}.bin",
        provider_name(provider),
        run_id()
    ));

    let result = std::panic::AssertUnwindSafe(qualify_interruption(
        provider,
        &bucket,
        &journal_path,
        &path,
        &file_path,
        hash,
    ))
    .catch_unwind()
    .await;
    let journal = MultipartJournal::new(crab_staging::MultipartRegistry::open(&journal_path)?);
    let cleanup = cleanup(&store, &journal, &path).await;
    match result {
        Ok(result) => result?,
        Err(panic) => std::panic::resume_unwind(panic),
    }
    cleanup
}

async fn qualify_interruption(
    provider: StorageProviderKind,
    bucket: &str,
    journal_path: &std::path::Path,
    path: &ObjectPath,
    file_path: &std::path::Path,
    hash: [u8; 32],
) -> TestResult {
    let store = Store::from_storage(crab_storage::build_static_env_store(bucket, provider)?);
    let journal = MultipartJournal::new(crab_staging::MultipartRegistry::open(journal_path)?);
    let cancel = CancellationToken::new();
    let cancel_after_part = cancel.clone();
    let uploaded = AtomicU64::new(0);
    let on_part = |bytes| {
        uploaded.fetch_add(bytes, Ordering::Relaxed);
        cancel_after_part.cancel();
    };
    let interrupted = store
        .put_multipart_file_resumable(
            path,
            file_path,
            PAYLOAD_SIZE as u64,
            hash,
            &hash,
            PART_SIZE,
            &cancel,
            Some(&on_part),
            Some(&journal),
        )
        .await;
    assert!(
        matches!(interrupted, Err(crab::core::error::CrabError::Cancelled)),
        "first upload must stop at the injected interruption: {interrupted:?}"
    );
    assert!(uploaded.load(Ordering::Relaxed) > 0);
    drop(journal);
    drop(store);

    let store = Store::from_storage(crab_storage::build_static_env_store(bucket, provider)?);
    let journal = MultipartJournal::new(crab_staging::MultipartRegistry::open(journal_path)?);
    let outcome = store
        .put_multipart_file_resumable(
            path,
            file_path,
            PAYLOAD_SIZE as u64,
            hash,
            &hash,
            PART_SIZE,
            &CancellationToken::new(),
            None,
            Some(&journal),
        )
        .await?;
    assert_eq!(outcome, ResumableUploadOutcome::Resumed);
    let (persisted, _) = store.get_with_etag(path).await?;
    assert_eq!(persisted.len(), PAYLOAD_SIZE);
    assert_eq!(blake3::hash(&persisted).as_bytes(), &hash);
    assert!(
        journal
            .find_abandoned(SystemTime::now() + Duration::from_secs(120), Duration::ZERO,)
            .await?
            .is_empty(),
        "verified completion must remove its journal row"
    );
    Ok(())
}

async fn cleanup(store: &Store, journal: &MultipartJournal, path: &ObjectPath) -> TestResult {
    let cleanup_now = SystemTime::now() + Duration::from_secs(120);
    let cleanup_unix = i64::try_from(cleanup_now.duration_since(UNIX_EPOCH)?.as_secs())?;
    for abandoned in journal.find_abandoned(cleanup_now, Duration::ZERO).await? {
        let Some(claim) = journal
            .claim_abandoned(
                &abandoned.entry_id,
                abandoned.revision,
                "live-test-cleanup",
                cleanup_unix,
                Duration::from_secs(60),
            )
            .await?
        else {
            continue;
        };
        if let Some(upload_id) = claim.upload_id {
            store.abort_explicit_multipart(path, &upload_id).await?;
        }
        assert!(journal.abandon_repair(&claim.lease, cleanup_unix).await?);
    }
    store.delete(path).await?;
    Ok(())
}

fn enabled(key: &str) -> bool {
    env::var(key)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

fn provider_name(provider: StorageProviderKind) -> &'static str {
    match provider {
        StorageProviderKind::S3 => "s3",
        StorageProviderKind::Gcs => "gcs",
        StorageProviderKind::Azure => "azure",
        StorageProviderKind::Local => "local",
    }
}

fn run_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{timestamp}", std::process::id())
}
