use std::{sync::atomic::Ordering, time::Duration};

use crab_cell_runtime::Error;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::identity::{CellTarget, partition_for_shard};
use crab_cell_runtime::primitives::effects::{
    EffectClaimRequest, EffectLeaseOutcome, EffectState, EffectStatus,
};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::primitives::workflow::{WorkflowOutcome, WorkflowStatus};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(
    dead_code,
    reason = "this test uses only delayed signed Effect delivery"
)]
mod qualification_peer;

use qualification::{assert_zero_reservations, identity, rustfs_public_store};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_effect_client_with_delayed_receive;

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_expired_effect_delivery_never_reaches_destination() {
    run_expired_delivery(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_expired_effect_delivery_never_reaches_destination() {
    let (store, root) = rustfs_public_store();
    run_expired_delivery(public_host_fixture_with_store(store, root).await).await;
}

async fn run_expired_delivery(
    (node, writer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let observer = node
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(registry.clone(), handles.clone())
                .expect("separate typed observer"),
            tenant,
            application,
        )
        .unwrap();
    let (peer, entered, release, dispatched) =
        peer_effect_client_with_delayed_receive(registry, handles);
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
    let workflow = writer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("writer Workflow");
    let source = writer
        .effects::<fixture::ReferenceWorkflow>(workflow_target.clone())
        .expect("writer Effect source");
    let observed_source = observer
        .effects::<fixture::ReferenceWorkflow>(workflow_target.clone())
        .expect("observer Effect source");
    let destination = observer
        .sql::<fixture::ReferenceSql>(sql_target)
        .expect("observer SQL destination");
    let workflow_id = b"delayed-expiring-effect".to_vec();
    let started = workflow
        .start(
            identity(400, now_ms()),
            workflow_id.clone(),
            b"effect-expiring".to_vec(),
        )
        .await
        .expect("acknowledged Effect publication");
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
            identity(401, now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 30_000,
            },
        )
        .await
        .expect("source Effect claim");
    let [claim] = claimed.output.as_slice() else {
        panic!("source Effect was not claimable before expiry");
    };
    assert_eq!(claim.attempt, 1);
    let claim = claim.clone();
    let effect_id = claim.effect_id;
    let effect_expires_at_ms = claim.expires_at_ms;
    assert!(
        observed_source
            .validate(vec![claim.clone()], claimed.receipt)
            .await
            .expect("independent live Effect observation")
            .output
    );

    let delivery = {
        let peer = peer.clone();
        let claim = claim.clone();
        tokio::spawn(async move { peer.deliver(&claim, now_ms()).await })
    };
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("signed Effect reached delayed receive boundary");
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    let before = destination
        .query(None, effect_row_count())
        .await
        .expect("independent pre-expiry destination observation");
    assert_eq!(before.output[0].rows, vec![vec![SqlValue::Integer(0)]]);
    let wait_ms = claim.expires_at_ms.saturating_sub(now_ms()).max(0) + 100;
    tokio::time::sleep(Duration::from_millis(
        u64::try_from(wait_ms).expect("nonnegative Effect expiry wait"),
    ))
    .await;
    assert!(now_ms() > claim.expires_at_ms);
    release.notify_one();
    let outcome = delivery.await.expect("delayed Effect delivery task");
    assert!(matches!(
        outcome,
        Err(Error::Peer("invalid or expired effect identity"))
    ));
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    let stale_ack = source
        .ack(
            identity(402, now_ms()),
            claim.clone(),
            b"unpublished".to_vec(),
        )
        .await;
    assert!(matches!(
        stale_ack,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == EffectLeaseOutcome::LeaseLost
    ));
    assert!(
        !observed_source
            .validate(vec![claim], claimed.receipt)
            .await
            .expect("independent expired Effect observation")
            .output
    );
    let expired = observed_source
        .claim(
            identity(403, now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("expired source Effect observation");
    assert!(expired.output.is_empty());
    let ledger = observed_source
        .status(effect_id, Some(expired.receipt))
        .await
        .expect("independent source Effect ledger observation");
    assert_eq!(
        ledger.output,
        Some(EffectStatus {
            state: EffectState::Failed,
            attempt: 1,
            token_present: false,
            lease_until_ms: None,
            expires_at_ms: effect_expires_at_ms,
            result: None,
        })
    );
    let after = destination
        .query(None, effect_row_count())
        .await
        .expect("independent post-expiry destination observation");
    assert_eq!(after.output[0].rows, vec![vec![SqlValue::Integer(0)]]);
    let run = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow")
        .state(workflow_id, None)
        .await
        .expect("independent source Workflow observation")
        .output
        .expect("acknowledged source Workflow");
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 1);
    assert_eq!(run.result, Some(b"effect-valid-scheduled".to_vec()));

    drop(writer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

fn effect_row_count() -> SqlBatch {
    SqlBatch {
        statements: vec![SqlStatement {
            sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
            parameters: Vec::new(),
        }],
    }
}
