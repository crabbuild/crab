use std::{
    env,
    path::Path as FilePath,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::{
    ActivityCompletion, ActivityCompletionOutcome, ActivityRunOutcome, ActivitySupervisor,
    ApplicationId, BlobArtifactStore, BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery,
    BlobQueryResult, BoundedEncoder, CellAuthority, CellCatalog, CellClient, CellReplica,
    CellStorageLayout, CellTarget, CronMutation, CronMutationOutcome, CronQueryResult,
    EffectAckRequest, EffectClaimRequest, EffectLease, EffectLeaseCommand, EffectLeaseOutcome,
    EffectLeaseRequest, EffectState, EffectStatus, IncarnationId, InvocationError, KvAtomicOutcome,
    KvAtomicRequest, KvMutation, MaintenanceTickCommand, MaintenanceTickOutcome,
    MaintenanceTickRequest, MutationIdentity, NodeLeaseGuard, Owner, QueueClaimRequest,
    QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest, QueueState, RecoveryManifestStore,
    ReplicaLimits, Resolution, SessionId, SqlBatch, SqlResultSet, SqlStatement, SqlValue,
    SqlWorkerPool, StoredOutcome, TenantId, WireValue, WorkflowActivityClaimCommand,
    WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand, WorkflowOutcome, WorkflowStatus,
    partition_for_shard,
};
use crab_storage::Store;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[path = "../../crab-cell-runtime/src/process_store.rs"]
mod process_store;

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification_process_cron.rs"]
mod process_cron;
#[path = "support/qualification_process_duplicate.rs"]
mod process_duplicate;
#[path = "support/qualification_process_effect.rs"]
mod process_effect;
#[path = "support/qualification_scheduled_expiry.rs"]
#[expect(
    dead_code,
    reason = "the process fault test uses the scheduled SQL expiry case"
)]
mod process_expiry;
#[path = "support/qualification_process_observer.rs"]
mod process_observer;
#[path = "support/qualification_process_owner.rs"]
mod process_owner;
#[path = "support/qualification_process_restore.rs"]
mod process_restore;
#[path = "support/qualification_process_successor.rs"]
mod process_successor;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_cancellation.rs"]
#[expect(
    dead_code,
    reason = "the process expiry case uses the shared clock helper"
)]
mod qualification_cancellation;
#[path = "support/qualification_fence.rs"]
mod qualification_fence;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(
    dead_code,
    reason = "this test uses only the signed Effect delivery helper"
)]
mod qualification_peer;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fence::fence_public_session;
use qualification_fixture::public_host_fixture_with_store;

const ROLE_ENV: &str = "CRAB_CELL_PROCESS_FAULT_ROLE";
const CASE_ENV: &str = "CRAB_CELL_PROCESS_FAULT_CASE";
const ROOT_ENV: &str = "CRAB_CELL_PROCESS_FAULT_ROOT";
const SYNC_ENV: &str = "CRAB_CELL_PROCESS_FAULT_SYNC";
const STORE_BACKEND_ENV: &str = "CRAB_CELL_PROCESS_FAULT_STORE_BACKEND";
const STORE_PATH_ENV: &str = "CRAB_CELL_PROCESS_FAULT_STORE_PATH";
const FILESYSTEM_STORE_BACKEND: &str = "filesystem";
const BEFORE_WRITE: &str = "before-write";
const AFTER_ACK: &str = "after-ack";
const AFTER_LEASE: &str = "after-lease";
const AFTER_SETTLEMENT: &str = "after-settlement";
const AFTER_SQL_EXPIRY: &str = "after-sql-expiry";
const SQL_PAYLOAD: &[u8] = b"acknowledged-before-owner-kill";
const SQL_EXPIRY_OPERATION_ID: u64 = 900_150;
const SQL_EXPIRY_NONCE: u64 = 900_152;
const KV_SCOPE: &[u8] = b"process-fault";
const KV_KEY: &[u8] = b"acknowledged";
const KV_PAYLOAD: &[u8] = b"published-kv-before-owner-kill";
const BLOB_KEY: &[u8] = b"acknowledged-blob";
const BLOB_PAYLOAD: &[u8] = b"published-blob-before-owner-kill";
const QUEUE_PAYLOAD: &[u8] = b"published-queue-before-owner-kill";
const CRON_PAYLOAD: &[u8] = b"published-cron-before-owner-kill";
const WORKFLOW_ID: &[u8] = b"published-workflow-before-owner-kill";
const WORKFLOW_RESULT: &[u8] = b"workflow-result-before-owner-kill";
const ACTIVITY_WORKFLOW_ID: &[u8] = b"published-activity-before-owner-kill";
const EFFECT_WORKFLOW_ID: &[u8] = b"published-effect-before-owner-kill";
const EFFECT_RESULT: &[u8] = b"delivered-effect";

