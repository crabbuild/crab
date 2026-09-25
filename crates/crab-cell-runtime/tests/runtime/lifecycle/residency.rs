//! Resident routes, hydration, and owner progress.

use super::*;

/// Counts the metadata and origin reads one cold route performs.
#[derive(Default)]
struct ColdPathRecorder {
    catalog_reads: AtomicUsize,
    control_reads: AtomicUsize,
    origin_requests: AtomicUsize,
    ltx_phases: AtomicUsize,
    activation_phases: std::sync::Mutex<Vec<crab_cell_runtime::fleet::telemetry::ActivationPhase>>,
}

impl ColdPathRecorder {
    fn catalog_reads(&self) -> usize {
        self.catalog_reads.load(Ordering::Acquire)
    }

    fn control_reads(&self) -> usize {
        self.control_reads.load(Ordering::Acquire)
    }

    fn origin_requests(&self) -> usize {
        self.origin_requests.load(Ordering::Acquire)
    }

    fn ltx_phases(&self) -> usize {
        self.ltx_phases.load(Ordering::Acquire)
    }

    fn activation_phases(&self) -> Vec<crab_cell_runtime::fleet::telemetry::ActivationPhase> {
        self.activation_phases.lock().unwrap().clone()
    }
}

impl crab_cell_runtime::fleet::telemetry::CellTelemetry for ColdPathRecorder {
    fn catalog_read(
        &self,
        _kind: crab_cell_runtime::fleet::telemetry::CatalogReadKind,
        _elapsed: std::time::Duration,
        _succeeded: bool,
    ) {
        self.catalog_reads.fetch_add(1, Ordering::AcqRel);
    }

    fn control_read(&self, _elapsed: std::time::Duration, _succeeded: bool) {
        self.control_reads.fetch_add(1, Ordering::AcqRel);
    }

    fn ltx_phase(
        &self,
        _phase: crab_cell_runtime::ltx::LtxPhase,
        _elapsed: std::time::Duration,
        _succeeded: bool,
    ) {
        self.ltx_phases.fetch_add(1, Ordering::AcqRel);
    }

    fn ltx_origin_request(
        &self,
        _origin: crab_cell_runtime::ltx::LtxReadOrigin,
        _outcome: crab_cell_runtime::ltx::LtxRequestOutcome,
        _bytes: u64,
    ) {
        self.origin_requests.fetch_add(1, Ordering::AcqRel);
    }

    fn activation_phase(
        &self,
        phase: crab_cell_runtime::fleet::telemetry::ActivationPhase,
        _elapsed: std::time::Duration,
    ) {
        self.activation_phases.lock().unwrap().push(phase);
    }
}

