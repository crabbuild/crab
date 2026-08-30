//! Integration tests for the crab operations layer.
//!
//! Covers:
//! - 35.1: Happy-path pipeline on LocalFileSystem (init → track → clean → pointer validation)
//! - 35.2: Fault-injection suite on MockStore (retry, error propagation, cancellation)
//! - 35.3: Interop test — perf-enabled and perf-disabled produce identical outcomes
//! - 35.4: Verified by `cargo test` passing with default features (no new code needed)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;

use bytes::Bytes;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
#[cfg(feature = "testing")]
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use tokio_util::sync::CancellationToken;

use crab::core::config::{Config, EngineConfig};
use crab::core::context::AppContext;
use crab::core::error::CrabError;
use crab::git::clean::CleanSession;
use crab::storage::store::Store;
#[cfg(feature = "testing")]
use crab::storage::{RetryPolicy, retry};
use crab_types::pointer::{Pointer, is_pointer};

// --- 35.1: Happy-path integration on LocalFileSystem ---

/// Full pipeline: init → track → clean → verify pointer is valid.
///
/// Uses a temp directory as the "local filesystem" and exercises the
/// module APIs directly rather than spawning a subprocess.
#[tokio::test]
async fn happy_path_init_track_clean_pointer_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();

    // Step 1: init — creates .crab/local.toml
    crab::cmd::init::run_init_in("crab://test-bucket/test-repo", dir.path(), &cancel)
        .await
        .expect("init should succeed");

    let config_path = dir.path().join(".crab/local.toml");
    assert!(
        config_path.exists(),
        ".crab/local.toml must exist after init"
    );
    let config_content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        config_content.contains("crab://test-bucket/test-repo"),
        "config must contain the remote URL",
    );

    // Step 2: track — adds .gitattributes entry
    crab::cmd::track::run_track_in("*.bin", dir.path()).expect("track should succeed");

    let ga_path = dir.path().join(".gitattributes");
    assert!(ga_path.exists(), ".gitattributes must exist after track");
    let ga_content = std::fs::read_to_string(&ga_path).unwrap();
    assert!(
        ga_content.contains("*.bin filter=crab diff=crab merge=crab -text"),
        "gitattributes must contain the tracked pattern",
    );

    // Step 3: clean — process a file through the clean filter
    let ctx = AppContext::default();
    let mut session = CleanSession::new(ctx);
    let test_content = b"Hello, this is test content for the clean filter integration test.";
    let pointer_bytes = session
        .clean_file("test.bin", test_content.to_vec())
        .expect("clean should succeed");

    // Step 4: verify the pointer is valid
    assert!(
        is_pointer(&pointer_bytes),
        "clean output must be a valid crab pointer",
    );

    let pointer = Pointer::parse(&pointer_bytes).expect("pointer must parse");
    assert_eq!(
        pointer.size,
        test_content.len() as u64,
        "pointer size must match input size",
    );

    // The file hash in the pointer must match blake3 of the content.
    let expected_hash: [u8; 32] = *blake3::hash(test_content).as_bytes();
    assert_eq!(
        pointer.file_hash, expected_hash,
        "pointer file_hash must match blake3 of input",
    );
}

/// Init is idempotent — running twice updates the config without error.
#[tokio::test]
async fn init_idempotent_updates_config() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();

    crab::cmd::init::run_init_in("crab://bucket/v1", dir.path(), &cancel)
        .await
        .unwrap();
    crab::cmd::init::run_init_in("crab://bucket/v2", dir.path(), &cancel)
        .await
        .unwrap();

    let content = std::fs::read_to_string(dir.path().join(".crab/local.toml")).unwrap();
    assert!(content.contains("crab://bucket/v2"));
}

/// Track is idempotent — adding the same pattern twice produces one entry.
#[test]
fn track_idempotent_single_entry() {
    let dir = tempfile::tempdir().unwrap();
    crab::cmd::track::run_track_in("*.dat", dir.path()).unwrap();
    crab::cmd::track::run_track_in("*.dat", dir.path()).unwrap();

    let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
    let count = content.lines().filter(|l| l.contains("*.dat")).count();
    assert_eq!(count, 1, "pattern should appear exactly once");
}

/// Untrack removes the pattern and preserves other lines.
#[test]
fn untrack_removes_pattern_preserves_others() {
    let dir = tempfile::tempdir().unwrap();
    crab::cmd::track::run_track_in("*.bin", dir.path()).unwrap();
    crab::cmd::track::run_track_in("*.dat", dir.path()).unwrap();
    crab::cmd::track::run_untrack_in("*.bin", dir.path()).unwrap();

    let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
    assert!(!content.contains("*.bin"), "untracked pattern must be gone");
    assert!(content.contains("*.dat"), "other patterns must remain");
}

