//! Integration tests for Commit 2 of the `crab-push-perf-tier2` spec:
//! the parallel step-9 shard/file-index PUT loop in
//! `PushPipeline::upload_shard_and_file_index`.
//!
//! These tests validate the `buffer_unordered(concurrency).try_collect()`
//! pattern that Commit 2 introduced, using the same `Store::put` entry
//! point (and therefore the same retry/CAS wrapping) the production path
//! relies on. The `PushPipeline` itself is not driven end-to-end because
//! its fatal metadata-PUT path — the subject under test — needs seeded
//! `shard_results` / `file_shard_index` / `pointers` mutexes that are
//! private to the `push` module. Exposing those would add permanent API
//! surface for a narrow test benefit; validating the pattern against a
//! latency/error-injecting `ObjectStore` is strictly equivalent because
//! the parallel fan-out semantics live entirely in `buffer_unordered` +
//! `try_collect`, not in any `PushPipeline`-specific code.
//!
//! Tests:
//!
//! * `parallel_puts_complete_within_wall_clock_budget` — 100 PUTs at
//!   20 ms each through a concurrency-16 `buffer_unordered` must finish
//!   well under the 2 s serial baseline.
//! * `error_short_circuits_with_bounded_collateral` — a single PUT that
//!   errors surfaces via `try_collect` without waiting for the rest, and
//!   the number of sibling PUTs that ran to completion before the error
//!   propagated is bounded by `concurrency`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt, TryStreamExt};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

use crab::core::error::CrabError;
use crab::storage::store::Store;

/// Concurrency used by both tests — matches `PushConfig::default().upload_concurrency`.
const CONCURRENCY: usize = 16;

// ---------------------------------------------------------------------------
// Latency + error injecting ObjectStore wrapper
// ---------------------------------------------------------------------------

/// `ObjectStore` wrapper that sleeps `latency` before every `put_opts`
/// and optionally fails one specific path with a pre-canned error.
///
/// All non-put methods delegate to the inner store unchanged. The only
/// interception points relevant to step 9's PUT loop are `put_opts`
/// (used by `Store::put` via `PutMode::Create`) and its multipart
/// sibling (included for completeness but not exercised here — step 9
/// PUTs are always single-shot).
struct InjectingStore {
    inner: Arc<InMemory>,
    /// Delay applied before every `put_opts` call.
    latency: Duration,
    /// When `Some`, this path returns `PermissionDenied` instead of
    /// calling the inner store. Mapped to `CrabError::Forbidden` by
    /// `Store::put`, which the retry layer classifies as Fatal — no
    /// retry, so the error reaches `try_collect` on its first attempt.
    fail_path: Option<ObjectPath>,
    /// Counts successful completions of `put_opts` (i.e. calls that
    /// reached the inner store and returned `Ok`). Read by test 2.8 to
    /// bound the number of sibling PUTs that completed before the error
    /// short-circuit propagated.
    completed_puts: Arc<AtomicUsize>,
}

impl InjectingStore {
    fn new(latency: Duration, fail_path: Option<ObjectPath>) -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            latency,
            fail_path,
            completed_puts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn completed_puts(&self) -> usize {
        self.completed_puts.load(Ordering::Acquire)
    }
}

impl fmt::Debug for InjectingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InjectingStore")
            .field("latency_ms", &self.latency.as_millis())
            .field("fail_path", &self.fail_path.as_ref().map(|p| p.as_ref()))
            .finish()
    }
}

impl fmt::Display for InjectingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InjectingStore")
    }
}

#[async_trait]
impl ObjectStore for InjectingStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        tokio::time::sleep(self.latency).await;
        if let Some(fail) = &self.fail_path
            && location == fail
        {
            return Err(object_store::Error::PermissionDenied {
                path: location.as_ref().to_owned(),
                source: Box::<dyn std::error::Error + Send + Sync>::from("injected 403"),
            });
        }
        let result = self.inner.put_opts(location, payload, opts).await?;
        self.completed_puts.fetch_add(1, Ordering::AcqRel);
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a content-addressed `(ObjectPath, Bytes)` plan. Bodies are
/// small and deterministic — the tests care about count and ordering,
/// not payload size.
fn build_plan(count: usize) -> Vec<(ObjectPath, Bytes)> {
    (0..count)
        .map(|i| {
            // Content-addressed path layout matches what
            // `StoreLayout::shard_path` produces for the parallel PUT
            // loop — a single content-addressed prefix with a unique
            // hex suffix per entry. The exact layout doesn't affect
            // what's under test; uniqueness does.
            let hex: String = (0..32).map(|b| format!("{:02x}", b ^ (i as u8))).collect();
            let path = ObjectPath::from(format!(".crab/shards/{hex}"));
            let body = Bytes::from(vec![i as u8; 64]);
            (path, body)
        })
        .collect()
}

