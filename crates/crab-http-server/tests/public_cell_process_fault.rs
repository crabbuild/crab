use std::{
    env,
    path::Path as FilePath,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::{
    ApplicationId, BlobArtifactStore, BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery,
    BlobQueryResult, CellAuthority, CellCatalog, CellClient, CellReplica, CellStorageLayout,
    CellTarget, IncarnationId, KvAtomicOutcome, KvAtomicRequest, KvMutation, NodeLeaseGuard, Owner,
    QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest, QueueState,
    RecoveryManifestStore, ReplicaLimits, SessionId, SqlBatch, SqlStatement, SqlValue,
    SqlWorkerPool, TenantId, partition_for_shard,
};
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fence.rs"]
mod qualification_fence;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fence::fence_public_session;
use qualification_fixture::public_host_fixture_with_store;

const ROLE_ENV: &str = "CRAB_CELL_PROCESS_FAULT_ROLE";
const CASE_ENV: &str = "CRAB_CELL_PROCESS_FAULT_CASE";
const ROOT_ENV: &str = "CRAB_CELL_PROCESS_FAULT_ROOT";
const SYNC_ENV: &str = "CRAB_CELL_PROCESS_FAULT_SYNC";
const BEFORE_WRITE: &str = "before-write";
const AFTER_ACK: &str = "after-ack";
const SQL_PAYLOAD: &[u8] = b"acknowledged-before-owner-kill";
const KV_SCOPE: &[u8] = b"process-fault";
const KV_KEY: &[u8] = b"acknowledged";
const KV_PAYLOAD: &[u8] = b"published-kv-before-owner-kill";
const BLOB_KEY: &[u8] = b"acknowledged-blob";
const BLOB_PAYLOAD: &[u8] = b"published-blob-before-owner-kill";
const QUEUE_PAYLOAD: &[u8] = b"published-queue-before-owner-kill";

#[derive(Deserialize, Serialize)]
struct Acknowledgement {
    sql_sequence: u64,
    kv_sequence: u64,
    kv_version: Vec<u8>,
    blob_sequence: u64,
    blob_etag: [u8; 32],
    blob_size: u64,
    queue_sequence: u64,
    queue_message_id: [u8; 16],
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
        case == BEFORE_WRITE || case == AFTER_ACK,
        "unknown fault case"
    );
    case
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_role() {
    match env::var(ROLE_ENV).ok().as_deref() {
        Some("owner") => owner_process().await,
        Some("successor") => successor_process().await,
        None => {}
        Some(role) => panic!("unknown fault process role: {role}"),
    }
}

async fn owner_process() {
    let (root, sync) = process_input();
    let case = process_case();
    let (store, _) = rustfs_public_store();
    let (node, typed, tenant, application, _directory, _registry, _handles, _store) =
        public_host_fixture_with_store(store.clone(), root.clone()).await;
    if case == BEFORE_WRITE {
        publish_marker(&sync, "ready", b"owner-bootstrapped");
        let _node = node;
        std::future::pending::<()>().await;
        return;
    }
    let target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("source SQL target");
    let sql = typed
        .sql::<fixture::ReferenceSql>(target.clone())
        .expect("source SQL");
    let committed = sql
        .batch(
            identity(900_100, now_ms()),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (1, ?1)".into(),
                    parameters: vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())],
                }],
            },
        )
        .await
        .expect("acknowledged SQL write");
    assert_eq!(committed.output[0].rows_affected, 1);
    let layout = CellStorageLayout::new(store, root, *application.as_bytes());
    let authority = CellAuthority::new(layout);
    let observed = authority
        .load(target.cell_id())
        .await
        .expect("source authority")
        .expect("source owner");
    assert!(observed.value().root.is_some());
    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("source KV");
    let written = kv
        .atomic(
            identity(900_101, now_ms()),
            KvAtomicRequest {
                scope: KV_SCOPE.to_vec(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: KV_KEY.to_vec(),
                    value: KV_PAYLOAD.to_vec(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .expect("acknowledged KV write");
    let KvAtomicOutcome::Applied(results) = written.output else {
        panic!("source KV write was not applied");
    };
    let version = results[0].version.expect("published KV version");
    let blob = typed.blob::<fixture::ReferenceBlob>().expect("source Blob");
    let upload_id = [103; 16];
    let issued_at_ms = now_ms();
    blob.mutate(
        identity(900_102, issued_at_ms),
        BlobMutation::Begin {
            key: BLOB_KEY.to_vec(),
            upload_id,
            condition: BlobCondition::Missing,
            content_type: None,
            metadata: Vec::new(),
            expires_at_ms: issued_at_ms + 60_000,
        },
    )
    .await
    .expect("source Blob begin");
    blob.mutate(
        identity(900_103, issued_at_ms),
        BlobMutation::PutPart {
            key: BLOB_KEY.to_vec(),
            upload_id,
            part_number: 1,
            payload: BLOB_PAYLOAD.to_vec(),
        },
    )
    .await
    .expect("source Blob part");
    let published = blob
        .mutate(
            identity(900_104, issued_at_ms),
            BlobMutation::Complete {
                key: BLOB_KEY.to_vec(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .expect("acknowledged Blob commit");
    let BlobMutationOutcome::Committed { etag, size } = published.output else {
        panic!("source Blob was not committed");
    };
    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("source Queue");
    let queued = queue
        .send(
            identity(900_105, now_ms()),
            QueueSendRequest {
                producer_id: fixed_id(900_105),
                payload: QUEUE_PAYLOAD.to_vec(),
                available_at_ms: now_ms(),
            },
        )
        .await
        .expect("acknowledged Queue send");
    let QueueSendOutcome::Sent { message_id } = queued.output else {
        panic!("source Queue message was not sent");
    };
    let acknowledgement = Acknowledgement {
        sql_sequence: committed.receipt.commit_sequence,
        kv_sequence: written.receipt.commit_sequence,
        kv_version: version.to_vec(),
        blob_sequence: published.receipt.commit_sequence,
        blob_etag: etag,
        blob_size: size,
        queue_sequence: queued.receipt.commit_sequence,
        queue_message_id: message_id,
    };
    let bytes = serde_json::to_vec(&acknowledgement).expect("encode acknowledgement");
    publish_marker(&sync, "ack", &bytes);
    let _node = node;
    std::future::pending::<()>().await;
}

async fn successor_process() {
    let (root, sync) = process_input();
    let case = process_case();
    let acknowledgement = if case == AFTER_ACK {
        let bytes = std::fs::read(sync.join("ack")).expect("owner acknowledgement");
        Some(serde_json::from_slice::<Acknowledgement>(&bytes).expect("decode acknowledgement"))
    } else {
        None
    };
    let (store, _) = rustfs_public_store();
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
    let source_session = SessionId::from_bytes([24; 16]);
    let successor_session = SessionId::from_bytes([70; 16]);
    let fenced = fence_public_session(&layout, source_session, successor_session).await;
    let successor = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(2, 8).expect("successor pool"),
            32 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(successor_session)
        .build()
        .expect("successor node");
    successor
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .expect("successor tasks");
    successor
        .install_node_lease(NodeLeaseGuard::new(0, 60_000).expect("successor lease"))
        .expect("successor readiness");
    let directory = tempfile::tempdir().expect("empty successor directory");
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("successor directory listing")
            .count(),
        0
    );
    let authority = CellAuthority::new(layout.clone());
    let mut restored_cells = Vec::new();
    for (namespace, module, incarnation_byte) in [
        (fixture::SQL_NAMESPACE, fixture::SQL_MODULE, 40_u8),
        (fixture::KV_NAMESPACE, fixture::KV_MODULE, 41_u8),
        (fixture::BLOB_NAMESPACE, fixture::BLOB_MODULE, 42_u8),
        (fixture::QUEUE_NAMESPACE, fixture::QUEUE_MODULE, 43_u8),
    ] {
        let target = CellTarget::new(tenant, application_id, namespace, &partition_for_shard(0))
            .expect("successor target");
        let observed = authority
            .load(target.cell_id())
            .await
            .expect("source authority")
            .expect("source owner");
        let source_root = observed.value().root.clone().expect("acknowledged root");
        let source_epoch = observed.value().epoch;
        let proof = CellCatalog::new(layout.clone(), tenant)
            .lookup(target.cell_id())
            .await
            .expect("source catalog")
            .expect("source provisioned");
        let restored = successor
            .runtime()
            .takeover_restored(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *IncarnationId::from_bytes([incarnation_byte; 16]).as_bytes(),
                    ReplicaLimits::default(),
                )
                .expect("successor replica"),
                authority.clone(),
                observed,
                fenced.direct_takeover().expect("direct takeover proof"),
                RecoveryManifestStore::new(layout.clone(), ReplicaLimits::default()),
                directory.path().join(format!("{module}-successor.sqlite")),
                Owner {
                    session: successor_session,
                    endpoint: "https://public-successor.internal:8081".into(),
                },
            )
            .await
            .expect("exact-root takeover");
        let current = authority
            .load(target.cell_id())
            .await
            .expect("successor authority")
            .expect("successor owner");
        assert!(current.value().epoch > source_epoch);
        assert_eq!(current.value().root.as_ref(), Some(&source_root));
        assert_eq!(
            current.value().owner.as_ref().map(|owner| owner.session),
            Some(successor_session)
        );
        restored_cells.push(restored);
    }
    let typed = successor
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(application.registry(), restored_cells)
                .expect("successor typed client"),
            tenant,
            application_id,
        )
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let target = CellTarget::new(
        tenant,
        application_id,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("successor SQL target");
    let sql = typed
        .sql::<fixture::ReferenceSql>(target)
        .expect("successor SQL");
    let observed = sql
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT payload FROM qualification_rows WHERE id = 1".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .expect("successor SQL read");
    if let Some(acknowledged) = &acknowledgement {
        assert_eq!(
            observed.output[0].rows,
            vec![vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())]]
        );
        assert!(observed.receipt.commit_sequence >= acknowledged.sql_sequence);
    } else {
        assert!(
            observed.output[0].rows.is_empty(),
            "unacknowledged SQL row appeared"
        );
    }
    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("successor KV");
    let observed = kv
        .get(KV_SCOPE.to_vec(), KV_KEY.to_vec(), None)
        .await
        .expect("successor KV read");
    if let Some(acknowledged) = &acknowledgement {
        let entry = observed.output.expect("acknowledged KV value");
        assert_eq!(entry.value, KV_PAYLOAD);
        assert_eq!(entry.version.as_slice(), acknowledged.kv_version);
        assert!(observed.receipt.commit_sequence >= acknowledged.kv_sequence);
    } else {
        assert!(
            observed.output.is_none(),
            "unacknowledged KV value appeared"
        );
    }
    let blob = typed
        .blob::<fixture::ReferenceBlob>()
        .expect("successor Blob");
    let observed = blob
        .query(
            BlobQuery::Read {
                key: BLOB_KEY.to_vec(),
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("successor Blob read");
    if let Some(acknowledged) = &acknowledgement {
        let BlobQueryResult::Read(Some(read)) = observed.output else {
            panic!("acknowledged Blob is absent");
        };
        assert_eq!(read.bytes, BLOB_PAYLOAD);
        assert_eq!(read.metadata.etag, acknowledged.blob_etag);
        assert_eq!(read.metadata.size, acknowledged.blob_size);
        assert!(observed.receipt.commit_sequence >= acknowledged.blob_sequence);
    } else {
        assert!(
            matches!(observed.output, BlobQueryResult::Read(None)),
            "unacknowledged Blob appeared"
        );
    }
    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("successor Queue");
    let info = queue.info(0, None).await.expect("successor Queue info");
    if let Some(acknowledged) = &acknowledgement {
        assert_eq!(info.output.ready, 1);
        assert_eq!(info.output.leased, 0);
        assert!(info.receipt.commit_sequence >= acknowledged.queue_sequence);
        let claimed = queue
            .claim(
                identity(900_106, now_ms()),
                0,
                QueueClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("successor Queue claim");
        assert_eq!(claimed.output.len(), 1);
        let message = &claimed.output[0];
        assert_eq!(message.message_id, acknowledged.queue_message_id);
        assert_eq!(message.payload, QUEUE_PAYLOAD);
        assert_eq!(message.attempt, 1);
        let acked = queue
            .ack(
                identity(900_107, now_ms()),
                0,
                message.message_id,
                message.token,
            )
            .await
            .expect("successor Queue ack");
        assert!(matches!(
            acked.output,
            QueueLeaseOutcome::Applied {
                state: QueueState::Acked,
                ..
            }
        ));
        let settled = queue
            .info(0, Some(acked.receipt))
            .await
            .expect("successor Queue settlement");
        assert_eq!(settled.output.acked, 1);
        assert_eq!(settled.output.ready, 0);
        assert_eq!(settled.output.leased, 0);
    } else {
        assert_eq!(info.output.ready, 0);
        assert_eq!(info.output.leased, 0);
        assert_eq!(info.output.acked, 0);
    }
    successor.shutdown().await.expect("successor drain");
    assert_zero_reservations(&successor);
    let observation = if acknowledgement.is_some() {
        let mut values = Vec::from(SQL_PAYLOAD);
        values.extend_from_slice(KV_PAYLOAD);
        values.extend_from_slice(BLOB_PAYLOAD);
        values.extend_from_slice(QUEUE_PAYLOAD);
        values
    } else {
        b"absent".to_vec()
    };
    publish_marker(&sync, "observation", &observation);
}

fn spawn_role(
    binary: &FilePath,
    role: &str,
    case: &str,
    root: &Path,
    sync: &FilePath,
) -> ChildGuard {
    let child = Command::new(binary)
        .args(["--exact", "process_role", "--nocapture"])
        .env(ROLE_ENV, role)
        .env(CASE_ENV, case)
        .env(ROOT_ENV, root.to_string())
        .env(SYNC_ENV, sync)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("fault process spawn");
    ChildGuard(child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_recovers_acknowledged_sql_kv_blob_and_queue_in_successor_process() {
    let mut expected = Vec::from(SQL_PAYLOAD);
    expected.extend_from_slice(KV_PAYLOAD);
    expected.extend_from_slice(BLOB_PAYLOAD);
    expected.extend_from_slice(QUEUE_PAYLOAD);
    run_process_fault(AFTER_ACK, "ack", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_owner_kill_before_sql_kv_blob_and_queue_write_has_no_ghost_state() {
    run_process_fault(BEFORE_WRITE, "ready", b"absent").await;
}

async fn run_process_fault(case: &str, barrier: &str, expected: &[u8]) {
    let (_, root) = rustfs_public_store();
    let sync = tempfile::tempdir().expect("fault synchronization directory");
    let binary = env::current_exe().expect("test binary");
    let mut owner = spawn_role(&binary, "owner", case, &root, sync.path());
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
    let mut successor = spawn_role(&binary, "successor", case, &root, sync.path());
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
}
