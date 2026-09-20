#![cfg(feature = "replica")]

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_ltx::{
    CaptureBatch, CaptureTiming, CellReplica, Host, Limits, LtxPhase, LtxReadOrigin,
    LtxRequestOutcome, LtxTelemetry, ManagedDb, RootRef, VerifiedPlan, restore_exact,
};
use crab_storage::Store;
use futures_util::{StreamExt as _, stream::BoxStream};
use object_store::{
    GetOptions, GetRange, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
    path::Path,
};

const NO_FAULT: u8 = 0;
const SHORT_RANGE: u8 = 1;
const CORRUPT_RANGE: u8 = 2;
const TIMEOUT_RANGE: u8 = 3;

#[derive(Default)]
struct RecordingTelemetry {
    phases: Mutex<Vec<LtxPhase>>,
    logical_reads: Mutex<Vec<LtxReadOrigin>>,
    reads: Mutex<Vec<(LtxReadOrigin, LtxRequestOutcome, u64)>>,
}

impl LtxTelemetry for RecordingTelemetry {
    fn phase(&self, phase: LtxPhase, _: Duration, _: bool) {
        self.phases.lock().unwrap().push(phase);
    }

    fn logical_read(&self, origin: LtxReadOrigin) {
        self.logical_reads.lock().unwrap().push(origin);
    }

    fn origin_request(&self, origin: LtxReadOrigin, outcome: LtxRequestOutcome, bytes: u64) {
        self.reads.lock().unwrap().push((origin, outcome, bytes));
    }
}

struct ReadStats {
    active: AtomicUsize,
    peak: AtomicUsize,
    active_ranges: AtomicUsize,
    peak_ranges: AtomicUsize,
    active_range_bytes: AtomicUsize,
    peak_range_bytes: AtomicUsize,
    maximum_range_bytes: AtomicUsize,
    body_requests: AtomicUsize,
}

impl ReadStats {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            active_ranges: AtomicUsize::new(0),
            peak_ranges: AtomicUsize::new(0),
            active_range_bytes: AtomicUsize::new(0),
            peak_range_bytes: AtomicUsize::new(0),
            maximum_range_bytes: AtomicUsize::new(0),
            body_requests: AtomicUsize::new(0),
        }
    }

    fn begin(self: &Arc<Self>, range_bytes: Option<usize>) -> ReadGuard {
        self.body_requests.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        if let Some(bytes) = range_bytes {
            let active = self.active_ranges.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_ranges.fetch_max(active, Ordering::SeqCst);
            let active_bytes = self.active_range_bytes.fetch_add(bytes, Ordering::SeqCst) + bytes;
            self.peak_range_bytes
                .fetch_max(active_bytes, Ordering::SeqCst);
        }
        ReadGuard {
            stats: Arc::clone(self),
            range_bytes,
        }
    }

    fn reset(&self) {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        assert_eq!(self.active_ranges.load(Ordering::SeqCst), 0);
        assert_eq!(self.active_range_bytes.load(Ordering::SeqCst), 0);
        self.peak.store(0, Ordering::SeqCst);
        self.peak_ranges.store(0, Ordering::SeqCst);
        self.peak_range_bytes.store(0, Ordering::SeqCst);
        self.maximum_range_bytes.store(0, Ordering::SeqCst);
        self.body_requests.store(0, Ordering::SeqCst);
    }
}

struct ReadGuard {
    stats: Arc<ReadStats>,
    range_bytes: Option<usize>,
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(bytes) = self.range_bytes {
            self.stats.active_ranges.fetch_sub(1, Ordering::SeqCst);
            self.stats
                .active_range_bytes
                .fetch_sub(bytes, Ordering::SeqCst);
        }
    }
}

struct InstrumentedStore {
    inner: Arc<dyn ObjectStore>,
    delay: Duration,
    fault: AtomicU8,
    stats: Arc<ReadStats>,
}

