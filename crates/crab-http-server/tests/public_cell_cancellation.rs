use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    CellTarget, Resolution, SqlBatch, SqlBatchCommand, SqlStatement, SqlValue, StoredOutcome,
    partition_for_shard,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(dead_code, reason = "this test uses the paused peer fault helper")]
mod qualification_peer;

use qualification::{assert_zero_reservations, identity, rustfs_public_store};
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
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
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
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
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
    let evidence = prepared.evidence().clone();
    let retained_attempt = prepared.clone();
    let task = tokio::spawn(async move { prepared.execute().await });
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("mutation reached injected await boundary");
    task.abort();
    assert!(task.await.expect_err("cancelled task join").is_cancelled());
    assert_eq!(
        dispatched.load(Ordering::Acquire),
        usize::from(after_dispatch)
    );

    let resolution = observer
        .resolve(&evidence)
        .await
        .expect("separate client request resolution");
    let commit_sequence = match (after_dispatch, resolution) {
        (
            true,
            Resolution::Committed(StoredOutcome::Success {
                commit_sequence, ..
            }),
        ) => commit_sequence,
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

fn select_row() -> SqlBatch {
    SqlBatch {
        statements: vec![SqlStatement {
            sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
            parameters: vec![SqlValue::Integer(1)],
        }],
    }
}
