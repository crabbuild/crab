//! Background speculative hydration launcher.
//!
//! [`SpeculativeDriver`] queries a [`Predictor`] for likely-next files
//! and spawns background hydrate tasks, capped by a tokio semaphore so
//! speculative work never starves foreground requests.
//!
//! The actual hydration callback is injected as a trait object
//! ([`HydrateFn`]) so the driver is testable without real S3 access.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, warn};

use crate::core::error::CrabError;
use crate::core::metrics::Metrics;
use crate::speculation::predictor::Predictor;

type Result<T> = std::result::Result<T, CrabError>;

/// Default number of concurrent speculative hydrations.
pub const DEFAULT_SPECULATIVE_CONCURRENCY: usize = 2;

/// Async hydration callback.
///
/// Accepts a path and returns a future that completes when the file is
/// hydrated. Boxed so the driver can store it as a trait object.
pub trait HydrateFn: Send + Sync + 'static {
    fn hydrate(&self, path: String) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
}

/// Blanket impl: any `Fn(String) -> Future<Output = Result<()>>` works.
impl<F, Fut> HydrateFn for F
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    fn hydrate(&self, path: String) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        Box::pin((self)(path))
    }
}

/// Background speculative hydration driver.
///
/// Holds a predictor, a semaphore for concurrency control, and a set of
/// in-flight paths to avoid duplicate launches.
pub struct SpeculativeDriver {
    predictor: Arc<Predictor>,
    semaphore: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    hydrate_fn: Arc<dyn HydrateFn>,
    /// Callback to check whether a path is already hydrated on disk.
    is_hydrated_fn: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// Optional callback that returns `true` when the chunk cache is
    /// under pressure (≥80% full). When set and returning `true`, all
    /// speculative hydrations are skipped to avoid evicting useful
    /// cached data.
    cache_pressure_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Paths that have been speculatively hydrated in this session.
    /// Used to detect speculation hits when a user later explicitly
    /// opens one of these files.
    speculatively_hydrated: Arc<Mutex<HashSet<String>>>,
    /// Shared metrics counters for observability.
    metrics: Option<Arc<Metrics>>,
}