impl InstrumentedStore {
    fn new(inner: Arc<dyn ObjectStore>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner,
            delay,
            fault: AtomicU8::new(NO_FAULT),
            stats: Arc::new(ReadStats::new()),
        })
    }

    fn arm(&self, fault: u8) {
        self.fault.store(fault, Ordering::SeqCst);
    }

    fn reset(&self) {
        self.stats.reset();
    }
}

impl fmt::Debug for InstrumentedStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstrumentedStore")
    }
}

impl fmt::Display for InstrumentedStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstrumentedStore")
    }
}

#[async_trait]
impl ObjectStore for InstrumentedStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let is_range = options.range.is_some();
        let range_bytes = if let Some(GetRange::Bounded(range)) = &options.range {
            let bytes = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
            self.stats
                .maximum_range_bytes
                .fetch_max(bytes, Ordering::SeqCst);
            Some(bytes)
        } else {
            None
        };
        let _guard = (!options.head).then(|| self.stats.begin(range_bytes));
        if !options.head {
            tokio::time::sleep(self.delay).await;
        }
        if is_range && self.fault.load(Ordering::SeqCst) == TIMEOUT_RANGE {
            return Err(object_store::Error::Generic {
                store: "InstrumentedStore",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "injected range timeout",
                )),
            });
        }
        let result = self.inner.get_opts(location, options).await?;
        let fault = self.fault.load(Ordering::SeqCst);
        if !is_range || fault == NO_FAULT {
            return Ok(result);
        }

        let meta = result.meta.clone();
        let range = result.range.clone();
        let attributes = result.attributes.clone();
        let extensions = result.extensions.clone();
        let bytes = result.bytes().await?;
        let bytes = match fault {
            SHORT_RANGE => bytes.slice(..bytes.len().saturating_sub(1)),
            CORRUPT_RANGE => {
                let mut corrupted = bytes.to_vec();
                if let Some(first) = corrupted.first_mut() {
                    *first ^= 1;
                }
                Bytes::from(corrupted)
            }
            _ => bytes,
        };
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures_util::stream::once(async move { Ok(bytes) }).boxed(),
            ),
            meta,
            range,
            attributes,
            extensions,
        })
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

struct Fixture {
    backend: Arc<InMemory>,
    cell: [u8; 32],
    incarnation: [u8; 16],
    root: RootRef,
    expected: Vec<u8>,
}

async fn fixture(extra_segments: usize) -> Fixture {
    let directory = tempfile::TempDir::new().unwrap();
    let source = directory.path().join("source.sqlite");
    let mut writer = ManagedDb::open(&source, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE counter(value INTEGER NOT NULL);\
                 INSERT INTO counter VALUES(0);\
                 CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let first = writer.capture().unwrap();
    let mut segments = first.segments;
    let mut position = first.position;
    for _ in 0..extra_segments {
        writer
            .transaction(|transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(())
            })
            .unwrap();
        let next = writer.capture().unwrap();
        segments.extend(next.segments);
        position = next.position;
    }
    writer.close().unwrap();
    let batch = CaptureBatch {
        segments,
        position,
        timing: CaptureTiming::default(),
    };

    let expected_path = directory.path().join("expected.sqlite");
    let plan = VerifiedPlan::new(&batch.segments, batch.position, Limits::default()).unwrap();
    restore_exact(&plan, &expected_path).unwrap();
    let expected = std::fs::read(expected_path).unwrap();

    let backend = Arc::new(InMemory::new());
    let cell = [141; 32];
    let incarnation = [142; 16];
    let replica = cell_replica(
        Store::new(backend.clone()),
        cell,
        incarnation,
        Host::default(),
    );
    let root = replica.prepare(None, &batch, 1, 1).await.unwrap().root();
    Fixture {
        backend,
        cell,
        incarnation,
        root,
        expected,
    }
}

