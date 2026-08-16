//! Integration test: end-to-end speculative hydration prediction flow.
//!
//! Validates that driving an A→B access sequence trains the co-access
//! model, produces correct predictions, triggers speculative hydration
//! via the driver, and that decay cleans up old entries.
//!
//! Fully in-memory — no S3 or network access required.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use crab::core::metrics::Metrics;
use crab::speculation::access_db::AsyncAccessDb;
use crab::speculation::driver::{HydrateFn, SpeculativeDriver};
use crab::speculation::predictor::Predictor;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open an `AsyncAccessDb` in a temp directory.
async fn open_db(tmp: &TempDir) -> AsyncAccessDb {
    let db_path = tmp.path().join("access.db");
    AsyncAccessDb::open(db_path).await.unwrap()
}

/// Record an A→B access sequence at the given base timestamp.
/// Both events land within the 5-second co-access window.
async fn record_a_then_b(db: &AsyncAccessDb, a: &str, b: &str, base_ts: i64, run_id: &str) {
    db.record_access(a.into(), base_ts, run_id.into())
        .await
        .unwrap();
    db.record_access(b.into(), base_ts + 500, run_id.into())
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Drive A→B access 5 times, roll up co-access edges, verify B is
/// predicted on access to A, speculatively hydrated by the driver,
/// and detected as a hit when B is later explicitly accessed.
#[tokio::test(flavor = "multi_thread")]
async fn a_to_b_sequence_produces_speculative_hydration() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(&tmp).await;

    // Step 1: Simulate 5 A→B access sequences within the co-access window.
    // Each sequence uses tightly packed timestamps so all events fall
    // within the predictor's 5-second window for roll-up.
    for run in 0..5 {
        let base_ts = 1000 + (run as i64) * 100;
        record_a_then_b(&db, "src/a.rs", "src/b.rs", base_ts, &format!("run-{run}")).await;
    }

    // Step 2: Build the predictor and run roll-up to create co-access edges.
    let predictor = Arc::new(Predictor::new(db.clone(), 5_000, 8, 3));
    let pairs = predictor.roll_up().await.unwrap();
    assert!(pairs > 0, "roll_up should produce co-access pairs");

    // Step 3: Verify prediction — B should be in the predicted neighbors of A.
    let predictions = predictor.predict("src/a.rs").await.unwrap();
    assert!(
        predictions.contains(&"src/b.rs".to_string()),
        "src/b.rs should be predicted after 5 co-accesses; got: {predictions:?}"
    );

    // Step 4: Wire up a SpeculativeDriver with a mock hydrate callback.
    let hydrated_files: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let hydrated_files_c = Arc::clone(&hydrated_files);

    let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |path: String| {
        let files = Arc::clone(&hydrated_files_c);
        async move {
            files.lock().await.push(path);
            Ok(())
        }
    });

    let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);
    let metrics = Arc::new(Metrics::new());

    let driver = SpeculativeDriver::with_metrics(
        Arc::clone(&predictor),
        2,
        hydrate_fn,
        is_hydrated,
        None,
        Arc::clone(&metrics),
    );

    // Launch speculative hydration for A — should trigger hydration of B.
    driver.launch_speculative("src/a.rs").await;

    // Wait for background tasks to complete.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let files = hydrated_files.lock().await;
    assert!(
        files.contains(&"src/b.rs".to_string()),
        "src/b.rs should have been speculatively hydrated; got: {files:?}"
    );

    // Metrics should reflect the hydration.
    assert!(
        metrics.snapshot().speculation_hydrates_total >= 1,
        "speculation_hydrates_total should be at least 1"
    );

    // Step 5: Verify hit detection — B was speculatively hydrated.
    let was_hit = driver.record_hit_if_speculative("src/b.rs").await;
    assert!(
        was_hit,
        "src/b.rs should be recognized as a speculation hit"
    );
    assert_eq!(
        metrics.snapshot().speculation_hits_total,
        1,
        "speculation_hits_total should be 1 after recording the hit"
    );
}

/// Decay removes old co-access edges and access events, leaving
/// recent entries intact.
#[tokio::test(flavor = "multi_thread")]
async fn decay_removes_old_entries_preserves_recent() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(&tmp).await;

    // Insert old events at ts=1000.
    for run in 0..4 {
        record_a_then_b(
            &db,
            "old/a.rs",
            "old/b.rs",
            1000 + run * 10,
            &format!("old-{run}"),
        )
        .await;
    }

    // Roll up old events to create co-access edges.
    let predictor = Predictor::new(db.clone(), 5_000, 8, 3);
    predictor.roll_up().await.unwrap();

    // Verify old edges exist before decay.
    let old_predictions = predictor.predict("old/a.rs").await.unwrap();
    assert!(
        old_predictions.contains(&"old/b.rs".to_string()),
        "old/b.rs should be predicted before decay"
    );

    // Insert recent events at ts=200_000 (far from old events).
    for run in 0..4 {
        record_a_then_b(
            &db,
            "new/a.rs",
            "new/b.rs",
            200_000 + run * 10,
            &format!("new-{run}"),
        )
        .await;
    }

    // Roll up again to capture new edges.
    predictor.roll_up().await.unwrap();

    // Run decay with max_age_ms=50_000.
    // Cutoff = max(ts) - 50_000 = ~200_040 - 50_000 = ~150_040.
    // Old events at ts=1000..1030 are well below the cutoff.
    let deleted = db.decay(50_000).await.unwrap();
    assert!(deleted > 0, "decay should have removed old entries");

    // Old co-access edges should be gone.
    let old_after_decay = predictor.predict("old/a.rs").await.unwrap();
    assert!(
        old_after_decay.is_empty(),
        "old edges should be removed after decay; got: {old_after_decay:?}"
    );

    // Recent co-access edges should survive.
    let new_after_decay = predictor.predict("new/a.rs").await.unwrap();
    assert!(
        new_after_decay.contains(&"new/b.rs".to_string()),
        "recent edges should survive decay; got: {new_after_decay:?}"
    );
}

/// The driver's concurrency cap is respected during speculative
/// hydration — peak concurrent hydrations never exceed the configured
/// semaphore limit.
#[tokio::test(flavor = "multi_thread")]
async fn concurrency_cap_respected_in_integration() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(&tmp).await;

    // Seed co-access edges for 6 neighbors so the predictor returns
    // more candidates than the concurrency cap allows simultaneously.
    let neighbors = ["n0.rs", "n1.rs", "n2.rs", "n3.rs", "n4.rs", "n5.rs"];
    for neighbor in &neighbors {
        for _ in 0..5 {
            db.upsert_co_access("trigger.rs".into(), (*neighbor).into(), 1000)
                .await
                .unwrap();
        }
    }

    let predictor = Arc::new(Predictor::new(db, 5_000, 8, 3));

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let active_c = Arc::clone(&active);
    let peak_c = Arc::clone(&peak);

    let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
        let active = Arc::clone(&active_c);
        let peak = Arc::clone(&peak_c);
        async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            loop {
                let old_peak = peak.load(Ordering::SeqCst);
                if current <= old_peak
                    || peak
                        .compare_exchange(old_peak, current, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    break;
                }
            }
            // Hold the task open so tasks overlap.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    });

    let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);
    let concurrency_cap = 2;

    let driver = SpeculativeDriver::new(predictor, concurrency_cap, hydrate_fn, is_hydrated, None);

    driver.launch_speculative("trigger.rs").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert!(
        peak.load(Ordering::SeqCst) <= concurrency_cap,
        "peak concurrency {} exceeded cap of {concurrency_cap}",
        peak.load(Ordering::SeqCst)
    );
}