#[tokio::test]
async fn cold_activation_reports_metadata_and_origin_reads() {
    let fixture = fixture_for(b"cold-path-counts");
    let session = SessionId::from_bytes([112; 16]);
    let first_runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&first_runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let recorder = Arc::new(ColdPathRecorder::default());
    let sink =
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(recorder.clone());
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::with_telemetry(
        fixture.layout.clone(),
        fixture.target.tenant(),
        sink.clone(),
    );
    let authority = CellAuthority::with_telemetry(fixture.layout.clone(), sink.clone());
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([113; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    runtime.install_telemetry(recorder.clone()).unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            cold_node_directory(&fixture).join("cold-path.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://cold-path.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    // The window above is one cold route: catalog locator, control record, and
    // the root open that fetches origin objects. A resident route issues none
    // of them, so these counts are the cold-start cost. Four of the seven
    // object-store requests are metadata; changing them changes what a cold
    // start costs, so a change here must be deliberate.
    assert_eq!(recorder.catalog_reads(), 2);
    // The caller's decision read plus the acquisition's own confirmation.
    assert_eq!(recorder.control_reads(), 2);
    assert_eq!(recorder.origin_requests(), 3);
    assert!(recorder.ltx_phases() >= 1);
    // The phases decompose the cold route: claim, verify the root, restore it
    // locally, then activate. A warm route would record none of them.
    assert_eq!(
        recorder.activation_phases(),
        [
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Ownership,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::RootOpen,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Restore,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Activate,
        ]
    );

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

/// Returns a node directory that holds no local image of the fixture's Cell.
///
/// A cold route is what a node without a readable resume record pays, so tests
/// that measure it must not reuse the directory the released database is in.
fn cold_node_directory(fixture: &Fixture) -> std::path::PathBuf {
    let directory = fixture._directory.path().join("cold-node");
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Boots one Cell, commits once, and releases it, leaving a resume record.
async fn released_cell(fixture: &Fixture, session: SessionId, start_ms: i64) -> i64 {
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, fixture, session).await;
    handle
        .execute(
            mutation_identity_window(161, start_ms, start_ms + 10_000),
            Digest::from_bytes([162; 32]),
            start_ms,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
    start_ms
}

/// Wakes a released Cell on the same node and returns what it read and ran.
async fn wake_released_cell(
    fixture: &Fixture,
    successor: SessionId,
    destination: std::path::PathBuf,
) -> (
    Arc<ColdPathRecorder>,
    crab_cell_runtime::cell::actor::CellHandle,
    CellRuntime,
) {
    let recorder = Arc::new(ColdPathRecorder::default());
    let sink =
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(recorder.clone());
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::with_telemetry(
        fixture.layout.clone(),
        fixture.target.tenant(),
        sink.clone(),
    );
    let authority = CellAuthority::with_telemetry(fixture.layout.clone(), sink.clone());
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    runtime.install_telemetry(recorder.clone()).unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            destination,
            Owner {
                session: successor,
                endpoint: "https://warm-wake.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    (recorder, restored, runtime)
}

#[tokio::test]
async fn a_warm_wake_continues_the_local_database_without_the_origin() {
    let fixture = fixture_for(b"warm-wake");
    let start_ms = now_ms();
    released_cell(&fixture, SessionId::from_bytes([163; 16]), start_ms).await;

    // The same node wakes the same root. The release record makes the local
    // image the fast path, so this activation reads no origin object at all
    // and never verifies or restores the immutable root graph.
    let successor = SessionId::from_bytes([164; 16]);
    let (recorder, restored, runtime) = wake_released_cell(
        &fixture,
        successor,
        fixture._directory.path().join("warm.sqlite"),
    )
    .await;
    assert_eq!(recorder.origin_requests(), 0);
    assert_eq!(
        recorder.activation_phases(),
        [
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Ownership,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Resume,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Activate,
        ]
    );

    // The resumed database continues the lineage rather than resetting it: the
    // commit from the released session is readable, and the next commit lands
    // on the same chain.
    let outcome = restored
        .execute(
            mutation_identity_window(165, start_ms + 20_000, start_ms + 30_000),
            Digest::from_bytes([166; 32]),
            start_ms + 20_000,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                let value: i64 =
                    transaction.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
                Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.result(), 2i64.to_be_bytes());

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_resume_record_that_names_another_root_is_discarded() {
    let fixture = fixture_for(b"stale-resume");
    let start_ms = now_ms();
    released_cell(&fixture, SessionId::from_bytes([167; 16]), start_ms).await;
    let record = fixture._directory.path().join("cell.sqlite.resume");
    let mut bytes = std::fs::read(&record).unwrap();
    // Field layout: magic, version, schema, code, cell, incarnation, then the
    // root digest the record has to match against the observed control.
    bytes[96] ^= 0xff;
    std::fs::write(&record, bytes).unwrap();

    // A record that no longer names the observed root must be discarded with
    // the database it names, and the wake must fall back to the exact restore.
    let successor = SessionId::from_bytes([168; 16]);
    let (recorder, restored, runtime) = wake_released_cell(
        &fixture,
        successor,
        fixture._directory.path().join("cold-again.sqlite"),
    )
    .await;
    assert!(recorder.origin_requests() > 0);
    assert_eq!(
        recorder.activation_phases(),
        [
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Ownership,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::RootOpen,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Restore,
            crab_cell_runtime::fleet::telemetry::ActivationPhase::Activate,
        ]
    );
    assert!(!fixture.database.exists());

    let outcome = restored
        .execute(
            mutation_identity_window(169, start_ms + 20_000, start_ms + 30_000),
            Digest::from_bytes([170; 32]),
            start_ms + 20_000,
            1_024,
            1_024,
            |transaction| {
                let value: i64 =
                    transaction.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
                Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.result(), 1i64.to_be_bytes());

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn clean_drain_publishes_and_consumes_one_due_hint() {
    let fixture = fixture_for(b"due-hint");
    let start_ms = now_ms();
    let session = SessionId::from_bytes([118; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle
        .execute(
            mutation_identity_window(119, start_ms, start_ms + 10_000),
            Digest::from_bytes([120; 32]),
            start_ms,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let due_ms = control.value().next_due_ms.unwrap();
    // Eviction releases ownership from a spawned task, so the hint lands just
    // after control turns Idle: wait for the key instead of racing it.
    let bucket = crab_cell_runtime::cell::due::bucket_for(due_ms).unwrap();
    let expected = fixture
        .layout
        .due_hint_path(bucket, fixture.target.cell_id().as_bytes());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if fixture
                .layout
                .store()
                .get_with_etag_bounded(&expected, 8)
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        crab_cell_runtime::cell::due::take(&fixture.layout, due_ms, 8)
            .await
            .unwrap(),
        vec![fixture.target.cell_id()]
    );
    // Consuming a hint deletes it; the backstop scan is what makes that safe.
    assert!(
        crab_cell_runtime::cell::due::take(&fixture.layout, due_ms, 8)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn idle_eviction_publishes_the_same_due_hint() {
    let fixture = fixture_for(b"due-hint-eviction");
    let start_ms = now_ms();
    let session = SessionId::from_bytes([122; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle
        .execute(
            mutation_identity_window(123, start_ms, start_ms + 10_000),
            Digest::from_bytes([124; 32]),
            start_ms,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    // Pressure eviction, not a clean drain: it must leave the same hint, or
    // the thirty-cycle backstop would carry every evicted deadline.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if runtime.evict_idle(1).await.unwrap() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let due_ms = control.value().next_due_ms.unwrap();
    // The release runs in a spawned task, so the hint lands just after control
    // turns Idle: wait for the key instead of racing it.
    let bucket = crab_cell_runtime::cell::due::bucket_for(due_ms).unwrap();
    let expected = fixture
        .layout
        .due_hint_path(bucket, fixture.target.cell_id().as_bytes());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if fixture
                .layout
                .store()
                .get_with_etag_bounded(&expected, 8)
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .layout
            .store()
            .get_with_etag_bounded(&expected, 8)
            .await
            .is_ok(),
        "a pressure eviction leaves a hint in the released head's bucket"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cleanly_closed_database_survives_a_rename_to_a_fresh_path() {
    let fixture = fixture_for(b"spike-rename");
    let session = SessionId::from_bytes([131; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    // A clean session's file, renamed to a path nothing has ever opened: the
    // capture-directory fence is per path, so this is the candidate mechanism
    // for a warm wake that reuses local bytes.
    let old = fixture.database.clone();
    let fresh = fixture._directory.path().join("fresh-name.sqlite");
    std::fs::rename(&old, &fresh).unwrap();
    let mut db = crab_ltx::Db::open_with_host(
        &fresh,
        crab_ltx::Limits::default(),
        crab_ltx::Host::default(),
    )
    .unwrap();
    let rows = db
        .query_with(|connection| {
            connection.query_row("SELECT count(*) FROM counter", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(rows, 1, "the renamed database still holds its rows");
    db.close().unwrap();
}

#[tokio::test]
async fn due_hint_publication_skips_a_deadline_outside_the_window() {
    let fixture = fixture_for(b"due-hint-window");
    let bucket_ms = crab_cell_runtime::cell::due::HINT_BUCKET_MS;
    let now_ms = bucket_ms * 100;
    // Older than the listing window: no listing would see this key, so writing
    // it would leave an object behind that nothing ever consumes.
    let stale_ms = now_ms - bucket_ms * 10;
    crab_cell_runtime::cell::due::publish(
        &fixture.layout,
        fixture.target.cell_id(),
        stale_ms,
        now_ms,
    )
    .await
    .unwrap();
    let stale_group = crab_cell_runtime::cell::due::bucket_for(stale_ms).unwrap();
    assert!(
        fixture
            .layout
            .store()
            .get_with_etag_bounded(
                &fixture
                    .layout
                    .due_hint_path(stale_group, fixture.target.cell_id().as_bytes()),
                8
            )
            .await
            .is_err(),
        "a deadline outside the window publishes no key"
    );

    // Inside the window the same deadline is published for the next listing.
    let inside_ms = now_ms - bucket_ms;
    crab_cell_runtime::cell::due::publish(
        &fixture.layout,
        fixture.target.cell_id(),
        inside_ms,
        now_ms,
    )
    .await
    .unwrap();
    let inside_group = crab_cell_runtime::cell::due::bucket_for(inside_ms).unwrap();
    assert!(
        fixture
            .layout
            .store()
            .get_with_etag_bounded(
                &fixture
                    .layout
                    .due_hint_path(inside_group, fixture.target.cell_id().as_bytes()),
                8
            )
            .await
            .is_ok(),
        "a deadline inside the window publishes its key"
    );
}

#[tokio::test]
async fn due_hint_listing_clears_foreign_keys() {
    let fixture = fixture_for(b"due-hint-junk");
    let bucket = crab_cell_runtime::cell::due::bucket_for(1_000).unwrap();
    let foreign = object_store::path::Path::from(format!(
        "{}/not-a-cell.json",
        fixture.layout.due_hint_prefix(bucket)
    ));
    fixture
        .layout
        .store()
        .put(&foreign, bytes::Bytes::new())
        .await
        .unwrap();
    assert!(
        fixture
            .layout
            .store()
            .get_with_etag_bounded(&foreign, 1_024)
            .await
            .is_ok(),
        "the foreign key must exist before the listing"
    );
    assert!(
        crab_cell_runtime::cell::due::take(&fixture.layout, 1_000, 8)
            .await
            .unwrap()
            .is_empty(),
        "a foreign key is not a candidate"
    );
    assert!(
        fixture
            .layout
            .store()
            .get_with_etag_bounded(&foreign, 1_024)
            .await
            .is_err(),
        "a foreign key must not be listed on every cycle"
    );
}

#[tokio::test]
async fn resident_due_list_mirrors_the_published_head() {
    let fixture = fixture_for(b"resident-due-list");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;
    // A bootstrap that arms no deadline publishes no due time.
    assert!(runtime.due_resident(i64::MAX, 4).await.unwrap().is_empty());
    let now_ms = 1_000;
    // An accepted command leaves a retention row, so the Cell publishes an
    // earliest due class. The due list must answer from that mirror without
    // reading the catalog entry or the control record.
    let committed = handle
        .execute(
            mutation_identity_window(97, 1_000, 10_000),
            Digest::from_bytes([98; 32]),
            now_ms,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    assert!(runtime.due_resident(now_ms, 4).await.unwrap().is_empty());
    let due = runtime.due_resident(i64::MAX, 4).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].handle().cell_id(), handle.cell_id());
    assert_eq!(
        due[0].expected_commit_sequence(),
        committed.commit_sequence()
    );
    assert!(due[0].next_due_ms() > now_ms);
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_stats_follow_active_cell_lifecycle() {
    let fixture = fixture_for(b"runtime-stats");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert_eq!(runtime.stats().active_cells(), 1);
    assert_eq!(
        runtime.stats().resident_bytes(),
        crab_cell_runtime::cell::actor::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().resident_capacity_bytes(),
        10 * crab_cell_runtime::cell::actor::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().file_descriptors(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(
        runtime.stats().file_descriptor_capacity(),
        10 * ACTIVE_CELL_FILE_DESCRIPTORS
    );
    handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    assert_eq!(runtime.stats().resident_bytes(), 0);
    assert_eq!(runtime.stats().file_descriptors(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resident_lookup_is_invalidated_before_drain_releases_the_cell() {
    let fixture = fixture_for(b"resident-drain-race");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_some()
    );

    handle.drain().await.unwrap();

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_none()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resident_route_reports_zero_origin_reads_and_latency_percentiles() {
    let origin_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&origin_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_kind| {
            observed_reads.fetch_add(1, Ordering::AcqRel);
        }));
    let fixture =
        fixture_with_limits_and_store(b"resident-warm-qualification", Limits::default(), store);
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;
    origin_reads.store(0, Ordering::Release);

    let mut samples = Vec::with_capacity(64);
    for _ in 0..64 {
        let started = std::time::Instant::now();
        let resident = runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .expect("bootstrapped Cell must remain resident");
        let value = resident
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap();
        assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 0);
        samples.push(started.elapsed());
    }

    samples.sort_unstable();
    let percentile = |percent: usize| {
        let index = ((samples.len() - 1) * percent).div_ceil(100);
        samples[index]
    };
    println!(
        "resident warm route: samples={} p50_us={} p95_us={} p99_us={} max_us={}",
        samples.len(),
        percentile(50).as_micros(),
        percentile(95).as_micros(),
        percentile(99).as_micros(),
        samples.last().unwrap().as_micros()
    );
    assert_eq!(origin_reads.load(Ordering::Acquire), 0);

    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn restored_sparse_route_promotes_before_zero_origin_reads() {
    let origin_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&origin_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_kind| {
            observed_reads.fetch_add(1, Ordering::AcqRel);
        }));
    let fixture = fixture_with_limits_and_store(
        b"resident-warm-restart-qualification",
        Limits::default(),
        store,
    );
    let session = SessionId::from_bytes([110; 16]);
    let first_runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&first_runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([111; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("warm-restart.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://warm-restart.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime
                .resident_handle(&fixture.target, CatalogRole::Repository)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    origin_reads.store(0, Ordering::Release);

    let resident = runtime
        .resident_handle(&fixture.target, CatalogRole::Repository)
        .await
        .unwrap()
        .expect("verified sparse restore must promote to resident");
    let value = resident
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 0);
    assert_eq!(origin_reads.load(Ordering::Acquire), 0);

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_a_hydration_reservation_after_an_origin_wait() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let fixture = fixture_with_limits_and_store(
        b"hydration-shutdown-cancellation",
        Limits::default(),
        Store::new(pausing.clone()),
    );
    let first_session = SessionId::from_bytes([113; 16]);
    let first_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        64 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = bootstrap_role_on(
        &first_runtime,
        &fixture,
        first_session,
        CatalogRole::Repository,
        |transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 WITH RECURSIVE numbers(value) AS (\
                   SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 512\
                 )\
                 INSERT INTO payload(value) SELECT zeroblob(16384) FROM numbers;",
            )?;
            Ok(())
        },
    )
    .await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([114; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        64 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            observed,
            cold_node_directory(&fixture).join("hydration-shutdown.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://hydration-shutdown.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    pausing.arm_gets();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_get_blocked(),
    )
    .await
    .unwrap();
    assert_eq!(runtime.stats().hydration_jobs(), 1);

    let shutdown_runtime = runtime.clone();
    let shutdown = tokio::spawn(async move { shutdown_runtime.shutdown().await });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    pausing.release_gets();
    shutdown.await.unwrap().unwrap();
    assert_eq!(runtime.stats().hydration_jobs(), 0);
    drop(restored);
}

#[tokio::test]
async fn retained_request_outcome_moves_with_exact_root() {
    let fixture = fixture_for(b"eviction-persisted-work");
    let session = SessionId::from_bytes([78; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(79, 10, 10_000),
                Digest::from_bytes([79; 32]),
                20,
                64,
                64,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));

    let generation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some((_, generation, _, _)) =
                runtime.idle_transfer_candidates().await.unwrap().first()
            {
                break *generation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime
        .release_idle_cell(fixture.target.cell_id(), session, generation)
        .await
        .unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor_session = SessionId::from_bytes([95; 16]);
    let successor_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        successor_session,
    )
    .unwrap();
    let successor = successor_runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("outcome-successor.sqlite"),
            Owner {
                session: successor_session,
                endpoint: "https://outcome-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        successor
            .resolve(
                mutation_identity_window(79, 10, 10_000),
                Digest::from_bytes([79; 32]),
                20,
                64
            )
            .await
            .unwrap(),
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence: 1,
            ..
        })
    ));
    successor.drain().await.unwrap();
    successor_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_owner_progress_is_renewed_without_a_per_cell_task() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let initial = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let initial_progress = initial.value().progress;
    let initial_root = initial.value().root.clone();

    let renewed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().progress > initial_progress {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(renewed.value().root, initial_root);
    assert_eq!(renewed.value().owner, initial.value().owner);
    assert_eq!(renewed.value().revision, initial.value().revision + 1);
    handle.drain().await.unwrap();
}
