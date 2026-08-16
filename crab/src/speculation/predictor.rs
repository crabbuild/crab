//! Co-access roll-up and prediction for speculative hydration.
//!
//! [`Predictor`] scans `access_events` for file pairs accessed within a
//! configurable time window, increments `co_access` counters for each
//! observed pair, and provides top-K prediction for likely next files.
//!
//! The roll-up runs as a debounced background task — spawned once on the
//! first filter-process call and persisting for the process lifetime.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::core::error::CrabError;
use crate::speculation::access_db::AsyncAccessDb;

type Result<T> = std::result::Result<T, CrabError>;

/// Default co-access window in milliseconds (5 seconds).
pub const DEFAULT_WINDOW_MS: i64 = 5_000;

/// Default number of top co-accessed paths to return.
pub const DEFAULT_TOP_K: usize = 8;

/// Default minimum co-access count threshold.
pub const DEFAULT_MIN_COUNT: i64 = 3;

/// Default debounce interval for the background roll-up task (2 seconds).
pub const DEFAULT_DEBOUNCE_MS: u64 = 2_000;

/// Co-access roll-up and prediction engine.
///
/// Scans `access_events` for pairs of files accessed within `window_ms`
/// and maintains `co_access` counters. On prediction, returns the top-K
/// co-accessed paths above `min_count`.
pub struct Predictor {
    db: AsyncAccessDb,
    window_ms: i64,
    top_k: usize,
    min_count: i64,
}

impl Predictor {
    /// Create a new predictor with the given configuration.
    pub fn new(db: AsyncAccessDb, window_ms: i64, top_k: usize, min_count: i64) -> Self {
        Self {
            db,
            window_ms,
            top_k,
            min_count,
        }
    }

    /// Run the co-access roll-up: scan recent access events and update
    /// co_access counters for pairs within the window.
    ///
    /// Returns the number of pairs processed (upserted).
    pub async fn roll_up(&self) -> Result<u64> {
        let events = self.db.get_recent_events(self.window_ms).await?;

        if events.len() < 2 {
            debug!(
                event_count = events.len(),
                "roll_up: too few events to form pairs"
            );
            return Ok(0);
        }

        let mut pairs_processed: u64 = 0;

        // For each pair (i, j) where i < j and both are within the window,
        // record a co-access edge from events[i] → events[j].
        for i in 0..events.len() {
            for j in (i + 1)..events.len() {
                let a = &events[i];
                let b = &events[j];

                // Skip self-pairs.
                if a.path == b.path {
                    continue;
                }

                // Only pair events within the window of each other.
                let delta = b.ts_ms.saturating_sub(a.ts_ms);
                if delta > self.window_ms {
                    break;
                }

                // Use the later timestamp for the co-access edge.
                self.db
                    .upsert_co_access(a.path.clone(), b.path.clone(), b.ts_ms)
                    .await?;
                pairs_processed += 1;
            }
        }

        debug!(
            pairs_processed,
            event_count = events.len(),
            "roll_up complete"
        );
        Ok(pairs_processed)
    }

    /// Predict which files are likely to be accessed next given path A.
    ///
    /// Returns top-K co-accessed paths above `min_count` threshold,
    /// ordered by descending co-access count.
    pub async fn predict(&self, path: &str) -> Result<Vec<String>> {
        let neighbors = self
            .db
            .top_k(path.to_owned(), self.top_k, self.min_count)
            .await?;

        debug!(
            path,
            neighbor_count = neighbors.len(),
            "prediction complete"
        );
        Ok(neighbors)
    }

    /// Spawn a debounced background roll-up task that runs periodically.
    ///
    /// The task runs `roll_up` at `debounce_ms` intervals. It persists
    /// for the process lifetime (or until the returned handle is aborted).
    pub fn spawn_background_rollup(
        self: Arc<Self>,
        debounce_ms: u64,
    ) -> tokio::task::JoinHandle<()> {
        info!(debounce_ms, "spawning background co-access roll-up task");

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(debounce_ms));

            // The first tick completes immediately; consume it so the
            // first real roll-up happens after one debounce period.
            interval.tick().await;

