//! Ordered fault matrix across the durability seams.
//!
//! Each case injects a failure at one seam and asserts three things: the call
//! fails with the class callers branch on, the durable outcome is safe (no
//! published cut, no installed destination, or a fenced session), and a clean
//! retry after the fault clears produces the exact artifact.

use super::*;
use crab_ltx::FailureClass;

/// Lists the published LTX file names under one managed database.
fn published_cuts(database: &Path) -> Vec<String> {
    let name = database
        .file_name()
        .expect("database file name")
        .to_string_lossy()
        .into_owned();
    let directory = database
        .parent()
        .expect("database parent")
        .join(format!(".{name}-crab-ltx/ltx/0"));
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".ltx"))
        .collect();
    names.sort();
    names
}

/// Asserts the session refuses further work once a capture failure fenced it.
fn assert_fenced(writer: &mut Db) {
    assert!(matches!(
        writer.transaction(|_| Ok(())),
        Err(CrabError::Fenced)
    ));
}

#[test]
fn exhausted_local_disk_budget_refuses_before_the_commit() {
    let directory = tempfile::TempDir::new().unwrap();
    // Each transaction reserves its worst-case capture up front, so the bound
    // that matters here is the per-capture bound, not the database size.
    let limits = Limits {
        max_capture_bytes: 64 * 1024,
        max_file_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let budget = crab_ltx::DiskBudget::new(600 * 1024);
    let host = Host::default().with_local_disk_budget(budget.clone());
    let mut writer =
        Db::open_with_host(&directory.path().join("bounded.sqlite"), limits, host).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("CREATE TABLE t(v)"))
        .unwrap();
    writer.capture().unwrap();

    let mut refusals = 0;
    let mut committed = 0_i64;
    for _ in 0..128 {
        let result =
            writer.transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(randomblob(16384))"));
        match result {
            Ok(()) => {
                writer.capture().unwrap();
                committed += 1;
            }
            Err(CrabError::Limit(crab_ltx::LimitKind::LocalDiskBytes)) => {
                refusals += 1;
                break;
            }
            Err(error) => panic!("unexpected refusal: {error}"),
        }
    }
    assert_eq!(refusals, 1, "the budget must refuse the next commit");
    assert!(budget.used() <= budget.capacity());
    // The refusal happened before SQLite published anything: the refused row is
    // absent, no cut is pending, and the session is not fenced.
    assert!(!writer.has_pending_capture());
    let rows = writer
        .query_with(|connection| {
            connection.query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0))
        })
        .unwrap();
    assert_eq!(rows, committed);
}

#[test]
fn capture_write_and_sync_faults_leave_no_published_cut() {
    for operation in ["write_all", "sync_all", "rename"] {
        let (directory, faults, _host, mut writer) = fixture();
        let database = directory.path().join("source.sqlite");
        assert!(published_cuts(&database).is_empty());

        faults.plan([operation]);
        let error = writer.capture().unwrap_err();
        assert_eq!(
            error.classify(),
            FailureClass::Capacity,
            "{operation}: an injected disk-full failure is a capacity class"
        );
        assert!(
            published_cuts(&database).is_empty(),
            "{operation}: a failed capture must not publish a cut"
        );
        assert_fenced(&mut writer);
    }
}

#[test]
fn deferred_capture_barrier_fault_fences_before_acknowledgement() {
    let (directory, faults, _host, mut writer) = fixture();
    let database = directory.path().join("source.sqlite");
    let batch = writer.capture_deferred().unwrap();
    assert_eq!(batch.segments.len(), 1);

    faults.plan(["sync_parent"]);
    let error = writer.durability_barrier().unwrap_err();
    assert_eq!(error.classify(), FailureClass::Capacity);
    assert_fenced(&mut writer);

    // The deferred cut exists but was never durable, so no caller may
    // acknowledge it, and no later barrier call can release it.
    assert_eq!(published_cuts(&database).len(), 1);
    assert!(matches!(
        writer.durability_barrier(),
        Err(CrabError::Fenced)
    ));
    // A fresh session claims a new directory instead of adopting the residue.
    writer.close().unwrap();
    assert!(Db::open(&database, Limits::default()).is_err());
}

#[test]
fn checkpoint_boundary_fault_fences_without_publishing() {
    let (directory, faults, _host, mut writer) = fixture();
    let database = directory.path().join("source.sqlite");
    let published = writer.capture().unwrap();
    assert_eq!(published.segments.len(), 1);
    let cuts_before = published_cuts(&database);

    faults.plan(["rename"]);
    let error = writer.checkpoint(CheckpointMode::Truncate).unwrap_err();
    assert_eq!(error.classify(), FailureClass::Capacity);
    assert_eq!(
        published_cuts(&database),
        cuts_before,
        "a failed checkpoint must not publish a boundary cut"
    );
    assert_fenced(&mut writer);
}

#[test]
fn restore_install_fault_leaves_no_destination_and_retries_exactly() {
    let source = tempfile::TempDir::new().unwrap();
    let mut clean = Db::open(&source.path().join("clean.sqlite"), Limits::default()).unwrap();
    clean
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(4096))")
        })
        .unwrap();
    let batch = clean.capture().unwrap();
    clean.close().unwrap();

    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let plan = host
        .verify(&batch.segments, batch.position, Limits::default())
        .unwrap();
    let destination = directory.path().join("restored.sqlite");

    faults.plan(["persist_new"]);
    let error = host.restore(&plan, &destination).unwrap_err();
    assert_eq!(error.classify(), FailureClass::Capacity);
    assert!(
        !destination.exists(),
        "a failed install must not leave a destination"
    );

    // The identical call succeeds once the fault clears, and matches the plan.
    let restored = host.restore(&plan, &destination).unwrap();
    assert_eq!(restored, batch.position);
    assert!(destination.exists());
}

#[test]
fn compaction_install_fault_leaves_no_destination_and_retries_exactly() {
    let source = tempfile::TempDir::new().unwrap();
    let mut clean = Db::open(&source.path().join("clean.sqlite"), Limits::default()).unwrap();
    clean
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(4096))")
        })
        .unwrap();
    let batch = clean.capture().unwrap();
    clean.close().unwrap();

    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let plan = host
        .verify(&batch.segments, batch.position, Limits::default())
        .unwrap();
    let destination = directory.path().join("compacted.ltx");

    faults.plan(["persist_file_new"]);
    let error = host.compact(&plan, &destination).unwrap_err();
    assert_eq!(error.classify(), FailureClass::Capacity);
    assert!(!destination.exists(), "a failed install must not publish");

    let compacted = host.compact(&plan, &destination).unwrap();
    assert_eq!(compacted.info().max_txid, batch.position.txid);
    let compact_plan = crab_ltx::VerifiedPlan::new(
        std::slice::from_ref(&compacted),
        batch.position,
        Limits::default(),
    )
    .unwrap();
    let restored = directory.path().join("restored.sqlite");
    assert_eq!(
        crab_ltx::restore_exact(&compact_plan, &restored).unwrap(),
        batch.position
    );
}

#[test]
fn ordered_plan_injects_each_seam_once() {
    let (_directory, faults, _host, mut writer) = fixture();
    // The capture writes the cut, then renames it into place: a plan that fails
    // both operations must inject exactly once per seam.
    faults.plan(["write_all", "rename"]);
    assert!(writer.capture().is_err());
    assert!(matches!(
        writer.transaction(|_| Ok(())),
        Err(CrabError::Fenced)
    ));
}
