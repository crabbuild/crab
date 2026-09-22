use std::{sync::atomic::Ordering, time::Duration};

use cellule_runtime::{
    CellTarget, EffectClaimRequest, EffectLeaseOutcome, EffectState, EffectStatus, Resolution,
    SqlBatch, SqlResultSet, SqlStatement, SqlValue, WorkflowOutcome, WorkflowStatus,
    partition_for_shard,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_cancellation.rs"]
#[expect(dead_code, reason = "this test uses only the result decoder and clock")]
mod qualification_cancellation;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(dead_code, reason = "this test uses the paused Effect delivery helper")]
mod qualification_peer;

use qualification::{assert_zero_reservations, identity, rustfs_public_store};
use qualification_cancellation::{committed_output, now_ms};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_effect_client_with_paused_delivery;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_delivery_cancellation_before_dispatch_resolves_absent() {
    run_effect_delivery_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_delivery_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_effect_delivery_cancellation(public_host_fixture_with_store(store, root).await, false)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_delivery_cancellation_after_dispatch_resolves_committed() {
    run_effect_delivery_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_delivery_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_effect_delivery_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_effect_delivery_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer, entered, dispatched) =
        peer_effect_client_with_paused_delivery(registry, handles, after_dispatch);
    let workflow_target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect source target");
    let sql_target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect destination target");
    let source = observer
        .effects::<fixture::ReferenceWorkflow>(workflow_target)
        .expect("observer Effect source");
    let destination = observer
        .sql::<fixture::ReferenceSql>(sql_target)
        .expect("observer SQL destination");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let issued_at_ms = now_ms();
    let workflow_id = if after_dispatch {
        b"cancelled-effect-delivery-after".to_vec()
    } else {
        b"cancelled-effect-delivery-before".to_vec()
    };
    let started = workflow
        .start(
            identity(290 + u64::from(after_dispatch), issued_at_ms),
            workflow_id,
            b"effect-valid".to_vec(),
        )
        .await
        .expect("source Effect publication");
    assert!(matches!(
        started.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            event_sequence: 1,
            ..
        }
    ));
    let claimed = source
        .claim(
            identity(292 + u64::from(after_dispatch), now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("source Effect claim");
    assert_eq!(claimed.output.len(), 1);
    let claim = claimed.output[0].clone();
    let live = source
        .validate(vec![claim.clone()], claimed.receipt)
        .await
        .expect("independent source Effect lease observation");
    assert!(live.output);

    let delivery = {
        let peer = peer.clone();
        let claim = claim.clone();
        tokio::spawn(async move { peer.deliver(&claim, now_ms()).await })
    };
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("Effect delivery reached injected await boundary");
    delivery.abort();
    assert!(matches!(delivery.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        dispatched.load(Ordering::Acquire),
        usize::from(after_dispatch)
    );
    let resolution = peer
        .resolve(&claim, now_ms())
        .await
        .expect("separate destination inbox resolution");
    let outcome = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => outcome,
        (false, Resolution::Absent) => {
            let absent = destination
                .query(None, select_effect_row())
                .await
                .expect("independent destination absence observation");
            assert!(absent.output[0].rows.is_empty());
            peer.deliver(&claim, now_ms())
                .await
                .expect("retry exact absent Effect delivery")
        }
        (_, resolution) => panic!("unexpected cancelled Effect resolution: {resolution:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let (output, commit_sequence) = committed_output::<Vec<SqlResultSet>>(outcome.clone());
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].rows_affected, 1);
    let observed = destination
        .query(None, select_effect_row())
        .await
        .expect("independent destination Effect observation");
    assert_eq!(
        observed.output[0].rows,
        vec![vec![SqlValue::Blob(b"delivered-effect".to_vec())]]
    );
    assert_eq!(observed.output[1].rows, vec![vec![SqlValue::Integer(1)]]);
    assert!(observed.receipt.commit_sequence >= commit_sequence);
    let replay = peer
        .deliver(&claim, now_ms())
        .await
        .expect("duplicate Effect inbox delivery");
    assert_eq!(replay, outcome);
    let acked = source
        .ack(
            identity(294 + u64::from(after_dispatch), now_ms()),
            claim.clone(),
            outcome.result().to_vec(),
        )
        .await
        .expect("source Effect acknowledgement");
    assert_eq!(acked.output, EffectLeaseOutcome::Delivered);
    let status = source
        .status(claim.effect_id, Some(acked.receipt))
        .await
        .expect("independent source Effect outcome observation");
    assert_eq!(
        status.output,
        Some(EffectStatus {
            state: EffectState::Delivered,
            attempt: 1,
            token_present: false,
            lease_until_ms: None,
            expires_at_ms: claim.expires_at_ms,
            result: Some(outcome.result().to_vec()),
        })
    );
    let settled = source
        .validate(vec![claim], acked.receipt)
        .await
        .expect("independent source Effect settlement observation");
    assert!(!settled.output);
    let remaining = source
        .claim(
            identity(296 + u64::from(after_dispatch), now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("source Effect queue observation");
    assert!(remaining.output.is_empty());
    let final_row = destination
        .query(None, select_effect_row())
        .await
        .expect("independent Effect duplicate observation");
    assert_eq!(final_row.output[1].rows, vec![vec![SqlValue::Integer(1)]]);

    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

fn select_effect_row() -> SqlBatch {
    SqlBatch {
        statements: vec![
            SqlStatement {
                sql: "SELECT payload FROM qualification_rows WHERE id = 90".into(),
                parameters: Vec::new(),
            },
            SqlStatement {
                sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
                parameters: Vec::new(),
            },
        ],
    }
}
