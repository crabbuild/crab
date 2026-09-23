use super::*;

pub(super) async fn verify(
    typed: &crab_cell_app::ApplicationHandle<fixture::ReferenceApplication>,
    direct: &CellClient,
    peer_effect: &crab_cell_runtime::peer::EffectPeerClient,
    sql: &crab_cell_runtime::SqlCell<fixture::ReferenceSql>,
    workflow: &crab_cell_runtime::WorkflowNamespace<fixture::ReferenceWorkflow>,
    tenant: TenantId,
    application_id: ApplicationId,
    acknowledgement: Option<&Acknowledgement>,
) {
    let effect_state = workflow
        .state(EFFECT_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("successor Effect workflow read");
    if let Some(acknowledged) = &acknowledgement {
        let run = effect_state.output.expect("acknowledged Effect workflow");
        assert_eq!(run.run_id, acknowledged.effect_run_id);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert_eq!(
            run.result.as_deref(),
            Some(b"effect-valid-scheduled".as_slice())
        );
        assert!(effect_state.receipt.commit_sequence >= acknowledged.effect_sequence);
    } else {
        assert!(
            effect_state.output.is_none(),
            "unacknowledged Effect workflow appeared"
        );
    }
    let effect_target = CellTarget::new(
        tenant,
        application_id,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("successor Effect target");
    let effects = typed
        .effects::<fixture::ReferenceWorkflow>(effect_target.clone())
        .expect("successor Effects");
    let claimed = if acknowledgement
        .as_ref()
        .and_then(|ack| ack.settlements.as_ref())
        .is_some()
    {
        effects
            .claim(
                identity(900_142, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("successor settled Effect claim")
    } else if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref()) {
        let stale = direct
            .command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
                &effect_target,
                identity(900_124, now_ms()),
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease {
                        effect_id: leases.effect_id,
                        attempt: leases.effect_attempt,
                        token: leases.effect_token,
                        expires_at_ms: leases.effect_expires_at_ms,
                    },
                    result: EFFECT_RESULT.to_vec(),
                }),
            )
            .await;
        assert!(matches!(
            stale,
            Err(InvocationError::Rejected(outcome))
                if outcome.output == EffectLeaseOutcome::LeaseLost
        ));
        let mut reclaimed = None;
        for attempt in 0..4 {
            let next = effects
                .claim(
                    identity(900_125 + attempt, now_ms()),
                    EffectClaimRequest {
                        limit: 1,
                        lease_ms: 30_000,
                    },
                )
                .await
                .expect("successor Effect reclaim");
            if !next.output.is_empty() {
                reclaimed = Some(next);
                break;
            }
            // Expired source leases become ready after their bounded retry delay.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        reclaimed.expect("successor Effect became claimable")
    } else {
        effects
            .claim(
                identity(900_112, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("successor Effect claim")
    };
    if let Some(settlements) = acknowledgement
        .as_ref()
        .and_then(|ack| ack.settlements.as_ref())
    {
        let acknowledged = acknowledgement.as_ref().expect("Effect acknowledgement");
        let replayed = typed
            .command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
                &effect_target,
                replay_identity(900_140, acknowledged.replay_issued_at_ms),
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease {
                        effect_id: settlements.effect_id,
                        attempt: settlements.effect_attempt,
                        token: settlements.effect_token,
                        expires_at_ms: settlements.effect_expires_at_ms,
                    },
                    result: settlements.effect_result.clone(),
                }),
            )
            .await
            .expect("recovered Effect acknowledgement replay");
        assert_eq!(replayed.output, EffectLeaseOutcome::Delivered);
        assert_eq!(
            replayed.receipt.commit_sequence,
            settlements.effect_ack_sequence
        );
        assert!(claimed.output.is_empty(), "settled Effect became claimable");
        assert!(claimed.receipt.commit_sequence >= settlements.effect_ack_sequence);
        let destination_state = sql
            .query(
                None,
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
                },
            )
            .await
            .expect("successor restored Effect destination");
        assert_eq!(
            destination_state.output[0].rows,
            vec![vec![SqlValue::Blob(EFFECT_RESULT.to_vec())]]
        );
        assert_eq!(
            destination_state.output[1].rows,
            vec![vec![SqlValue::Integer(1)]]
        );
        assert!(
            destination_state.receipt.commit_sequence >= settlements.effect_destination_sequence
        );
    } else if acknowledgement.is_some() {
        assert_eq!(claimed.output.len(), 1);
        let effect = claimed.output[0].clone();
        if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref()) {
            assert!(claimed.receipt.commit_sequence >= leases.effect_sequence);
            assert_eq!(effect.effect_id, leases.effect_id);
            assert_eq!(effect.attempt, leases.effect_attempt + 1);
            assert_ne!(effect.token, leases.effect_token);
        } else {
            assert_eq!(effect.attempt, 1);
        }
        assert!(
            effects
                .validate(vec![effect.clone()], claimed.receipt)
                .await
                .expect("successor Effect validation")
                .output
        );
        let destination = peer_effect
            .deliver(&effect, now_ms())
            .await
            .expect("successor destination Effect delivery");
        assert!(matches!(destination, StoredOutcome::Success { .. }));
        let replay = peer_effect
            .deliver(&effect, now_ms())
            .await
            .expect("duplicate destination Effect delivery");
        assert_eq!(replay, destination);
        assert_eq!(
            peer_effect
                .resolve(&effect, now_ms())
                .await
                .expect("successor destination inbox resolution"),
            Resolution::Committed(destination.clone())
        );
        let destination_state = sql
            .query(
                None,
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
                },
            )
            .await
            .expect("successor destination Effect observation");
        assert_eq!(
            destination_state.output[0].rows,
            vec![vec![SqlValue::Blob(EFFECT_RESULT.to_vec())]]
        );
        assert_eq!(
            destination_state.output[1].rows,
            vec![vec![SqlValue::Integer(1)]]
        );
        assert!(destination_state.receipt.commit_sequence >= destination.commit_sequence());
        let acked = effects
            .ack(
                identity(900_113, now_ms()),
                effect.clone(),
                destination.result().to_vec(),
            )
            .await
            .expect("successor Effect ack");
        assert_eq!(acked.output, EffectLeaseOutcome::Delivered);
        assert!(
            !effects
                .validate(vec![effect], acked.receipt)
                .await
                .expect("successor Effect settlement")
                .output
        );
        assert!(
            effects
                .claim(
                    identity(900_114, now_ms()),
                    EffectClaimRequest {
                        limit: 1,
                        lease_ms: 5_000,
                    },
                )
                .await
                .expect("successor Effect settled claim")
                .output
                .is_empty()
        );
    } else {
        assert!(claimed.output.is_empty(), "unacknowledged Effect appeared");
        let absent = sql
            .query(
                None,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
                        parameters: Vec::new(),
                    }],
                },
            )
            .await
            .expect("successor destination Effect absence");
        assert_eq!(absent.output[0].rows, vec![vec![SqlValue::Integer(0)]]);
    }
}