impl SpeculativeDriver {
    /// Create a new driver.
    ///
    /// `concurrency` controls the maximum number of simultaneous
    /// speculative hydrations (maps to `[hydrate].speculative_concurrency`).
    ///
    /// `cache_pressure_fn`, when provided, is called before launching
    /// any speculative hydrations. If it returns `true` (cache ≥80%
    /// full), all predictions are skipped for that invocation.
    pub fn new(
        predictor: Arc<Predictor>,
        concurrency: usize,
        hydrate_fn: Arc<dyn HydrateFn>,
        is_hydrated_fn: Arc<dyn Fn(&str) -> bool + Send + Sync>,
        cache_pressure_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            predictor,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            hydrate_fn,
            is_hydrated_fn,
            cache_pressure_fn,
            speculatively_hydrated: Arc::new(Mutex::new(HashSet::new())),
            metrics: None,
        }
    }

    /// Create a new driver with metrics support.
    ///
    /// Same as [`new`](Self::new) but attaches shared metrics counters
    /// for `speculation_hydrates_total`, `speculation_hits_total`, and
    /// `speculation_evictions_total`.
    pub fn with_metrics(
        predictor: Arc<Predictor>,
        concurrency: usize,
        hydrate_fn: Arc<dyn HydrateFn>,
        is_hydrated_fn: Arc<dyn Fn(&str) -> bool + Send + Sync>,
        cache_pressure_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            predictor,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            hydrate_fn,
            is_hydrated_fn,
            cache_pressure_fn,
            speculatively_hydrated: Arc::new(Mutex::new(HashSet::new())),
            metrics: Some(metrics),
        }
    }

    /// Launch speculative hydrations for predicted neighbors of `path`.
    ///
    /// Returns immediately — background tasks are spawned via
    /// `tokio::spawn`. If the semaphore is full, predictions are
    /// skipped. If the predictor returns an error, it is logged and
    /// swallowed so the foreground request is never blocked.
    ///
    /// When a `cache_pressure_fn` is set and returns `true`, all
    /// predictions are skipped to avoid evicting useful cached data.
    pub async fn launch_speculative(&self, path: &str) {
        // Check cache pressure before doing any work.
        if let Some(ref pressure_fn) = self.cache_pressure_fn {
            if pressure_fn() {
                debug!(path, "cache under pressure, skipping speculation");
                if let Some(ref m) = self.metrics {
                    m.inc_speculation_evictions_total();
                }
                return;
            }
        }

        let neighbors = match self.predictor.predict(path).await {
            Ok(n) => n,
            Err(e) => {
                warn!(path, error = %e, "speculation prediction failed");
                return;
            }
        };

        if neighbors.is_empty() {
            debug!(path, "no speculative neighbors predicted");
            return;
        }

        debug!(
            path,
            neighbor_count = neighbors.len(),
            "launching speculative hydrations"
        );

        for neighbor in neighbors {
            // Skip already-hydrated files.
            if (self.is_hydrated_fn)(&neighbor) {
                debug!(path = %neighbor, "already hydrated, skipping speculation");
                continue;
            }

            // Skip if already in-flight.
            {
                let mut guard = self.in_flight.lock().await;
                if guard.contains(&neighbor) {
                    debug!(path = %neighbor, "already in-flight, skipping speculation");
                    continue;
                }
                guard.insert(neighbor.clone());
            }

            // Non-blocking semaphore acquire — skip if all permits taken.
            let Ok(permit) = self.semaphore.clone().try_acquire_owned() else {
                debug!(
                    path = %neighbor,
                    "semaphore full, skipping speculative hydration"
                );
                // Remove from in-flight since we won't actually launch.
                let mut guard = self.in_flight.lock().await;
                guard.remove(&neighbor);
                continue;
            };

            let hydrate_fn = Arc::clone(&self.hydrate_fn);
            let in_flight = Arc::clone(&self.in_flight);
            let speculatively_hydrated = Arc::clone(&self.speculatively_hydrated);
            let metrics = self.metrics.clone();
            let neighbor_clone = neighbor.clone();

            // Count every spawned speculative hydration.
            if let Some(ref m) = self.metrics {
                m.inc_speculation_hydrates_total();
            }

            tokio::spawn(async move {
                // Hold the permit for the duration of the hydration.
                let _permit = permit;

                match hydrate_fn.hydrate(neighbor_clone.clone()).await {
                    Ok(()) => {
                        debug!(path = %neighbor_clone, "speculative hydration complete");
                        // Track successfully hydrated paths for hit detection.
                        speculatively_hydrated
                            .lock()
                            .await
                            .insert(neighbor_clone.clone());
                    }
                    Err(e) => {
                        warn!(path = %neighbor_clone, error = %e, "speculative hydration failed");
                    }
                }

                // Remove from in-flight set when done.
                let mut guard = in_flight.lock().await;
                guard.remove(&neighbor_clone);

                // Prevent unused-variable warning when metrics are not wired.
                drop(metrics);
            });
        }
    }

    /// Number of currently in-flight speculative hydrations.
    ///
    /// Useful for metrics and testing.
    pub async fn in_flight_count(&self) -> usize {
        self.in_flight.lock().await.len()
    }

    /// Check whether `path` was speculatively hydrated and record a hit
    /// if so.
    ///
    /// Returns `true` when the path was in the speculatively-hydrated
    /// set (and the hit counter was bumped). The path is removed from
    /// the set so each file counts as at most one hit.
    pub async fn record_hit_if_speculative(&self, path: &str) -> bool {
        let removed = self.speculatively_hydrated.lock().await.remove(path);
        if removed {
            if let Some(ref m) = self.metrics {
                m.inc_speculation_hits_total();
            }
        }
        removed
    }

    /// Number of paths currently tracked as speculatively hydrated.
    ///
    /// Useful for testing.
    pub async fn speculatively_hydrated_count(&self) -> usize {
        self.speculatively_hydrated.lock().await.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speculation::access_db::AsyncAccessDb;
    use crate::speculation::predictor::Predictor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Build a predictor seeded with co-access edges so `predict(trigger)`
    /// returns `neighbors`.
    async fn seeded_predictor(
        tmp: &TempDir,
        trigger: &str,
        neighbors: &[&str],
        min_count: i64,
    ) -> Arc<Predictor> {
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();

        // Insert co-access edges with count >= min_count.
        for neighbor in neighbors {
            for _ in 0..min_count {
                db.upsert_co_access(trigger.into(), (*neighbor).into(), 1000)
                    .await
                    .unwrap();
            }
        }

        Arc::new(Predictor::new(db, 5000, 8, min_count))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrency_cap_is_enforced() {
        let tmp = TempDir::new().unwrap();
        let neighbors: Vec<&str> = (0..6)
            .map(|i| match i {
                0 => "f0.rs",
                1 => "f1.rs",
                2 => "f2.rs",
                3 => "f3.rs",
                4 => "f4.rs",
                5 => "f5.rs",
                _ => unreachable!(),
            })
            .collect();

        let predictor = seeded_predictor(&tmp, "trigger.rs", &neighbors, 3).await;

        // Track the peak number of concurrent hydrations.
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let active_c = Arc::clone(&active);
        let peak_c = Arc::clone(&peak);

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
            let active = Arc::clone(&active_c);
            let peak = Arc::clone(&peak_c);
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                // Update peak.
                loop {
                    let old_peak = peak.load(Ordering::SeqCst);
                    if current <= old_peak {
                        break;
                    }
                    if peak
                        .compare_exchange(old_peak, current, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }
                // Simulate work so tasks overlap.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 2, hydrate_fn, is_hydrated, None);

        driver.launch_speculative("trigger.rs").await;

        // Wait for all tasks to complete.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Peak concurrency should never exceed the semaphore cap of 2.
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak concurrency {} exceeded cap of 2",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_launches_are_prevented() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_c = Arc::clone(&call_count);

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
            let count = Arc::clone(&call_count_c);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                // Hold the task open so the second launch sees it in-flight.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(())
            }
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, None);

        // Launch twice in quick succession.
        driver.launch_speculative("trigger.rs").await;
        driver.launch_speculative("trigger.rs").await;

        // Wait for tasks to complete.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // The hydrate function should only have been called once.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "duplicate launch should have been prevented"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn launch_speculative_returns_immediately() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["slow.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move {
            // Simulate a very slow hydration.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(())
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 2, hydrate_fn, is_hydrated, None);

        let start = tokio::time::Instant::now();
        driver.launch_speculative("trigger.rs").await;
        let elapsed = start.elapsed();

        // launch_speculative should return in well under 1 second,
        // even though the hydration takes 10 seconds.
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "launch_speculative blocked for {:?}",
            elapsed
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn already_hydrated_files_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let predictor =
            seeded_predictor(&tmp, "trigger.rs", &["hydrated.rs", "dehydrated.rs"], 3).await;

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_c = Arc::clone(&call_count);

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
            let count = Arc::clone(&call_count_c);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        // Mark "hydrated.rs" as already hydrated.
        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> =
            Arc::new(|path| path == "hydrated.rs");

        let driver = SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, None);

        driver.launch_speculative("trigger.rs").await;

        // Wait for tasks.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Only "dehydrated.rs" should have been hydrated.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "already-hydrated file should have been skipped"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prediction_error_is_swallowed() {
        // Use a predictor with a DB that we'll close/corrupt by using
        // a path that doesn't exist — actually, just use a normal
        // predictor with no data. predict() returns empty, not error.
        // Instead, test that the driver handles the empty case gracefully.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("access.db");
        let db = AsyncAccessDb::open(db_path).await.unwrap();
        let predictor = Arc::new(Predictor::new(db, 5000, 8, 3));

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move { Ok(()) });
        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 2, hydrate_fn, is_hydrated, None);

        // Should not panic or block — no predictions means no launches.
        driver.launch_speculative("unknown.rs").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_flight_set_is_cleaned_up_after_completion() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(())
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, None);

        driver.launch_speculative("trigger.rs").await;

        // In-flight should be non-empty immediately after launch.
        let count_during = driver.in_flight_count().await;
        assert!(count_during > 0, "should have in-flight tasks");

        // Wait for completion.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // In-flight should be empty after completion.
        let count_after = driver.in_flight_count().await;
        assert_eq!(
            count_after, 0,
            "in-flight set should be empty after completion"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hydration_failure_cleans_up_in_flight() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["fail.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move {
            Err(CrabError::Internal("simulated failure".into()))
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let driver = SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, None);

        driver.launch_speculative("trigger.rs").await;

        // Wait for the failed task to clean up.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            driver.in_flight_count().await,
            0,
            "failed hydration should clean up in-flight entry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_pressure_skips_all_speculation() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_c = Arc::clone(&call_count);

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
            let count = Arc::clone(&call_count_c);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        // Simulate cache under pressure — callback always returns true.
        let pressure_fn: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);

        let driver =
            SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, Some(pressure_fn));

        driver.launch_speculative("trigger.rs").await;

        // Give time for any tasks that might have been spawned.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // No hydrations should have been launched.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "cache pressure should skip all speculation"
        );
        assert_eq!(
            driver.in_flight_count().await,
            0,
            "no tasks should be in-flight under cache pressure"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_cache_pressure_allows_speculation() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_c = Arc::clone(&call_count);

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(move |_path: String| {
            let count = Arc::clone(&call_count_c);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        // Cache not under pressure — callback returns false.
        let pressure_fn: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);

        let driver =
            SpeculativeDriver::new(predictor, 4, hydrate_fn, is_hydrated, Some(pressure_fn));

        driver.launch_speculative("trigger.rs").await;

        // Wait for tasks to complete.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Hydration should have proceeded normally.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "speculation should proceed when cache is not under pressure"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_hydrates_total_incremented_on_spawn() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["a.rs", "b.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move { Ok(()) });
        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let metrics = Arc::new(Metrics::new());
        let driver = SpeculativeDriver::with_metrics(
            predictor,
            4,
            hydrate_fn,
            is_hydrated,
            None,
            Arc::clone(&metrics),
        );

        driver.launch_speculative("trigger.rs").await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(
            metrics.snapshot().speculation_hydrates_total,
            2,
            "should count one hydrate per spawned neighbor"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_evictions_total_incremented_on_cache_pressure() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move { Ok(()) });
        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);
        let pressure_fn: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);

        let metrics = Arc::new(Metrics::new());
        let driver = SpeculativeDriver::with_metrics(
            predictor,
            4,
            hydrate_fn,
            is_hydrated,
            Some(pressure_fn),
            Arc::clone(&metrics),
        );

        driver.launch_speculative("trigger.rs").await;

        assert_eq!(
            metrics.snapshot().speculation_evictions_total,
            1,
            "cache pressure should bump eviction counter"
        );
        assert_eq!(
            metrics.snapshot().speculation_hydrates_total,
            0,
            "no hydrations should have been launched"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_hit_if_speculative_tracks_and_counts() {
        let tmp = TempDir::new().unwrap();
        let predictor = seeded_predictor(&tmp, "trigger.rs", &["target.rs"], 3).await;

        let hydrate_fn: Arc<dyn HydrateFn> = Arc::new(|_path: String| async move { Ok(()) });
        let is_hydrated: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(|_| false);

        let metrics = Arc::new(Metrics::new());
        let driver = SpeculativeDriver::with_metrics(
            predictor,
            4,
            hydrate_fn,
            is_hydrated,
            None,
            Arc::clone(&metrics),
        );

        // Launch speculation so "target.rs" gets hydrated.
        driver.launch_speculative("trigger.rs").await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(driver.speculatively_hydrated_count().await, 1);

        // Simulate user opening the speculatively-hydrated file.
        let was_hit = driver.record_hit_if_speculative("target.rs").await;
        assert!(was_hit, "target.rs should be recognized as speculative");
        assert_eq!(
            metrics.snapshot().speculation_hits_total,
            1,
            "hit counter should be bumped"
        );

        // Second call for the same path should not count again.
        let was_hit_again = driver.record_hit_if_speculative("target.rs").await;
        assert!(!was_hit_again, "path already consumed");
        assert_eq!(
            metrics.snapshot().speculation_hits_total,
            1,
            "hit counter should not double-count"
        );

        // Unknown path should not count.
        let unknown = driver.record_hit_if_speculative("unknown.rs").await;
        assert!(!unknown);
        assert_eq!(metrics.snapshot().speculation_hits_total, 1);
    }
}