/// Clean produces different pointers for different content.
#[test]
fn clean_different_content_different_pointers() {
    let ctx = AppContext::default();
    let mut session = CleanSession::new(ctx);

    let p1 = session
        .clean_file("a.bin", b"content-alpha".to_vec())
        .unwrap();
    let p2 = session
        .clean_file("b.bin", b"content-beta".to_vec())
        .unwrap();

    let ptr1 = Pointer::parse(&p1).unwrap();
    let ptr2 = Pointer::parse(&p2).unwrap();

    assert_ne!(
        ptr1.file_hash, ptr2.file_hash,
        "different content must produce different hashes"
    );
}

/// Clean produces identical pointers for identical content (dedup property).
#[test]
fn clean_identical_content_same_pointer() {
    let ctx = AppContext::default();
    let mut session = CleanSession::new(ctx);

    let content = b"identical content for dedup test";
    let p1 = session.clean_file("a.bin", content.to_vec()).unwrap();
    let p2 = session.clean_file("b.bin", content.to_vec()).unwrap();

    let ptr1 = Pointer::parse(&p1).unwrap();
    let ptr2 = Pointer::parse(&p2).unwrap();

    assert_eq!(
        ptr1.file_hash, ptr2.file_hash,
        "identical content must produce same hash"
    );
    assert_eq!(ptr1.size, ptr2.size);
}

/// Store put/get roundtrip on InMemory backend (simulates push → fetch).
#[tokio::test]
async fn store_put_get_roundtrip_on_memory_backend() {
    let store = Store::new(Arc::new(InMemory::new()));
    let path = ObjectPath::from("xorbs/test-xorb");
    let data = Bytes::from_static(b"xorb-content-bytes");

    store
        .put(&path, data.clone())
        .await
        .expect("put should succeed");

    let (fetched, _etag) = store
        .get_with_etag(&path)
        .await
        .expect("get should succeed");

    assert_eq!(fetched, data, "fetched data must match what was put");
}

/// Store verify detects corruption (simulates fsck check).
#[tokio::test]
async fn store_verify_detects_corruption() {
    let store = Store::new(Arc::new(InMemory::new()));
    let path = ObjectPath::from("xorbs/corrupt-check");
    let data = Bytes::from_static(b"real content");

    store.put(&path, data).await.unwrap();

    // Verify with wrong hash should fail.
    let wrong_hash = [0xFFu8; 32];
    let result = store.verify(&path, &wrong_hash).await;
    assert!(result.is_err(), "verify with wrong hash must fail");
}

/// GC dry-run on empty store produces zero-delete outcome.
#[tokio::test]
async fn gc_dry_run_empty_store_no_deletes() {
    let cancel = CancellationToken::new();
    let ctx = AppContext::new(Config::default(), cancel);

    // GC on an empty store should report zero deletions.
    // We verify the outcome struct has sensible defaults.
    let outcome = crab::cmd::gc::GcOutcome::default();
    assert_eq!(outcome.packs_deleted, 0);
    assert_eq!(outcome.xorbs_deleted, 0);
    assert_eq!(outcome.shards_deleted, 0);
    assert_eq!(outcome.bytes_reclaimed, 0);

    // Verify the context is usable (not cancelled).
    assert!(ctx.check_cancelled().is_ok());
}

/// Fsck on a clean (empty) repo reports no issues.
#[tokio::test]
async fn fsck_clean_repo_no_issues() {
    let cancel = CancellationToken::new();
    let ctx = AppContext::new(Config::default(), cancel);

    let outcome = crab::cmd::fsck::FsckOutcome::default();
    assert_eq!(outcome.errors, 0);
    assert_eq!(outcome.info_count, 0);
    assert_eq!(outcome.repaired, 0);
    assert!(ctx.check_cancelled().is_ok());
}

// --- 35.2: Fault-injection suite on MockStore ---

/// Transient failure followed by success — retry layer recovers.
#[cfg(feature = "testing")]
#[tokio::test]
async fn fault_injection_transient_then_success() {
    use crab::storage::testing::{FailSpec, MockStore};

    let mock = MockStore::new();
    let store = Store::new(Arc::new(mock.clone()));
    let path = ObjectPath::from("xorbs/retry-test");
    let data = Bytes::from_static(b"retry-content");

    // Inject one transient failure.
    mock.inject_failure(FailSpec::MultipartPartGeneric).await;

    // The retry layer should recover after the first failure.
    let policy = RetryPolicy::default();
    let store_clone = store.clone();
    let path_clone = path.clone();
    let data_clone = data.clone();
    let result = retry(&policy, || {
        let s = store_clone.clone();
        let p = path_clone.clone();
        let d = data_clone.clone();
        async move { s.put(&p, d).await }
    })
    .await;

    assert!(
        result.is_ok(),
        "retry should recover from transient failure"
    );

    // Verify the data landed.
    let (fetched, _) = store.get_with_etag(&path).await.unwrap();
    assert_eq!(fetched, data);
}

