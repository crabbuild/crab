use super::performance_fixture::{PerfFixture, identity, item_id, now_ms};
use crate::*;
use std::time::{Duration, Instant};

use crab_cell_runtime::primitives::effects::EffectRunOutcome;
use crab_cell_runtime::primitives::maintenance::{MaintenanceTickOutcome, MaintenanceTickRequest};

async fn measure<F, Fut>(name: &str, iterations: usize, mut action: F) -> Vec<Duration>
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
    report_samples(name, &mut samples, elapsed);
    samples
}

pub(super) fn report_samples(name: &str, samples: &mut [Duration], elapsed: Duration) {
    samples.sort_unstable();
    let iterations = samples.len();
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
    let fixture = PerfFixture::start(1).await;
    run_reference_primitive_performance(&fixture, false, "single_runtime").await;
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "manual three-node end-to-end performance run"]
async fn reference_three_node_fleet_end_to_end_performance() {
    let fixture = PerfFixture::start(3).await;
    run_reference_primitive_performance(&fixture, true, "fleet_three_runtime_mixed").await;
    fixture.shutdown().await;
}

pub(super) async fn run_reference_primitive_performance(
    fixture: &PerfFixture,
    concurrent: bool,
    fleet_label: &str,
) {
    let iterations = std::env::var("CRAB_CELL_PERF_ITERATIONS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(30);
    assert!((1..=1_000).contains(&iterations));
    let sql = fixture
        .typed
        .sql::<ReferenceSql>(fixture.sql_target.clone())
        .unwrap();
    let kv = fixture.typed.kv::<ReferenceKv>(KV_NAMESPACE).unwrap();
    let blob = fixture.typed.blob::<ReferenceBlob>().unwrap();
    let queue = fixture.typed.queue::<ReferenceQueue>().unwrap();
    let workflow = fixture.typed.workflow::<ReferenceWorkflow>().unwrap();
    let activity = crab_cell_runtime::primitives::workflow::ActivitySupervisor::new(
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

    let sql_run = measure("sql_order_insert_read", iterations, |index| async move {
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
    });

    let kv_run = measure("kv_cart_put_get", iterations, |index| async move {
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
    });

    let blob_run = measure(
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
    );

    let queue_run = measure(
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
                crab_cell_runtime::primitives::queue::QueueSendOutcome::Sent { .. }
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
    );

    let workflow_run = measure(
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
                crab_cell_runtime::primitives::workflow::WorkflowOutcome::Applied {
                    run_id,
                    ..
                } => run_id,
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
    );

    let cron_run = measure(
        "cron_invoice_schedule_deliver",
        iterations,
        |index| async move {
            let schedule_id = item_id(index);
            let payload = format!("invoice/{index}").into_bytes();
            let scheduled = cron
                .mutate(
                    identity(6, index, 0),
                    CronMutation::Upsert {
                        schedule_id,
                        target_index: 0,
                        target_partition: partition_for_shard(0).to_vec(),
                        payload: payload.clone(),
                        interval_ms: 60_000,
                        next_due_ms: now_ms() + 5,
                    },
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(6)).await;
            let tick = fixture
                .registry
                .run_maintenance_once(
                    fixture.client.clone(),
                    fixture.cron_target.clone(),
                    identity(6, index, 1),
                    MaintenanceTickRequest {
                        expected_commit_sequence: scheduled.receipt.commit_sequence,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                tick.output,
                MaintenanceTickOutcome::Applied { processed: 1 }
            ));
            let fired = cron
                .get(schedule_id, Some(tick.receipt))
                .await
                .unwrap()
                .output;
            assert!(
                matches!(fired, CronQueryResult::Get(Some(schedule)) if schedule.occurrence == 1)
            );
            let delivered = fixture
                .registry
                .run_effect_once(
                    fixture.client.clone(),
                    fixture.cron_target.clone(),
                    (*peer).clone(),
                    5_000,
                )
                .await
                .unwrap();
            assert!(
                matches!(delivered, EffectRunOutcome::Delivered { .. }),
                "{delivered:?}"
            );
            let observed = sql.query(None, SqlBatch { statements: vec![SqlStatement {
            sql: "SELECT payload FROM invoice_receipts WHERE schedule_id = ?1 AND occurrence = 1".into(),
            parameters: vec![SqlValue::Blob(schedule_id.to_vec())],
        }] }).await.unwrap();
            assert_eq!(observed.output[0].rows, vec![vec![SqlValue::Blob(payload)]]);
        },
    );

    if concurrent {
        let started = Instant::now();
        let (sql, kv, blob, queue, workflow, cron) =
            tokio::join!(sql_run, kv_run, blob_run, queue_run, workflow_run, cron_run);
        let elapsed = started.elapsed();
        let mut samples = [sql, kv, blob, queue, workflow, cron].concat();
        samples.sort_unstable();
        let percentile = |percent: usize| {
            samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)].as_secs_f64()
                * 1_000.0
        };
        println!(
            "PERF {fleet_label}: count={} elapsed_s={:.3} ops_per_s={:.2} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            samples.len(),
            elapsed.as_secs_f64(),
            samples.len() as f64 / elapsed.as_secs_f64(),
            percentile(50),
            percentile(95),
            percentile(99),
            percentile(100),
        );
    } else {
        sql_run.await;
        kv_run.await;
        blob_run.await;
        queue_run.await;
        workflow_run.await;
        cron_run.await;
    }
}