            loop {
                interval.tick().await;

                match self.roll_up().await {
                    Ok(pairs) => {
                        if pairs > 0 {
                            debug!(pairs, "background roll-up processed pairs");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "background roll-up failed; will retry next interval");
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn open_predictor(tmp: &TempDir) -> Predictor {
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();
        Predictor::new(db, DEFAULT_WINDOW_MS, DEFAULT_TOP_K, DEFAULT_MIN_COUNT)
    }

    async fn open_predictor_custom(
        tmp: &TempDir,
        window_ms: i64,
        top_k: usize,
        min_count: i64,
    ) -> Predictor {
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();
        Predictor::new(db, window_ms, top_k, min_count)
    }

    #[tokio::test]
    async fn roll_up_empty_db_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        let pairs = predictor.roll_up().await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn roll_up_single_event_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        predictor
            .db
            .record_access("a.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();

        let pairs = predictor.roll_up().await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn roll_up_creates_co_access_edges() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        // Three events within the 5s window.
        predictor
            .db
            .record_access("a.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        predictor
            .db
            .record_access("b.rs".into(), 2000, "run-1".into())
            .await
            .unwrap();
        predictor
            .db
            .record_access("c.rs".into(), 3000, "run-1".into())
            .await
            .unwrap();

        let pairs = predictor.roll_up().await.unwrap();
        // Pairs: (a→b), (a→c), (b→c) = 3
        assert_eq!(pairs, 3);

        // Verify edges exist (min_count=1 to see them).
        let neighbors_a = predictor.db.top_k("a.rs".into(), 10, 1).await.unwrap();
        assert_eq!(neighbors_a.len(), 2);
        assert!(neighbors_a.contains(&"b.rs".to_string()));
        assert!(neighbors_a.contains(&"c.rs".to_string()));
    }

    #[tokio::test]
    async fn roll_up_skips_self_pairs() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        // Same file accessed twice.
        predictor
            .db
            .record_access("a.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        predictor
            .db
            .record_access("a.rs".into(), 2000, "run-1".into())
            .await
            .unwrap();

        let pairs = predictor.roll_up().await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn roll_up_respects_window() {
        let tmp = TempDir::new().unwrap();
        // Use a 1-second window.
        let predictor = open_predictor_custom(&tmp, 1000, 8, 1).await;

        // Events 500ms apart — within window.
        predictor
            .db
            .record_access("a.rs".into(), 9500, "run-1".into())
            .await
            .unwrap();
        predictor
            .db
            .record_access("b.rs".into(), 10000, "run-1".into())
            .await
            .unwrap();

        let pairs = predictor.roll_up().await.unwrap();
        assert_eq!(pairs, 1);

        let neighbors = predictor.db.top_k("a.rs".into(), 10, 1).await.unwrap();
        assert_eq!(neighbors, vec!["b.rs"]);
    }

    #[tokio::test]
    async fn predict_returns_empty_below_threshold() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        // Record events and roll up once — each pair gets count=1,
        // but default min_count is 3.
        predictor
            .db
            .record_access("a.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        predictor
            .db
            .record_access("b.rs".into(), 2000, "run-1".into())
            .await
            .unwrap();

        predictor.roll_up().await.unwrap();

        let predictions = predictor.predict("a.rs").await.unwrap();
        assert!(predictions.is_empty(), "count=1 is below min_count=3");
    }

    #[tokio::test]
    async fn predict_returns_neighbors_above_threshold() {
        let tmp = TempDir::new().unwrap();
        let predictor = open_predictor(&tmp).await;

        // Simulate 3 separate access sequences so roll_up increments
        // the co-access count to 3.
        for run in 0..3 {
            let base_ts = (run as i64) * 10_000;
            predictor
                .db
                .record_access("a.rs".into(), base_ts + 1000, format!("run-{run}"))
                .await
                .unwrap();
            predictor
                .db
                .record_access("b.rs".into(), base_ts + 2000, format!("run-{run}"))
                .await
                .unwrap();
        }

        // Roll up picks up the most recent window; run multiple times
        // to accumulate counts from overlapping windows.
        // Actually, get_recent_events uses a window from the max ts,
        // so we need all events within 5s of the latest. Let's use
        // timestamps that all fall within the window.
        // Re-seed with tightly packed timestamps.
        predictor.db.clear().await.unwrap();

        for run in 0..3 {
            let offset = (run as i64) * 100;
            predictor
                .db
                .record_access("a.rs".into(), 1000 + offset, format!("run-{run}"))
                .await
                .unwrap();
            predictor
                .db
                .record_access("b.rs".into(), 1500 + offset, format!("run-{run}"))
                .await
                .unwrap();
        }

        let pairs = predictor.roll_up().await.unwrap();
        assert!(pairs > 0);

        let predictions = predictor.predict("a.rs").await.unwrap();
        assert!(
            predictions.contains(&"b.rs".to_string()),
            "b.rs should be predicted after 3+ co-accesses"
        );
    }

    #[tokio::test]
    async fn predict_respects_top_k_limit() {
        let tmp = TempDir::new().unwrap();
        // top_k=2, min_count=1
        let predictor = open_predictor_custom(&tmp, 5000, 2, 1).await;

        // Create co-access edges directly via upsert for determinism.
        for i in 0..5 {
            let name = format!("file_{i}.rs");
            let count = 10 - i;
            for _ in 0..count {
                predictor
                    .db
                    .upsert_co_access("a.rs".into(), name.clone(), 1000)
                    .await
                    .unwrap();
            }
        }

        let predictions = predictor.predict("a.rs").await.unwrap();
        assert_eq!(predictions.len(), 2, "should return at most top_k=2");
        // Highest counts first.
        assert_eq!(predictions[0], "file_0.rs");
        assert_eq!(predictions[1], "file_1.rs");
    }

    #[tokio::test]
    async fn background_rollup_runs_and_can_be_aborted() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        // Seed some events.
        db.record_access("a.rs".into(), 1000, "run-1".into())
            .await
            .unwrap();
        db.record_access("b.rs".into(), 2000, "run-1".into())
            .await
            .unwrap();

        let predictor = Arc::new(Predictor::new(
            db.clone(),
            DEFAULT_WINDOW_MS,
            DEFAULT_TOP_K,
            1,
        ));

        // Spawn with a short debounce so it fires quickly in tests.
        let handle = Arc::clone(&predictor).spawn_background_rollup(50);

        // Give it time to run at least one roll-up cycle.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Abort the background task.
        handle.abort();

        // Verify the roll-up ran — co-access edge should exist.
        let neighbors = db.top_k("a.rs".into(), 10, 1).await.unwrap();
        assert!(
            neighbors.contains(&"b.rs".to_string()),
            "background roll-up should have created co-access edge"
        );
    }
}