#[derive(Deserialize, Serialize)]
struct Acknowledgement {
    replay_issued_at_ms: i64,
    queue_available_at_ms: i64,
    sql_sequence: u64,
    expiry_sql_sequence: Option<u64>,
    kv_sequence: u64,
    kv_version: Vec<u8>,
    blob_sequence: u64,
    blob_etag: [u8; 32],
    blob_size: u64,
    queue_sequence: u64,
    queue_message_id: [u8; 16],
    cron_sequence: u64,
    cron_generation: u64,
    cron_next_due_ms: i64,
    workflow_sequence: u64,
    workflow_run_id: [u8; 16],
    workflow_event_sequence: u64,
    activity_sequence: u64,
    activity_run_id: [u8; 16],
    effect_sequence: u64,
    effect_run_id: [u8; 16],
    leases: Option<LeaseEvidence>,
    settlements: Option<SettlementEvidence>,
}

#[derive(Deserialize, Serialize)]
struct LeaseEvidence {
    queue_attempt: u32,
    queue_token: [u8; 16],
    queue_until_ms: i64,
    queue_sequence: u64,
    activity_id: [u8; 16],
    activity_attempt: u32,
    activity_token: [u8; 16],
    activity_until_ms: i64,
    activity_sequence: u64,
    effect_id: [u8; 32],
    effect_attempt: u32,
    effect_token: [u8; 16],
    effect_until_ms: i64,
    effect_expires_at_ms: i64,
    effect_sequence: u64,
}

#[derive(Deserialize, Serialize)]
struct SettlementEvidence {
    queue_token: [u8; 16],
    queue_ack_sequence: u64,
    activity_id: [u8; 16],
    activity_attempt: u32,
    activity_token: [u8; 16],
    activity_completion_token: [u8; 16],
    activity_input: Vec<u8>,
    activity_complete_sequence: u64,
    activity_event_sequence: u64,
    activity_result: Vec<u8>,
    effect_id: [u8; 32],
    effect_attempt: u32,
    effect_token: [u8; 16],
    effect_expires_at_ms: i64,
    effect_result: Vec<u8>,
    effect_destination_sequence: u64,
    effect_ack_sequence: u64,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn process_input() -> (Path, std::path::PathBuf) {
    let root = Path::from(env::var(ROOT_ENV).expect("fault root"));
    let sync = env::var(SYNC_ENV).expect("fault sync directory").into();
    (root, sync)
}

fn process_case() -> String {
    let case = env::var(CASE_ENV).expect("fault case");
    assert!(
        case == BEFORE_WRITE
            || case == AFTER_ACK
            || case == AFTER_LEASE
            || case == AFTER_SETTLEMENT
            || case == AFTER_SQL_EXPIRY,
        "unknown fault case"
    );
    case
}

fn process_store() -> (Store, Path) {
    if env::var(STORE_BACKEND_ENV).ok().as_deref() == Some(FILESYSTEM_STORE_BACKEND) {
        let path = env::var(STORE_PATH_ENV).expect("filesystem fault store path");
        let store = process_store::FilesystemCasStore::new(FilePath::new(&path))
            .expect("filesystem fault store");
        return (
            Store::new(Arc::new(store)),
            Path::from("public-process-fault"),
        );
    }
    rustfs_public_store()
}

fn publish_marker(sync: &FilePath, name: &str, bytes: &[u8]) {
    let temporary = sync.join(format!("{name}.tmp"));
    std::fs::write(&temporary, bytes).expect("fault marker");
    std::fs::rename(temporary, sync.join(name)).expect("publish fault marker");
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("fault clock")
            .as_millis(),
    )
    .expect("fault clock bounds")
}