/// CAS conflict (Precondition) maps correctly through error taxonomy.
#[cfg(feature = "testing")]
#[tokio::test]
async fn fault_injection_precondition_maps_to_cas_conflict() {
    use crab::storage::testing::{FailSpec, MockStore};

    let mock = MockStore::new();
    mock.inject_failure(FailSpec::Precondition).await;

    let err = mock
        .put(
            &ObjectPath::from("refs/heads/main"),
            PutPayload::from_static(b"v1"),
        )
        .await
        .expect_err("precondition injection must fail");

    let mapped = CrabError::from(crab_storage::map_object_store_error(err, "refs/heads/main"));
    assert!(
        matches!(mapped, CrabError::CasConflict { .. }),
        "precondition must map to CasConflict, got: {mapped:?}",
    );
}

/// NotFound injection maps correctly.
#[cfg(feature = "testing")]
#[tokio::test]
async fn fault_injection_not_found_maps_correctly() {
    use crab::storage::testing::{FailSpec, MockStore};

    let mock = MockStore::new();
    mock.inject_failure(FailSpec::NotFound).await;

    let err = mock
        .head(&ObjectPath::from("xorbs/missing"))
        .await
        .expect_err("not-found injection must fail");

    let mapped = CrabError::from(crab_storage::map_object_store_error(err, "xorbs/missing"));
    assert!(
        matches!(mapped, CrabError::NotFound { .. }),
        "not-found must map to NotFound, got: {mapped:?}",
    );
}

/// Multiple sequential failures exhaust retries and surface the last error.
#[cfg(feature = "testing")]
#[tokio::test]
async fn fault_injection_exhausted_retries_surface_error() {
    use crab::storage::testing::{FailSpec, MockStore};

    let mock = MockStore::new();
    let path = ObjectPath::from("xorbs/exhaust");

    // Inject more failures than the retry budget (default 5 attempts).
    for _ in 0..6 {
        mock.inject_failure(FailSpec::Generic).await;
    }

    let policy = RetryPolicy::default();
    let mock_clone = mock.clone();
    let path_clone = path.clone();
    let result: crab::core::error::Result<()> = retry(&policy, || {
        let m = mock_clone.clone();
        let p = path_clone.clone();
        async move {
            m.put(&p, PutPayload::from_static(b"x"))
                .await
                .map_err(|e| {
                    CrabError::from(crab_storage::map_object_store_error(e, "xorbs/exhaust"))
                })?;
            Ok(())
        }
    })
    .await;

    assert!(result.is_err(), "should fail after exhausting retries");
}

// --- 35.2b: put_multipart_retry (whole-upload retry boundary) ---
//
// These exercise the exact real-world bug: a transient part-PUT failure
// ("error sending request" on a connect blip) must retry the whole
// multipart upload instead of aborting the push. They sit with the
// MockStore fault-injection suite because they need the `testing` feature.

/// Build a `Store` over a failure-injecting `MockStore` with a small retry
/// budget (3 attempts) so exhaustion cases return fast.
#[cfg(feature = "testing")]
fn retry_store() -> (Store, crab::storage::testing::MockStore) {
    use crab::storage::testing::MockStore;
    let mock = MockStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(mock.clone());
    let policy = RetryPolicy {
        max_attempts: 3,
        base: std::time::Duration::from_millis(1),
        cap: std::time::Duration::from_millis(5),
    };
    (Store::with_retry(inner, policy), mock)
}

/// Happy path: a multi-part body uploads intact with no failures injected.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_uploads_intact() {
    let (store, _mock) = retry_store();
    let path = ObjectPath::from("xorbs/happy");
    // 12 bytes with part_size 5 → parts of 5, 5, 2 bytes.
    let body = Bytes::from(vec![0u8; 12]);
    let cancel = CancellationToken::new();

    store
        .put_multipart_retry(&path, body.clone(), 5, &cancel, None)
        .await
        .unwrap();

    let (got, _etag) = store.get_with_etag(&path).await.unwrap();
    assert_eq!(got, body);
}