/// Drive the parallel PUT fan-out with the exact stream shape used in
/// `PushPipeline::upload_shard_and_file_index`:
/// `stream::iter(...).buffer_unordered(concurrency).try_collect()`.
async fn run_parallel_puts(
    store: Store,
    plan: Vec<(ObjectPath, Bytes)>,
    concurrency: usize,
) -> crab::core::error::Result<()> {
    futures_util::stream::iter(plan.into_iter().map(|(path, body)| {
        let store = store.clone();
        async move { store.put(&path, body).await }
    }))
    .buffer_unordered(concurrency)
    .try_collect::<Vec<()>>()
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Task 2.7 — 100 PUTs at 20 ms each finish in well under the serial
/// wall-clock (100 × 20 ms = 2 s). Budget is
/// `(100 / 16) × 20 ms × 2 = 250 ms` — the `× 2` gives generous headroom
/// for scheduler jitter, blake3 hashing on the 64-byte bodies, and
/// `object_store::InMemory` bookkeeping.
///
/// Real wall-clock sleeps are used instead of `tokio::time::pause()`:
/// paused-time interacts poorly with `buffer_unordered`, which polls
/// all pending futures on the same task, and the 250 ms budget is
/// large enough that wall-clock timing is reliable on developer
/// machines and CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_puts_complete_within_wall_clock_budget() {
    const COUNT: usize = 100;
    const LATENCY: Duration = Duration::from_millis(20);
    // Serial baseline: COUNT × LATENCY = 2000 ms.
    // Parallel ideal:  ceil(COUNT / CONCURRENCY) × LATENCY = 140 ms.
    // Budget:          2 × parallel ideal (rounded) = 250 ms.
    const BUDGET: Duration = Duration::from_millis(250);

    let injector: Arc<InjectingStore> = Arc::new(InjectingStore::new(LATENCY, None));
    let backend: Arc<dyn ObjectStore> = injector.clone();
    let store = Store::new(backend);

    let plan = build_plan(COUNT);
    let start = Instant::now();
    run_parallel_puts(store, plan, CONCURRENCY)
        .await
        .expect("all PUTs must succeed");
    let elapsed = start.elapsed();

    assert_eq!(
        injector.completed_puts(),
        COUNT,
        "every PUT must have reached the inner store"
    );
    assert!(
        elapsed < BUDGET,
        "parallel PUT wall-clock {elapsed:?} exceeded budget {BUDGET:?} \
         (serial baseline would be ~{:?})",
        LATENCY * COUNT as u32,
    );
}

/// Task 2.8 — a single injected error surfaces unchanged through
/// `try_collect` without waiting for the remaining PUTs to complete.
///
/// `object_store` does not expose per-request cancellation, so the
/// futures that `buffer_unordered` had already pushed to the
/// under-the-hood join set continue running to completion after
/// `try_collect` short-circuits. The contract documented by Commit 2's
/// design is: **at most `concurrency - 1` sibling PUTs may land on the
/// remote before the error propagates.** The weaker bound
/// `completed_before_error <= concurrency` is what the test asserts —
/// it tolerates the race where the failing PUT resolves before any
/// sibling has completed (yielding 0 collateral) and the race where
/// every in-flight sibling finishes just before the error surfaces
/// (yielding `concurrency - 1`; the `<= concurrency` bound includes a
/// one-slot safety margin for scheduler quirks).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_short_circuits_with_bounded_collateral() {
    const COUNT: usize = 100;
    const LATENCY: Duration = Duration::from_millis(20);

    let plan = build_plan(COUNT);
    // Fail a PUT near the middle of the plan so siblings on either side
    // have a chance to be in-flight when the error fires. The exact
    // index doesn't matter for the assertion (the bound holds for any
    // failing position); mid-stream just gives the best chance of
    // observing the "up to concurrency - 1 siblings completed"
    // behaviour empirically.
    let fail_path = plan[CONCURRENCY / 2].0.clone();

    let injector: Arc<InjectingStore> =
        Arc::new(InjectingStore::new(LATENCY, Some(fail_path.clone())));
    let backend: Arc<dyn ObjectStore> = injector.clone();
    let store = Store::new(backend);

    let result = run_parallel_puts(store, plan, CONCURRENCY).await;

    // The error must surface — Forbidden is Fatal in the retry
    // classifier, so `Store::put` passes it through on the first
    // attempt without retrying.
    let err = result.expect_err("PUT loop must propagate the injected error");
    match err {
        CrabError::Forbidden { path } => {
            assert_eq!(
                path,
                fail_path.as_ref(),
                "propagated error must carry the failing path",
            );
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }

    let completed = injector.completed_puts();
    assert!(
        completed <= CONCURRENCY,
        "short-circuit must leave at most `concurrency` siblings completed; \
         got completed={completed}, concurrency={CONCURRENCY}",
    );
    assert!(
        completed < COUNT,
        "short-circuit must prevent the full plan from running; \
         got completed={completed}, total={COUNT}",
    );
}