fn cell_replica(store: Store, cell: [u8; 32], incarnation: [u8; 16], host: Host) -> CellReplica {
    CellReplica::new(
        CellStorageLayout::new(store, Path::from("parallel"), [143; 16]),
        cell,
        incarnation,
        Limits::default(),
    )
    .unwrap()
    .with_host(host)
}

fn delayed_replica(
    fixture: &Fixture,
    delay: Duration,
    io_slots: usize,
) -> (Arc<InstrumentedStore>, CellReplica) {
    let store = InstrumentedStore::new(fixture.backend.clone(), delay);
    let replica = cell_replica(
        Store::new(store.clone()),
        fixture.cell,
        fixture.incarnation,
        Host::default().with_io_slots(Arc::new(tokio::sync::Semaphore::new(io_slots))),
    );
    (store, replica)
}

fn p95(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() * 95).div_ceil(100) - 1]
}

#[tokio::test(start_paused = true)]
async fn cold_open_and_restore_improve_p95_under_object_latency() {
    let fixture = fixture(192).await;
    for delay_ms in [5, 20, 100] {
        let delay = Duration::from_millis(delay_ms);
        let (store, replica) = delayed_replica(&fixture, delay, 8);
        replica.open_root(&fixture.root).await.unwrap();

        let mut open_samples = Vec::new();
        for _ in 0..5 {
            store.reset();
            let started = tokio::time::Instant::now();
            replica.open_root(&fixture.root).await.unwrap();
            open_samples.push(started.elapsed());
            assert!(store.stats.peak.load(Ordering::SeqCst) >= 3);
        }
        assert!(p95(&mut open_samples) < delay * 4);

        let opened = replica.open_root(&fixture.root).await.unwrap();
        opened.paged().read_page(1).await.unwrap();
        let mut warm_read_samples = Vec::new();
        for _ in 0..5 {
            store.reset();
            let started = tokio::time::Instant::now();
            opened.paged().read_page(1).await.unwrap();
            warm_read_samples.push(started.elapsed());
            assert_eq!(store.stats.body_requests.load(Ordering::SeqCst), 1);
        }
        assert!(p95(&mut warm_read_samples) <= delay);

        let warm = tempfile::TempDir::new().unwrap();
        opened
            .restore(&warm.path().join("warm.sqlite"))
            .await
            .unwrap();
        let mut restore_samples = Vec::new();
        let mut minimum_requests = usize::MAX;
        for sample in 0..5 {
            store.reset();
            let output = tempfile::TempDir::new().unwrap();
            let destination = output.path().join(format!("restore-{sample}.sqlite"));
            let started = tokio::time::Instant::now();
            opened.restore(&destination).await.unwrap();
            restore_samples.push(started.elapsed());
            minimum_requests =
                minimum_requests.min(store.stats.body_requests.load(Ordering::SeqCst));
            assert_eq!(std::fs::read(destination).unwrap(), fixture.expected);
            assert!(store.stats.peak_ranges.load(Ordering::SeqCst) > 1);
            assert!(store.stats.maximum_range_bytes.load(Ordering::SeqCst) <= 1 << 20);
        }
        assert!(p95(&mut restore_samples) < delay * minimum_requests as u32);
    }
}

#[tokio::test(start_paused = true)]
async fn simultaneous_restores_share_host_io_admission() {
    let fixture = fixture(0).await;
    let (store, replica) = delayed_replica(&fixture, Duration::from_millis(20), 3);
    let opened = replica.open_root(&fixture.root).await.unwrap();
    let output = tempfile::TempDir::new().unwrap();
    let first = output.path().join("first.sqlite");
    let second = output.path().join("second.sqlite");
    store.reset();

    let (left, right) = tokio::join!(opened.restore(&first), opened.restore(&second));

    left.unwrap();
    right.unwrap();
    assert!(store.stats.peak.load(Ordering::SeqCst) > 1);
    assert!(store.stats.peak.load(Ordering::SeqCst) <= 3);
    assert!(store.stats.peak_range_bytes.load(Ordering::SeqCst) <= 3 << 20);
    assert_eq!(std::fs::read(first).unwrap(), fixture.expected);
    assert_eq!(std::fs::read(second).unwrap(), fixture.expected);
}