fn replay_identity(index: u64, issued_at_ms: i64) -> MutationIdentity {
    let mut identity = identity(index, issued_at_ms);
    identity.expires_at_ms = issued_at_ms + 300_000;
    identity
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_role() {
    match env::var(ROLE_ENV).ok().as_deref() {
        Some("owner") => process_owner::run().await,
        Some("successor") => process_successor::run().await,
        Some("observer") => process_observer::run().await,
        None => {}
        Some(role) => panic!("unknown fault process role: {role}"),
    }
}

fn spawn_role(
    binary: &FilePath,
    role: &str,
    case: &str,
    root: &Path,
    sync: &FilePath,
    backend: &str,
    store_path: Option<&FilePath>,
) -> ChildGuard {
    let mut command = Command::new(binary);
    command
        .args(["--exact", "process_role", "--nocapture"])
        .env(ROLE_ENV, role)
        .env(CASE_ENV, case)
        .env(ROOT_ENV, root.to_string())
        .env(SYNC_ENV, sync)
        .env(STORE_BACKEND_ENV, backend);
    if let Some(store_path) = store_path {
        command.env(STORE_PATH_ENV, store_path);
    }
    let child = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("fault process spawn");
    ChildGuard(child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_recovers_eight_acknowledged_primitives_in_successor_process() {
    let mut expected = Vec::from(SQL_PAYLOAD);
    expected.extend_from_slice(KV_PAYLOAD);
    expected.extend_from_slice(BLOB_PAYLOAD);
    expected.extend_from_slice(QUEUE_PAYLOAD);
    expected.extend_from_slice(CRON_PAYLOAD);
    expected.extend_from_slice(WORKFLOW_RESULT);
    expected.extend_from_slice(b"activity-result");
    expected.extend_from_slice(EFFECT_RESULT);
    expected.extend_from_slice(CRON_PAYLOAD);
    run_process_fault(AFTER_ACK, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_before_eight_primitive_writes_has_no_ghost_state() {
    run_process_fault(BEFORE_WRITE, "ready", b"absent").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_after_three_leases_reclaims_exact_attempts() {
    let mut expected = Vec::from(SQL_PAYLOAD);
    expected.extend_from_slice(KV_PAYLOAD);
    expected.extend_from_slice(BLOB_PAYLOAD);
    expected.extend_from_slice(QUEUE_PAYLOAD);
    expected.extend_from_slice(CRON_PAYLOAD);
    expected.extend_from_slice(WORKFLOW_RESULT);
    expected.extend_from_slice(b"activity-result");
    expected.extend_from_slice(EFFECT_RESULT);
    expected.extend_from_slice(CRON_PAYLOAD);
    run_process_fault(AFTER_LEASE, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_preserves_three_acknowledged_settlements() {
    let mut expected = Vec::from(SQL_PAYLOAD);
    expected.extend_from_slice(KV_PAYLOAD);
    expected.extend_from_slice(BLOB_PAYLOAD);
    expected.extend_from_slice(QUEUE_PAYLOAD);
    expected.extend_from_slice(CRON_PAYLOAD);
    expected.extend_from_slice(WORKFLOW_RESULT);
    expected.extend_from_slice(b"activity-result");
    expected.extend_from_slice(EFFECT_RESULT);
    expected.extend_from_slice(CRON_PAYLOAD);
    run_process_fault(AFTER_SETTLEMENT, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_preserves_rejected_sql_expiry_and_acknowledged_row() {
    let mut expected = acknowledged_payload(true);
    expected.extend_from_slice(&SQL_EXPIRY_NONCE.to_be_bytes());
    run_process_fault(AFTER_SQL_EXPIRY, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filesystem_owner_kill_preserves_rejected_sql_expiry_and_acknowledged_row() {
    let mut expected = acknowledged_payload(true);
    expected.extend_from_slice(&SQL_EXPIRY_NONCE.to_be_bytes());
    run_filesystem_process_fault(AFTER_SQL_EXPIRY, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filesystem_owner_kill_preserves_three_acknowledged_settlements() {
    run_filesystem_process_fault(AFTER_SETTLEMENT, "ack", &acknowledged_payload(true)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filesystem_owner_kill_after_three_leases_reclaims_exact_attempts() {
    run_filesystem_process_fault(AFTER_LEASE, "ack", &acknowledged_payload(true)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filesystem_owner_kill_before_eight_primitive_writes_has_no_ghost_state() {
    run_filesystem_process_fault(BEFORE_WRITE, "ready", b"absent").await;
}

fn acknowledged_payload(include_settlement_marker: bool) -> Vec<u8> {
    let mut expected = Vec::from(SQL_PAYLOAD);
    expected.extend_from_slice(KV_PAYLOAD);
    expected.extend_from_slice(BLOB_PAYLOAD);
    expected.extend_from_slice(QUEUE_PAYLOAD);
    expected.extend_from_slice(CRON_PAYLOAD);
    expected.extend_from_slice(WORKFLOW_RESULT);
    expected.extend_from_slice(b"activity-result");
    expected.extend_from_slice(EFFECT_RESULT);
    if include_settlement_marker {
        expected.extend_from_slice(CRON_PAYLOAD);
    }
    expected
}

async fn run_process_fault(case: &str, barrier: &str, expected: &[u8]) {
    run_process_fault_with_backend(case, barrier, expected, "rustfs", None).await;
}

async fn run_filesystem_process_fault(case: &str, barrier: &str, expected: &[u8]) {
    let store = tempfile::tempdir().expect("filesystem fault store directory");
    run_process_fault_with_backend(
        case,
        barrier,
        expected,
        FILESYSTEM_STORE_BACKEND,
        Some(store.path()),
    )
    .await;
}

async fn run_process_fault_with_backend(
    case: &str,
    barrier: &str,
    expected: &[u8],
    backend: &str,
    store_path: Option<&FilePath>,
) {
    let root = if backend == FILESYSTEM_STORE_BACKEND {
        Path::from("public-process-fault")
    } else {
        let (_, root) = rustfs_public_store();
        root
    };
    let sync = tempfile::tempdir().expect("fault synchronization directory");
    let binary = env::current_exe().expect("test binary");
    let mut owner = spawn_role(
        &binary,
        "owner",
        case,
        &root,
        sync.path(),
        backend,
        store_path,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !sync.path().join(barrier).exists() {
        assert!(
            owner.0.try_wait().expect("owner status").is_none(),
            "owner exited before fault boundary"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner fault boundary timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    owner.0.kill().expect("kill owner at fault boundary");
    assert!(!owner.0.wait().expect("owner exit").success());
    let mut successor = spawn_role(
        &binary,
        "successor",
        case,
        &root,
        sync.path(),
        backend,
        store_path,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = successor.0.try_wait().expect("successor status") {
            assert!(
                status.success(),
                "successor failed after owner kill: {status}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "successor recovery timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read(sync.path().join("observation")).expect("successor observation marker"),
        expected
    );
    let mut observer = spawn_role(
        &binary,
        "observer",
        case,
        &root,
        sync.path(),
        backend,
        store_path,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = observer.0.try_wait().expect("observer status") {
            assert!(status.success(), "independent observer failed: {status}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "independent observation timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read(sync.path().join("independent-observation"))
            .expect("independent observation marker"),
        expected
    );
}
