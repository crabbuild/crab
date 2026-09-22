use super::*;
use std::time::{Duration, Instant, SystemTime};

use crab_cell_runtime::{
    EffectPeerClient, EffectRunOutcome, MaintenanceTickOutcome, MaintenanceTickRequest,
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal, PeerRoundTrip, PeerSigner,
    PeerVerifier, VerifiedPeerRequest,
};

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn identity(phase: u8, index: usize, step: u8) -> MutationIdentity {
    let mut bytes = [0; 16];
    bytes[0] = phase;
    bytes[1] = step;
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    let now = now_ms();
    MutationIdentity {
        request_id: RequestId::from_bytes(bytes),
        issued_at_ms: now,
        expires_at_ms: now + 300_000,
    }
}

fn item_id(index: usize) -> [u8; 16] {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&(index as u64).to_be_bytes());
    id
}

fn install_sql_tables(tx: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE orders(id INTEGER PRIMARY KEY, total_cents INTEGER NOT NULL); \
         CREATE TABLE invoice_receipts(schedule_id BLOB NOT NULL, occurrence INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(schedule_id, occurrence));",
    )?;
    Ok(())
}

struct PerfFixture {
    _directory: tempfile::TempDir,
    runtime: CellRuntime,
    typed: ApplicationHandle<ReferenceApplication>,
    registry: Arc<Registry>,
    client: CellClient,
    sql_target: CellTarget,
    cron_target: CellTarget,
    sql_handle: CellHandle,
}

impl PerfFixture {
    async fn start() -> Self {
        let application = Arc::new(compiled());
        let registry = application.registry();
        let tenant = TenantId::from_bytes([81; 16]);
        let application_id = ApplicationId::from_bytes([82; 16]);
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = CellStorageLayout::new(
            store.clone(),
            object_store::path::Path::from("reference-performance"),
            *application_id.as_bytes(),
        );
        let directory = tempfile::TempDir::new().unwrap();
        let runtime = CellRuntime::new_with_replica_host(
            SqlWorkerPool::new(4, 32).unwrap(),
            64 * 1024 * 1024,
            crab_cell_runtime::SessionId::from_bytes([24; 16]),
            reference_host(),
        )
        .unwrap();
        type Schema = for<'a> fn(&crab_ltx::rusqlite::Transaction<'a>) -> Result<()>;
        let cells: [(NamespaceId, CatalogRole, &'static str, u8, Schema); 7] = [
            (
                SQL_NAMESPACE,
                CatalogRole::Sql,
                SQL_MODULE,
                40,
                install_sql_tables,
            ),
            (
                KV_NAMESPACE,
                CatalogRole::Kv,
                KV_MODULE,
                41,
                install_kv_schema,
            ),
            (
                BLOB_NAMESPACE,
                CatalogRole::Blob,
                BLOB_MODULE,
                42,
                install_blob_schema,
            ),
            (
                QUEUE_NAMESPACE,
                CatalogRole::Queue,
                QUEUE_MODULE,
                43,
                install_queue_schema,
            ),
            (
                DEAD_LETTER_NAMESPACE,
                CatalogRole::Queue,
                DEAD_LETTER_MODULE,
                44,
                install_queue_schema,
            ),
            (
                CRON_NAMESPACE,
                CatalogRole::Cron,
                CRON_MODULE,
                45,
                install_cron_schema,
            ),
            (
                WORKFLOW_NAMESPACE,
                CatalogRole::Workflow,
                WORKFLOW_MODULE,
                46,
                install_workflow_schema,
            ),
        ];
        let mut handles = Vec::new();
        for (namespace, role, module, incarnation, schema) in cells {
            handles.push(
                bootstrap_reference_cell(
                    &runtime,
                    &registry,
                    &layout,
                    &directory,
                    tenant,
                    application_id,
                    namespace,
                    role,
                    module,
                    incarnation,
                    schema,
                )
                .await
                .unwrap(),
            );
        }
        let sql_handle = handles[0].clone();
        let client = CellClient::local_many(Arc::clone(&registry), handles).unwrap();
        let typed = ApplicationHandle::new(client.clone(), application, tenant, application_id)
            .with_blob_artifact_store(BlobArtifactStore::new(store));
        let sql_target = CellTarget::new(
            tenant,
            application_id,
            SQL_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        let cron_target = CellTarget::new(
            tenant,
            application_id,
            CRON_NAMESPACE,
            &partition_for_shard(0),
        )
        .unwrap();
        Self {
            _directory: directory,
            runtime,
            typed,
            registry,
            client,
            sql_target,
            cron_target,
            sql_handle,
        }
    }

    fn cron_peer(&self) -> EffectPeerClient {
        let session = crab_cell_runtime::SessionId::from_bytes([77; 16]);
        let signer = PeerSigner::new(
            session,
            self.registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        );
        let verifier = Arc::new(PeerVerifier::new(
            session,
            self.registry.release_digest(),
            signer.verifying_key(),
        ));
        let dispatcher = Arc::new(PeerDispatcher::new(
            Arc::clone(&self.registry),
            Arc::new(SqlResolver {
                target: self.sql_target.clone(),
                handle: self.sql_handle.clone(),
            }),
            Arc::new(CronAuthorizer),
        ));
        EffectPeerClient::new(
            Arc::new(signer),
            PeerPrincipal {
                issuer: "reference-performance".into(),
                subject: "cron-supervisor".into(),
                actions: vec!["reference.cron.deliver".into()],
            },
            Arc::new(Loopback {
                verifier,
                dispatcher,
            }),
        )
    }
}

struct SqlResolver {
    target: CellTarget,
    handle: CellHandle,
}

impl PeerCellResolver for SqlResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>> {
        let allowed = target == self.target;
        let handle = self.handle.clone();
        Box::pin(async move {
            if allowed {
                Ok(handle)
            } else {
                Err(Error::CellNotActive)
            }
        })
    }
}

