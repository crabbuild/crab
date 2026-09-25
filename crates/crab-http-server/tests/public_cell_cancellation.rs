use std::sync::atomic::Ordering;

use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::identity::{CellTarget, partition_for_shard};
use crab_cell_runtime::primitives::cron::{
    CronCommand, CronMutation, CronMutationOutcome, CronQueryResult,
};
use crab_cell_runtime::primitives::kv::{
    KvAtomicCommand, KvAtomicOutcome, KvAtomicRequest, KvMutation,
};
use crab_cell_runtime::primitives::sql::{
    SqlBatch, SqlBatchCommand, SqlResultSet, SqlStatement, SqlValue,
};
use crab_cell_runtime::primitives::workflow::{
    WorkflowOutcome, WorkflowStart, WorkflowStartCommand, WorkflowStatus,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_cancellation.rs"]
mod qualification_cancellation;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(dead_code, reason = "this test uses the paused peer fault helper")]
mod qualification_peer;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_cancellation::{cancel_prepared, committed_output, now_ms};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_client_with_paused_mutation;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_sql_cancellation_before_dispatch_resolves_absent() {
    run_sql_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_sql_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_sql_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_sql_cancellation_after_dispatch_resolves_committed() {
    run_sql_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_sql_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_sql_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_sql_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .unwrap();
    let target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("SQL target");
    let observer_sql = observer
        .sql::<fixture::ReferenceSql>(target.clone())
        .expect("observer SQL");
    let now_ms = now_ms();
    let marker = if after_dispatch {
        b"cancelled-after-publication".to_vec()
    } else {
        b"cancelled-before-publication".to_vec()
    };
    let insert = SqlBatch {
        statements: vec![SqlStatement {
            sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
            parameters: vec![SqlValue::Integer(1), SqlValue::Blob(marker.clone())],
        }],
    };
    let prepared = peer
        .prepare_command::<SqlBatchCommand<fixture::ReferenceSql>>(
            &target,
            identity(230 + u64::from(after_dispatch), now_ms),
            insert,
        )
        .await
        .expect("prepare exact SQL mutation");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let commit_sequence = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<Vec<SqlResultSet>>(outcome);
            assert_eq!(output[0].rows_affected, 1);
            commit_sequence
        }
        (false, Resolution::Absent) => {
            let absent = observer_sql
                .query(None, select_row())
                .await
                .expect("independent absence observation");
            assert!(absent.output[0].rows.is_empty());
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent request");
            assert_eq!(retried.output[0].rows_affected, 1);
            retried.receipt.commit_sequence
        }
        (_, outcome) => panic!("unexpected cancelled SQL resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_sql
        .query(None, select_row())
        .await
        .expect("independent SQL state observation");
    assert_eq!(observed.output[0].rows, vec![vec![SqlValue::Blob(marker)]]);
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_kv_cancellation_before_dispatch_resolves_absent() {
    run_kv_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_kv_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_kv_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_kv_cancellation_after_dispatch_resolves_committed() {
    run_kv_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_kv_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_kv_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_kv_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .unwrap();
    let target = CellTarget::new(
        tenant,
        application,
        fixture::KV_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("KV target");
    let observer_kv = observer
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("observer KV");
    let scope = b"cancelled-kv".to_vec();
    let key = b"one-value".to_vec();
    let marker = if after_dispatch {
        b"cancelled-kv-after".to_vec()
    } else {
        b"cancelled-kv-before".to_vec()
    };
    let prepared = peer
        .prepare_command::<KvAtomicCommand<fixture::ReferenceKv>>(
            &target,
            identity(240 + u64::from(after_dispatch), now_ms()),
            KvAtomicRequest {
                scope: scope.clone(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: key.clone(),
                    value: marker.clone(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .expect("prepare exact KV mutation");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let (commit_sequence, expected_version) = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<KvAtomicOutcome>(outcome);
            let KvAtomicOutcome::Applied(results) = output else {
                panic!("cancelled KV ledger result was not applied");
            };
            (
                commit_sequence,
                results[0].version.expect("committed KV version"),
            )
        }
        (false, Resolution::Absent) => {
            let absent = observer_kv
                .get(scope.clone(), key.clone(), None)
                .await
                .expect("independent KV absence observation");
            assert!(absent.output.is_none());
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent KV request");
            let KvAtomicOutcome::Applied(results) = retried.output else {
                panic!("cancelled KV retry was not applied");
            };
            (
                retried.receipt.commit_sequence,
                results[0].version.expect("retried KV version"),
            )
        }
        (_, outcome) => panic!("unexpected cancelled KV resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_kv
        .get(scope, key, None)
        .await
        .expect("independent KV state observation");
    let entry = observed.output.expect("cancelled KV value");
    assert_eq!(entry.value, marker);
    assert_eq!(entry.version, expected_version);
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cron_cancellation_before_dispatch_resolves_absent() {
    run_cron_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cron_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_cron_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cron_cancellation_after_dispatch_resolves_committed() {
    run_cron_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cron_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_cron_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_cron_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .unwrap();
    let target = CellTarget::new(
        tenant,
        application,
        fixture::CRON_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Cron target");
    let observer_cron = observer
        .cron::<fixture::ReferenceCron>()
        .expect("observer Cron");
    let schedule_id = fixed_id(242 + u64::from(after_dispatch));
    let marker = if after_dispatch {
        b"cancelled-cron-after".to_vec()
    } else {
        b"cancelled-cron-before".to_vec()
    };
    let now_ms = now_ms();
    let next_due_ms = now_ms + 60_000;
    let prepared = peer
        .prepare_command::<CronCommand<fixture::ReferenceCron>>(
            &target,
            identity(242 + u64::from(after_dispatch), now_ms),
            CronMutation::Upsert {
                schedule_id,
                target_index: 0,
                target_partition: partition_for_shard(0).to_vec(),
                payload: marker.clone(),
                interval_ms: 60_000,
                next_due_ms,
            },
        )
        .await
        .expect("prepare exact Cron mutation");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let commit_sequence = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<CronMutationOutcome>(outcome);
            assert_eq!(output, CronMutationOutcome::Applied { generation: 1 });
            commit_sequence
        }
        (false, Resolution::Absent) => {
            let absent = observer_cron
                .get(schedule_id, None)
                .await
                .expect("independent Cron absence observation");
            assert!(matches!(absent.output, CronQueryResult::Get(None)));
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Cron request");
            assert_eq!(
                retried.output,
                CronMutationOutcome::Applied { generation: 1 }
            );
            retried.receipt.commit_sequence
        }
        (_, outcome) => panic!("unexpected cancelled Cron resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_cron
        .get(schedule_id, None)
        .await
        .expect("independent Cron state observation");
    let CronQueryResult::Get(Some(schedule)) = observed.output else {
        panic!("cancelled Cron schedule is absent");
    };
    assert_eq!(schedule.payload, marker);
    assert_eq!(schedule.generation, 1);
    assert_eq!(schedule.next_due_ms, next_due_ms);
    assert_eq!(schedule.occurrence, 0);
    assert!(schedule.enabled);
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_workflow_cancellation_before_dispatch_resolves_absent() {
    run_workflow_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_workflow_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_workflow_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_workflow_cancellation_after_dispatch_resolves_committed() {
    run_workflow_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_workflow_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_workflow_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_workflow_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .unwrap();
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Workflow target");
    let observer_workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let workflow_id = if after_dispatch {
        b"cancelled-workflow-after".to_vec()
    } else {
        b"cancelled-workflow-before".to_vec()
    };
    let event = if after_dispatch {
        b"completed-after-cancellation-after".to_vec()
    } else {
        b"completed-after-cancellation-before".to_vec()
    };
    let mutation = identity(244 + u64::from(after_dispatch), now_ms());
    let prepared = peer
        .prepare_command::<WorkflowStartCommand<fixture::ReferenceWorkflow>>(
            &target,
            mutation,
            WorkflowStart {
                workflow_id: workflow_id.clone(),
                request_id: mutation.request_id,
                event: event.clone(),
            },
        )
        .await
        .expect("prepare exact Workflow start");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let (commit_sequence, expected_run_id) = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<WorkflowOutcome>(outcome);
            let WorkflowOutcome::Applied {
                run_id,
                status: WorkflowStatus::Completed,
                event_sequence: 1,
            } = output
            else {
                panic!("cancelled Workflow ledger result did not complete");
            };
            (commit_sequence, run_id)
        }
        (false, Resolution::Absent) => {
            let absent = observer_workflow
                .state(workflow_id.clone(), None)
                .await
                .expect("independent Workflow absence observation");
            assert!(absent.output.is_none());
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Workflow start");
            let WorkflowOutcome::Applied {
                run_id,
                status: WorkflowStatus::Completed,
                event_sequence: 1,
            } = retried.output
            else {
                panic!("cancelled Workflow retry did not complete");
            };
            (retried.receipt.commit_sequence, run_id)
        }
        (_, outcome) => panic!("unexpected cancelled Workflow resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_workflow
        .state(workflow_id.clone(), None)
        .await
        .expect("independent Workflow state observation");
    let run = observed.output.expect("cancelled Workflow run");
    assert_eq!(run.workflow_id, workflow_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 1);
    assert_eq!(run.result.as_deref(), Some(event.as_slice()));
    assert_eq!(run.run_id, expected_run_id);
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

fn select_row() -> SqlBatch {
    SqlBatch {
        statements: vec![SqlStatement {
            sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
            parameters: vec![SqlValue::Integer(1)],
        }],
    }
}