#[tokio::test]
async fn replica_telemetry_attributes_cold_sparse_and_restore_work() {
    let fixture = fixture(4).await;
    let store = InstrumentedStore::new(fixture.backend.clone(), Duration::ZERO);
    let telemetry = Arc::new(RecordingTelemetry::default());
    let host = Host::default().with_ltx_telemetry(telemetry.clone());
    let replica = cell_replica(Store::new(store), fixture.cell, fixture.incarnation, host);

    let opened = replica.open_root(&fixture.root).await.unwrap();
    opened.paged().read_page(1).await.unwrap();
    let output = tempfile::TempDir::new().unwrap();
    opened
        .restore(&output.path().join("telemetry.sqlite"))
        .await
        .unwrap();

    let phases = telemetry.phases.lock().unwrap();
    for expected in [
        LtxPhase::RootOpen,
        LtxPhase::Directory,
        LtxPhase::FrameFetch,
        LtxPhase::RestoreWrite,
    ] {
        assert!(phases.contains(&expected), "missing {expected:?}");
    }
    let reads = telemetry.reads.lock().unwrap();
    for expected in [LtxReadOrigin::Cold, LtxReadOrigin::Sparse] {
        assert!(
            reads
                .iter()
                .any(|(origin, outcome, bytes)| *origin == expected
                    && *outcome == LtxRequestOutcome::Succeeded
                    && *bytes > 0),
            "missing {expected:?}"
        );
    }
    let logical_reads = telemetry.logical_reads.lock().unwrap();
    assert!(logical_reads.contains(&LtxReadOrigin::Cold));
    assert!(logical_reads.contains(&LtxReadOrigin::Sparse));
}

#[tokio::test]
async fn failed_provider_attempt_is_not_counted_as_a_logical_retry() {
    let fixture = fixture(0).await;
    let store = InstrumentedStore::new(fixture.backend.clone(), Duration::ZERO);
    let telemetry = Arc::new(RecordingTelemetry::default());
    let host = Host::default().with_ltx_telemetry(telemetry.clone());
    let replica = cell_replica(
        Store::new(store.clone()),
        fixture.cell,
        fixture.incarnation,
        host,
    );
    let opened = replica.open_root(&fixture.root).await.unwrap();
    telemetry.logical_reads.lock().unwrap().clear();
    telemetry.reads.lock().unwrap().clear();
    store.arm(TIMEOUT_RANGE);

    assert!(opened.paged().read_page(1).await.is_err());

    assert_eq!(
        *telemetry.logical_reads.lock().unwrap(),
        vec![LtxReadOrigin::Sparse]
    );
    assert!(
        telemetry
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|(origin, outcome, bytes)| *origin == LtxReadOrigin::Sparse
                && *outcome == LtxRequestOutcome::Failed
                && *bytes == 0)
    );
}

#[tokio::test(start_paused = true)]
async fn failed_or_cancelled_parallel_restore_never_publishes_destination() {
    let fixture = fixture(0).await;
    let (store, replica) = delayed_replica(&fixture, Duration::from_millis(100), 8);
    let opened = replica.open_root(&fixture.root).await.unwrap();
    let output = tempfile::TempDir::new().unwrap();

    for (name, fault) in [
        ("short.sqlite", SHORT_RANGE),
        ("corrupt.sqlite", CORRUPT_RANGE),
        ("timeout.sqlite", TIMEOUT_RANGE),
    ] {
        let destination = output.path().join(name);
        store.arm(fault);
        assert!(opened.restore(&destination).await.is_err());
        assert!(!destination.exists());
    }

    store.arm(NO_FAULT);
    let destination = output.path().join("cancelled.sqlite");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), opened.restore(&destination))
            .await
            .is_err()
    );
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(output.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-restore-")
    }));
}
