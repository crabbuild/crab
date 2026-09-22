use super::*;

pub(super) async fn verify(
    typed: &crab_cell_app::ApplicationHandle<fixture::ReferenceApplication>,
    peer_effect: &crab_cell_runtime::EffectPeerClient,
    sql: &crab_cell_runtime::SqlCell<fixture::ReferenceSql>,
    cron: &crab_cell_runtime::CronNamespace<fixture::ReferenceCron>,
    tenant: TenantId,
    application_id: ApplicationId,
    acknowledgement: Option<&Acknowledgement>,
) {
    if let Some(acknowledged) = &acknowledgement {
        let remaining_ms = acknowledged
            .cron_next_due_ms
            .saturating_sub(now_ms())
            .max(0);
        tokio::time::sleep(Duration::from_millis(
            u64::try_from(remaining_ms).expect("nonnegative Cron due wait"),
        ))
        .await;
        let cron_target = CellTarget::new(
            tenant,
            application_id,
            fixture::CRON_NAMESPACE,
            &partition_for_shard(0),
        )
        .expect("successor Cron target");
        let before_tick = cron
            .get(fixed_id(900_108), None)
            .await
            .expect("successor pre-Tick Cron read");
        let tick = typed
            .command::<MaintenanceTickCommand<fixture::ReferenceCron>>(
                &cron_target,
                identity(900_130, now_ms()),
                MaintenanceTickRequest {
                    expected_commit_sequence: before_tick.receipt.commit_sequence,
                },
            )
            .await
            .expect("successor Cron Tick");
        assert!(
            matches!(tick.output, MaintenanceTickOutcome::Applied { processed } if processed >= 1)
        );
        let after_tick = cron
            .get(fixed_id(900_108), Some(tick.receipt))
            .await
            .expect("successor fired Cron read");
        let CronQueryResult::Get(Some(fired)) = after_tick.output else {
            panic!("fired Cron schedule is absent");
        };
        assert_eq!(fired.generation, acknowledged.cron_generation);
        assert_eq!(fired.occurrence, 1);
        assert_eq!(fired.next_due_ms, acknowledged.cron_next_due_ms + 300_000);
        let repeated_tick = typed
            .command::<MaintenanceTickCommand<fixture::ReferenceCron>>(
                &cron_target,
                identity(900_131, now_ms()),
                MaintenanceTickRequest {
                    expected_commit_sequence: after_tick.receipt.commit_sequence,
                },
            )
            .await
            .expect("successor repeated Cron Tick");
        let repeated_schedule = cron
            .get(fixed_id(900_108), Some(repeated_tick.receipt))
            .await
            .expect("successor repeated Cron read");
        assert!(
            matches!(repeated_schedule.output, CronQueryResult::Get(Some(ref schedule)) if schedule.occurrence == 1)
        );
        let cron_effects = typed
            .effects::<fixture::ReferenceCron>(cron_target)
            .expect("successor Cron Effects");
        let claimed = cron_effects
            .claim(
                identity(900_132, now_ms()),
                EffectClaimRequest {
                    limit: 2,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("successor Cron Effect claim");
        assert_eq!(claimed.output.len(), 1);
        let effect = claimed.output[0].clone();
        assert_eq!(effect.attempt, 1);
        assert!(effect.created_sequence >= tick.receipt.commit_sequence);
        let destination = peer_effect
            .deliver(&effect, now_ms())
            .await
            .expect("successor Cron destination delivery");
        assert!(matches!(destination, StoredOutcome::Success { .. }));
        assert_eq!(
            peer_effect
                .deliver(&effect, now_ms())
                .await
                .expect("duplicate Cron destination delivery"),
            destination
        );
        assert_eq!(
            peer_effect
                .resolve(&effect, now_ms())
                .await
                .expect("successor Cron inbox resolution"),
            Resolution::Committed(destination.clone())
        );
        let destination_state = sql
            .query(
                None,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT schedule_id, generation, occurrence, scheduled_at_ms, payload FROM qualification_cron_invocations".into(),
                        parameters: Vec::new(),
                    }],
                },
            )
            .await
            .expect("successor Cron destination observation");
        assert_eq!(
            destination_state.output[0].rows,
            vec![vec![
                SqlValue::Blob(fixed_id(900_108).to_vec()),
                SqlValue::Integer(
                    i64::try_from(acknowledged.cron_generation).expect("generation bounds")
                ),
                SqlValue::Integer(1),
                SqlValue::Integer(acknowledged.cron_next_due_ms),
                SqlValue::Blob(CRON_PAYLOAD.to_vec()),
            ]]
        );
        assert!(destination_state.receipt.commit_sequence >= destination.commit_sequence());
        let acked = cron_effects
            .ack(
                identity(900_133, now_ms()),
                effect,
                destination.result().to_vec(),
            )
            .await
            .expect("successor Cron Effect ack");
        assert_eq!(acked.output, EffectLeaseOutcome::Delivered);
        assert!(
            cron_effects
                .claim(
                    identity(900_134, now_ms()),
                    EffectClaimRequest {
                        limit: 1,
                        lease_ms: 5_000
                    },
                )
                .await
                .expect("successor settled Cron claim")
                .output
                .is_empty()
        );
    } else {
        let absent = sql
            .query(
                None,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT COUNT(*) FROM qualification_cron_invocations".into(),
                        parameters: Vec::new(),
                    }],
                },
            )
            .await
            .expect("successor Cron destination absence");
        assert_eq!(absent.output[0].rows, vec![vec![SqlValue::Integer(0)]]);
    }
}