struct CronAuthorizer;

impl PeerAuthorizer for CronAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()> {
        if request.permits("reference.cron.deliver") {
            Ok(())
        } else {
            Err(Error::PeerAuthorization("missing cron delivery action"))
        }
    }
}

struct Loopback {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for Loopback {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let verified = verifier.verify(&request, now_ms())?;
            if verified.target() != &target {
                return Err(Error::Peer("round trip target changed"));
            }
            dispatcher.dispatch_bytes(&verified, now_ms()).await
        })
    }
}

async fn measure<F, Fut>(name: &str, iterations: usize, mut action: F)
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut samples = Vec::with_capacity(iterations);
    let started = Instant::now();
    for index in 0..iterations {
        let operation_started = Instant::now();
        action(index).await;
        samples.push(operation_started.elapsed());
    }
    let elapsed = started.elapsed();
    samples.sort_unstable();
    let percentile = |percent: usize| {
        samples[(iterations * percent).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1_000.0
    };
    println!(
        "PERF {name}: count={iterations} elapsed_s={:.3} ops_per_s={:.2} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
        elapsed.as_secs_f64(),
        iterations as f64 / elapsed.as_secs_f64(),
        percentile(50),
        percentile(95),
        percentile(99),
        percentile(100),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual end-to-end performance run"]
async fn reference_primitive_end_to_end_performance() {
    let iterations = std::env::var("CRAB_CELL_PERF_ITERATIONS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(30);
    assert!((1..=1_000).contains(&iterations));
    let fixture = PerfFixture::start().await;
    let sql = fixture
        .typed
        .sql::<ReferenceSql>(fixture.sql_target.clone())
        .unwrap();
    let kv = fixture.typed.kv::<ReferenceKv>(KV_NAMESPACE).unwrap();
    let blob = fixture.typed.blob::<ReferenceBlob>().unwrap();
    let queue = fixture.typed.queue::<ReferenceQueue>().unwrap();
    let workflow = fixture.typed.workflow::<ReferenceWorkflow>().unwrap();
    let activity = crab_cell_runtime::ActivitySupervisor::new(
        fixture.typed.activities::<ReferenceWorkflow>().unwrap(),
        5_000,
    )
    .unwrap();
    let cron = fixture.typed.cron::<ReferenceCron>().unwrap();
    let peer = fixture.cron_peer();
    let sql = &sql;
    let kv = &kv;
    let blob = &blob;
    let queue = &queue;
    let workflow = &workflow;
    let activity = &activity;
    let cron = &cron;
    let peer = &peer;
    let fixture = &fixture;

    measure("sql_order_insert_read", iterations, |index| async move {
        let committed = sql
            .batch(
                identity(1, index, 0),
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "INSERT INTO orders(id, total_cents) VALUES (?1, ?2)".into(),
                        parameters: vec![
                            SqlValue::Integer(index as i64),
                            SqlValue::Integer(1_999 + index as i64),
                        ],
                    }],
                },
            )
            .await
            .unwrap();
        let observed = sql
            .query(
                Some(committed.receipt),
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT total_cents FROM orders WHERE id = ?1".into(),
                        parameters: vec![SqlValue::Integer(index as i64)],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(
            observed.output[0].rows,
            vec![vec![SqlValue::Integer(1_999 + index as i64)]]
        );
    })
    .await;

    measure("kv_cart_put_get", iterations, |index| async move {
        let key = format!("cart/{index}").into_bytes();
        let value = format!("{{\"sku\":\"book\",\"quantity\":{}}}", index + 1).into_bytes();
        let committed = kv
            .atomic(
                identity(2, index, 0),
                KvAtomicRequest {
                    scope: b"shop".to_vec(),
                    checks: Vec::new(),
                    mutations: vec![KvMutation::Put {
                        key: key.clone(),
                        value: value.clone(),
                        expires_at_ms: None,
                    }],
                },
            )
            .await
            .unwrap();
        let observed = kv
            .get(b"shop".to_vec(), key, Some(committed.receipt))
            .await
            .unwrap();
        assert_eq!(observed.output.unwrap().value, value);
    })
    .await;

    measure(
        "blob_attachment_upload_read_32k",
        iterations,
        |index| async move {
            let key = format!("attachments/{index}.bin").into_bytes();
            let mut payload = vec![0; 32 * 1024];
            blake3::Hasher::new()
                .update(&(index as u64).to_be_bytes())
                .finalize_xof()
                .fill(&mut payload);
            let upload_id = item_id(index);
            blob.mutate(
                identity(3, index, 0),
                BlobMutation::Begin {
                    key: key.clone(),
                    upload_id,
                    condition: BlobCondition::Missing,
                    content_type: Some("application/octet-stream".into()),
                    metadata: Vec::new(),
                    expires_at_ms: now_ms() + 300_000,
                },
            )
            .await
            .unwrap();
            blob.mutate(
                identity(3, index, 1),
                BlobMutation::PutPart {
                    key: key.clone(),
                    upload_id,
                    part_number: 1,
                    payload: payload.clone(),
                },
            )
            .await
            .unwrap();
            let committed = blob
                .mutate(
                    identity(3, index, 2),
                    BlobMutation::Complete {
                        key: key.clone(),
                        upload_id,
                        part_count: 1,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                committed.output,
                BlobMutationOutcome::Committed { size: 32_768, .. }
            ));
            let observed = blob
                .query(
                    BlobQuery::Read {
                        key,
                        offset: 0,
                        limit: 32 * 1024,
                    },
                    Some(committed.receipt),
                )
                .await
                .unwrap();
            match observed.output {
                BlobQueryResult::Read(Some(read)) => assert_eq!(read.bytes, payload),
                other => panic!("unexpected Blob read: {other:?}"),
            }
        },
    )
    .await;

    measure(
        "queue_notification_send_claim_ack",
        iterations,
        |index| async move {
            let payload = format!("notify-order-{index}").into_bytes();
            let sent = queue
                .send(
                    identity(4, index, 0),
                    QueueSendRequest {
                        producer_id: item_id(index),
                        payload: payload.clone(),
                        available_at_ms: now_ms(),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                sent.output,
                crab_cell_runtime::QueueSendOutcome::Sent { .. }
            ));
            let claimed = queue
                .claim(
                    identity(4, index, 1),
                    0,
                    QueueClaimRequest {
                        limit: 1,
                        lease_ms: 5_000,
                    },
                )
                .await
                .unwrap();
            assert_eq!(claimed.output.len(), 1);
            assert_eq!(claimed.output[0].payload, payload);
            assert!(
                queue
                    .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
                    .await
                    .unwrap()
                    .output
            );
            let message = &claimed.output[0];
            let ack = queue
                .ack(identity(4, index, 2), 0, message.message_id, message.token)
                .await
                .unwrap();
            assert!(matches!(ack.output, QueueLeaseOutcome::Applied { .. }));
        },
    )
    .await;

    measure(
        "workflow_fulfillment_activity",
        iterations,
        |index| async move {
            let workflow_id = format!("fulfillment/{index}").into_bytes();
            let started = workflow
                .start(
                    identity(5, index, 0),
                    workflow_id.clone(),
                    b"activity".to_vec(),
                )
                .await
                .unwrap();
            let run_id = match started.output {
                crab_cell_runtime::WorkflowOutcome::Applied { run_id, .. } => run_id,
                other => panic!("unexpected Workflow start: {other:?}"),
            };
            assert!(matches!(
                activity.run_once(0, None).await.unwrap(),
                ActivityRunOutcome::Completed { .. }
            ));
            let observed = workflow
                .state(workflow_id, None)
                .await
                .unwrap()
                .output
                .unwrap();
            assert_eq!(observed.run_id, run_id);
            assert_eq!(observed.status, WorkflowStatus::Completed);
        },
    )
    .await;

    measure("cron_invoice_schedule_deliver", iterations, |index| async move {
        let schedule_id = item_id(index);
        let payload = format!("invoice/{index}").into_bytes();
        let scheduled = cron.mutate(identity(6, index, 0), CronMutation::Upsert {
            schedule_id, target_index: 0, target_partition: partition_for_shard(0).to_vec(),
            payload: payload.clone(), interval_ms: 60_000, next_due_ms: now_ms() + 5,
        }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(6)).await;
        let tick = fixture.registry.run_maintenance_once(
            fixture.client.clone(), fixture.cron_target.clone(), identity(6, index, 1),
            MaintenanceTickRequest { expected_commit_sequence: scheduled.receipt.commit_sequence },
        ).await.unwrap();
        assert!(matches!(tick.output, MaintenanceTickOutcome::Applied { processed: 1 }));
        let fired = cron.get(schedule_id, Some(tick.receipt)).await.unwrap().output;
        assert!(matches!(fired, CronQueryResult::Get(Some(schedule)) if schedule.occurrence == 1));
        let delivered = fixture.registry.run_effect_once(
            fixture.client.clone(), fixture.cron_target.clone(), (*peer).clone(), 5_000,
        ).await.unwrap();
        assert!(matches!(delivered, EffectRunOutcome::Delivered { .. }), "{delivered:?}");
        let observed = sql.query(None, SqlBatch { statements: vec![SqlStatement {
            sql: "SELECT payload FROM invoice_receipts WHERE schedule_id = ?1 AND occurrence = 1".into(),
            parameters: vec![SqlValue::Blob(schedule_id.to_vec())],
        }] }).await.unwrap();
        assert_eq!(observed.output[0].rows, vec![vec![SqlValue::Blob(payload)]]);
    }).await;

    fixture.runtime.shutdown().await.unwrap();
}