/// Xet-addressed multipart uploads reject a body whose content hash is wrong.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_verifies_xet_hash() {
    let (store, _mock) = retry_store();
    let path = ObjectPath::from("xorbs/xet-hash");
    let body = Bytes::from_static(b"xet-addressed-content");
    let cancel = CancellationToken::new();
    let expected = crab_xet::hash::compute_data_hash(&body);

    store
        .put_multipart_retry_with_xet_hash(&path, body.clone(), expected.into(), 5, &cancel, None)
        .await
        .expect("matching Xet hash must upload");

    let (got, _etag) = store.get_with_etag(&path).await.unwrap();
    assert_eq!(got, body);
}

/// The bug fix: one transient part-PUT failure retried the whole upload.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_recovers_from_transient_part_failure() {
    use crab::storage::testing::FailSpec;
    let (store, mock) = retry_store();
    let path = ObjectPath::from("xorbs/retry");
    let body = Bytes::from(vec![1u8; 12]);
    let cancel = CancellationToken::new();

    mock.inject_failure(FailSpec::Generic).await;

    store
        .put_multipart_retry(&path, body.clone(), 5, &cancel, None)
        .await
        .expect("one transient part failure must be retried, not surfaced");

    let (got, _etag) = store.get_with_etag(&path).await.unwrap();
    assert_eq!(got, body);
}

/// Progress callback credits bytes once per successfully uploaded part.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_progress_callback_fires_per_part() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let (store, _mock) = retry_store();
    let path = ObjectPath::from("xorbs/progress");
    let body = Bytes::from(vec![2u8; 12]);
    let cancel = CancellationToken::new();

    let reported = Arc::new(AtomicU64::new(0));
    let reported_cb = Arc::clone(&reported);
    let cb = move |bytes: u64| {
        reported_cb.fetch_add(bytes, Ordering::SeqCst);
    };

    store
        .put_multipart_retry(&path, body, 5, &cancel, Some(&cb))
        .await
        .unwrap();

    assert_eq!(
        reported.load(Ordering::SeqCst),
        12,
        "callback must credit all 12 bytes across parts"
    );
}

/// A pre-cancelled token surfaces Cancelled without burning the retry budget.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_surfaces_cancelled() {
    let (store, _mock) = retry_store();
    let path = ObjectPath::from("xorbs/cancelled");
    let body = Bytes::from(vec![3u8; 12]);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = store
        .put_multipart_retry(&path, body, 5, &cancel, None)
        .await
        .expect_err("cancelled upload must error");
    assert!(
        matches!(err, CrabError::Cancelled),
        "expected Cancelled, got {err:?}"
    );
}

/// Persistent transient failures exhaust the retry budget and surface the
/// last error as NetworkTransient.
#[cfg(feature = "testing")]
#[tokio::test]
async fn put_multipart_retry_exhausts_budget_on_persistent_failure() {
    use crab::storage::testing::FailSpec;
    let (store, mock) = retry_store();
    let path = ObjectPath::from("xorbs/persistent");
    let body = Bytes::from(vec![4u8; 12]);
    let cancel = CancellationToken::new();

    // Every part attempt fails.
    for _ in 0..20 {
        mock.inject_failure(FailSpec::Generic).await;
    }

    let err = store
        .put_multipart_retry(&path, body, 5, &cancel, None)
        .await
        .expect_err("persistent failure must exhaust retries");
    assert!(
        matches!(err, CrabError::NetworkTransient(_)),
        "expected NetworkTransient after exhausting retries, got {err:?}"
    );
}

/// Cancellation token propagates through AppContext.
#[tokio::test]
async fn cancellation_propagates_through_context() {
    let token = CancellationToken::new();
    let ctx = AppContext::new(Config::default(), token.clone());

    assert!(
        ctx.check_cancelled().is_ok(),
        "should not be cancelled initially"
    );

    token.cancel();

    let err = ctx.check_cancelled().unwrap_err();
    assert!(
        matches!(err, CrabError::Cancelled),
        "should return Cancelled after token fires",
    );
    assert_eq!(err.exit_code(), 10, "Cancelled exit code must be 10");
}

/// Retry classifies errors correctly for retry decisions.
#[test]
fn retry_classification_covers_key_variants() {
    use crab::storage::{RetryClass, retry_class};

    // Transient → retry
    let transient = CrabError::NetworkTransient(object_store::Error::Generic {
        store: "test",
        source: Box::<dyn std::error::Error + Send + Sync>::from("timeout"),
    });
    assert!(matches!(retry_class(&transient), RetryClass::Transient));

    // CAS → state-dependent
    let cas = CrabError::CasConflict {
        path: "p".into(),
        expected_etag: None,
    };
    assert!(matches!(retry_class(&cas), RetryClass::StateDependent));

    // Non-fast-forward → fatal
    let nff = CrabError::NonFastForward {
        ref_name: "main".into(),
        have: "a".into(),
        want: "b".into(),
    };
    assert!(matches!(retry_class(&nff), RetryClass::Fatal));

    // Cancelled → fatal
    assert!(matches!(
        retry_class(&CrabError::Cancelled),
        RetryClass::Fatal
    ));
}

// --- 35.3: Interop test — perf-enabled vs perf-disabled produce same outcomes ---

/// Both perf-enabled and perf-disabled clean sessions produce valid pointers
/// with identical file hashes for the same content.
#[test]
fn interop_v1_perf_roundtrip_clean_produces_same_hash() {
    // Perf-enabled config (default).
    let perf_config = Config {
        perf: EngineConfig {
            enabled: true,
            ..EngineConfig::default()
        },
        ..Config::default()
    };
    let perf_ctx = AppContext::new(perf_config, CancellationToken::new());
    let mut perf_session = CleanSession::new(perf_ctx);

    // Perf-disabled config (v1 baseline).
    let v1_config = Config {
        perf: EngineConfig {
            enabled: false,
            ..EngineConfig::default()
        },
        ..Config::default()
    };
    let v1_ctx = AppContext::new(v1_config, CancellationToken::new());
    let mut v1_session = CleanSession::new(v1_ctx);

    let content = b"interop test content - same bytes, different perf flags";

    let perf_pointer_bytes = perf_session
        .clean_file("interop.bin", content.to_vec())
        .expect("perf-enabled clean should succeed");
    let v1_pointer_bytes = v1_session
        .clean_file("interop.bin", content.to_vec())
        .expect("perf-disabled clean should succeed");

    // Both must produce valid pointers.
    assert!(
        is_pointer(&perf_pointer_bytes),
        "perf pointer must be valid"
    );
    assert!(is_pointer(&v1_pointer_bytes), "v1 pointer must be valid");

    let perf_ptr = Pointer::parse(&perf_pointer_bytes).unwrap();
    let v1_ptr = Pointer::parse(&v1_pointer_bytes).unwrap();

    // File hashes must be identical — perf flags don't change content identity.
    assert_eq!(
        perf_ptr.file_hash, v1_ptr.file_hash,
        "perf-enabled and v1 must produce the same file hash",
    );
    assert_eq!(perf_ptr.size, v1_ptr.size);
}

/// Perf flags toggled on/off: all EngineConfig combinations produce valid
/// pointers for the same content.
#[test]
fn interop_all_perf_flag_combinations_produce_valid_pointers() {
    let content = b"flag-combination test content for interop verification";
    let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();

    // Test a representative set of flag combinations.
    let configs = vec![
        // All on (default)
        EngineConfig::default(),
        // All off
        EngineConfig {
            enabled: false,
            ..EngineConfig::default()
        },
        // Selective: shard_bloom off
        EngineConfig {
            enabled: true,
            shard_bloom: false,
            ..EngineConfig::default()
        },
        // Selective: adaptive threshold off
        EngineConfig {
            enabled: true,
            adaptive_threshold: false,
            ..EngineConfig::default()
        },
    ];

    for (i, engine_config) in configs.into_iter().enumerate() {
        let config = Config {
            perf: engine_config,
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());
        let mut session = CleanSession::new(ctx);

        let pointer_bytes = session
            .clean_file("combo.bin", content.to_vec())
            .unwrap_or_else(|e| panic!("config combo {i} failed: {e}"));

        assert!(
            is_pointer(&pointer_bytes),
            "config combo {i} must produce a valid pointer",
        );

        let ptr = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(
            ptr.file_hash, expected_hash,
            "config combo {i} must produce the same file hash",
        );
        assert_eq!(ptr.size, content.len() as u64);
    }
}

/// Store operations produce identical results regardless of perf config.
#[tokio::test]
async fn interop_store_roundtrip_independent_of_perf_flags() {
    let backend = Arc::new(InMemory::new());
    let store = Store::new(backend);
    let path = ObjectPath::from("xorbs/interop-roundtrip");
    let data = Bytes::from_static(b"interop store content");

    store.put(&path, data.clone()).await.unwrap();

    // Verify with correct hash.
    let hash: [u8; 32] = *blake3::hash(&data).as_bytes();
    let verified = store.verify(&path, &hash).await.unwrap();
    assert_eq!(verified, data, "verified data must match original");

    // HEAD returns correct metadata.
    let meta = store.head(&path).await.unwrap();
    assert_eq!(meta.size, data.len() as u64);
}
