use super::*;

#[test]
fn serving_cell_admits_matching_work_and_local_lookup() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Command,
            admission_matches: true,
        }),
        CoordinationDecision::Admit
    );
    assert_eq!(
        state.step(CoordinationInput::Lookup),
        CoordinationDecision::LocalHandle
    );
}

#[test]
fn fence_rejects_new_work_and_returns_unknown_resolution() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::Fence);
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Command,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Fenced)
    );
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Resolve,
            admission_matches: true,
        }),
        CoordinationDecision::ResolveUnknown
    );
    assert_eq!(
        state.step(CoordinationInput::Lookup),
        CoordinationDecision::Reject(RejectReason::Fenced)
    );
}

#[test]
fn drain_closes_admission_but_finishes_existing_work() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Command,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::BeginDrain),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Query,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Draining)
    );
    assert_eq!(
        state.step(CoordinationInput::FinishWork { fenced: false }),
        CoordinationDecision::ReadyToDeactivate
    );
}

#[test]
fn fenced_work_completion_returns_fence_without_actor_redeciding() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Query,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::FinishWork { fenced: true }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert!(!state.is_busy());
}

#[test]
fn scheduler_decision_stays_pure_and_respects_lifecycle_obligations() {
    let mut serving = CoordinationState::serving(true);
    assert_eq!(
        serving.step(CoordinationInput::Schedule {
            queue_empty: false,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::StartQueuedWork
    );

    let mut busy = CoordinationState::serving(true);
    busy.step(CoordinationInput::BeginWork {
        kind: AdmissionKind::Query,
        publisher_ready: true,
    });
    assert_eq!(
        busy.step(CoordinationInput::Schedule {
            queue_empty: false,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );

    let mut draining = CoordinationState::serving(true);
    draining.step(CoordinationInput::BeginDrain);
    assert_eq!(
        draining.step(CoordinationInput::Schedule {
            queue_empty: true,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::ReadyToDeactivate
    );
}

#[test]
fn scheduler_blocks_publication_pressure_without_leaking_adapter_policy() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: false,
            publisher_ready: true,
            publication_blocked: true,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );
    assert!(!state.is_busy());
}

#[test]
fn scheduler_requires_publisher_observation_before_deactivation() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginDrain);
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: true,
            publisher_ready: false,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: true,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::ReadyToDeactivate
    );
}

#[test]
fn scheduler_fences_before_dispatch_when_the_node_lease_is_lost() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: false,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: false,
        }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert!(!state.is_busy());
}

#[test]
fn fenced_scheduler_can_release_after_all_local_obligations_drain() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: true,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: false,
        }),
        CoordinationDecision::ReadyToDeactivateFenced
    );
    assert!(state.is_fenced());
}

#[test]
fn background_effects_wait_for_foreground_and_publication_quiescence() {
    let mut renewal = CoordinationState::serving(true);
    renewal.step(CoordinationInput::BeginWork {
        kind: AdmissionKind::Query,
        publisher_ready: true,
    });
    assert_eq!(
        renewal.step(CoordinationInput::BeginRenewal {
            queue_empty: true,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );

    let mut hydration = CoordinationState::serving_with_residency(true, Residency::Sparse);
    assert_eq!(
        hydration.step(CoordinationInput::BeginHydration {
            queue_empty: false,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );
    assert_eq!(hydration.residency(), Residency::Sparse);
}

#[test]
fn inventory_refresh_requires_quiescence_and_live_lease() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginInventory {
            queue_empty: false,
            publication_idle: true,
            inventory_unknown: true,
            refreshing: false,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );
    assert_eq!(
        state.step(CoordinationInput::BeginInventory {
            queue_empty: true,
            publication_idle: true,
            inventory_unknown: true,
            refreshing: false,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    state.begin_effect(CoordinationEffect::Inventory);
    assert_eq!(
        state.step(CoordinationInput::Schedule {
            queue_empty: false,
            publisher_ready: true,
            publication_blocked: false,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );

    let mut fenced = CoordinationState::serving(true);
    assert_eq!(
        fenced.step(CoordinationInput::BeginInventory {
            queue_empty: true,
            publication_idle: true,
            inventory_unknown: true,
            refreshing: false,
            lease_live: false,
        }),
        CoordinationDecision::Fence
    );
    assert!(fenced.is_fenced());
}

#[test]
fn transfer_preflight_requires_a_quiescent_live_owner() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginTransferPreflight {
            queue_empty: false,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    assert!(state.is_transfer_preparing());
    assert_eq!(
        state.step(CoordinationInput::BeginCompaction {
            queue_empty: true,
            publication_idle: true,
            publisher_ready: true,
            due: true,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Command,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Draining)
    );
    assert_eq!(
        state.step(CoordinationInput::AbortTransfer),
        CoordinationDecision::Ignored
    );
    assert!(!state.is_transfer_preparing());
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Command,
            admission_matches: true,
        }),
        CoordinationDecision::Admit
    );

    let mut refreshing = CoordinationState::serving(true);
    refreshing.begin_effect(CoordinationEffect::Inventory);
    assert_eq!(
        refreshing.step(CoordinationInput::BeginTransferPreflight {
            queue_empty: true,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Ignored
    );

    let mut fenced = CoordinationState::serving(true);
    assert_eq!(
        fenced.step(CoordinationInput::BeginTransferPreflight {
            queue_empty: true,
            publication_idle: true,
            lease_live: false,
        }),
        CoordinationDecision::Fence
    );
}

#[test]
fn transfer_confirmation_enters_terminal_drain_and_fence_wins() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginTransferPreflight {
            queue_empty: true,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::ConfirmTransfer),
        CoordinationDecision::ReadyToDeactivate
    );
    assert!(state.is_draining());
    assert!(!state.is_transfer_preparing());
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Query,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Draining)
    );

    let mut fenced = CoordinationState::serving(true);
    fenced.step(CoordinationInput::BeginTransferPreflight {
        queue_empty: true,
        publication_idle: true,
        lease_live: true,
    });
    fenced.step(CoordinationInput::Fence);
    assert_eq!(
        fenced.step(CoordinationInput::ConfirmTransfer),
        CoordinationDecision::Reject(RejectReason::Fenced)
    );
    assert!(!fenced.is_transfer_preparing());
}

#[test]
fn publication_failure_fences_and_cannot_be_released_as_success() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginPublication);
    assert_eq!(state.publication_count(), 1);
    assert_eq!(
        state.step(CoordinationInput::FinishPublication {
            fenced: false,
            succeeded: false,
        }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert_eq!(state.publication_count(), 0);
}

#[test]
fn fenced_work_cannot_start_a_publication_after_completion() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginWork {
        kind: AdmissionKind::Command,
        publisher_ready: true,
    });
    assert_eq!(
        state.step(CoordinationInput::FinishWork { fenced: true }),
        CoordinationDecision::Fence
    );
    assert_eq!(
        state.step(CoordinationInput::BeginPublication),
        CoordinationDecision::Reject(RejectReason::Fenced)
    );
}

#[test]
fn lost_cas_fences_the_owner_and_releases_the_publication_obligation() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginPublication);
    assert_eq!(
        state.step(CoordinationInput::FinishPublication {
            fenced: true,
            succeeded: false,
        }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert_eq!(state.publication_count(), 0);
    assert!(state.can_deactivate());
}

#[test]
fn caller_cancellation_does_not_cancel_accepted_work() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Command,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::CallerCancel),
        CoordinationDecision::Ignored
    );
    assert!(state.is_busy());
}

#[test]
fn accepted_follower_proof_satisfies_publication_durability() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginPublication);
    state.step(CoordinationInput::FollowerProof { accepted: true });
    assert!(state.follower_proof());
    state.step(CoordinationInput::FinishPublication {
        fenced: false,
        succeeded: false,
    });
    assert!(!state.is_fenced());
    assert_eq!(state.publication_count(), 0);
}

#[test]
fn stale_renewal_completion_is_not_a_new_admission() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginRenewal {
            queue_empty: true,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    state.step(CoordinationInput::Fence);
    assert_eq!(
        state.step(CoordinationInput::FinishRenewal { fenced: false }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert!(!state.is_renewing());
}

#[test]
fn hydration_promotion_is_bounded_and_fenced_on_failure() {
    let mut state = CoordinationState::serving_with_residency(true, Residency::Sparse);
    assert_eq!(
        state.step(CoordinationInput::BeginHydration {
            queue_empty: true,
            publication_idle: true,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(state.residency(), Residency::Hydrating);
    state.step(CoordinationInput::FinishHydration {
        complete: true,
        stale: false,
    });
    assert_eq!(state.residency(), Residency::Resident);

    let mut failed = CoordinationState::serving_with_residency(true, Residency::Sparse);
    failed.step(CoordinationInput::BeginHydration {
        queue_empty: true,
        publication_idle: true,
        lease_live: true,
    });
    assert_eq!(
        failed.step(CoordinationInput::FinishHydration {
            complete: false,
            stale: true,
        }),
        CoordinationDecision::Fence
    );
    failed.step(CoordinationInput::Fence);
    assert!(failed.is_fenced());
    assert_eq!(failed.residency(), Residency::Sparse);
}

#[test]
fn migration_queues_successor_work_until_publication_drains() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginPublication);
    assert_eq!(
        state.step(CoordinationInput::BeginMigration),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Query,
            admission_matches: true,
        }),
        CoordinationDecision::Admit
    );
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Migration,
            publisher_ready: true,
        }),
        CoordinationDecision::Reject(RejectReason::PublicationPending)
    );
}

#[test]
fn migration_requires_a_live_publisher_observation() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginMigration);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Migration,
            publisher_ready: false,
        }),
        CoordinationDecision::Reject(RejectReason::PublicationPending)
    );
}

#[test]
fn admitted_work_may_publish_after_drain_begins() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Command,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    state.step(CoordinationInput::BeginDrain);
    assert_eq!(
        state.step(CoordinationInput::BeginPublication),
        CoordinationDecision::Started
    );
    state.step(CoordinationInput::FinishWork { fenced: false });
    assert_eq!(state.publication_count(), 1);
    state.step(CoordinationInput::FinishPublication {
        fenced: false,
        succeeded: true,
    });
    assert_eq!(state.publication_count(), 0);
}

#[test]
fn queued_work_can_finish_after_shutdown_closes_new_admission() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginShutdown);
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Query,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Query,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Draining)
    );
    assert_eq!(
        state.step(CoordinationInput::FinishWork { fenced: false }),
        CoordinationDecision::ReadyToDeactivate
    );
}

#[test]
fn shutdown_supersedes_migration_and_keeps_the_drain_terminal() {
    let mut state = CoordinationState::serving(true);
    state.step(CoordinationInput::BeginMigration);
    assert_eq!(
        state.step(CoordinationInput::BeginShutdown),
        CoordinationDecision::Started
    );
    assert!(state.is_shutdown());
    assert!(matches!(
        state.step(CoordinationInput::Admit {
            kind: AdmissionKind::Query,
            admission_matches: true,
        }),
        CoordinationDecision::Reject(RejectReason::Draining)
    ));
    assert_eq!(
        state.step(CoordinationInput::FinishMigration { fenced: false }),
        CoordinationDecision::ReadyToDeactivate
    );
    assert!(state.is_shutdown());
}

#[test]
fn fenced_migration_completion_does_not_reopen_serving_state() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginMigration),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::BeginWork {
            kind: AdmissionKind::Migration,
            publisher_ready: true,
        }),
        CoordinationDecision::Started
    );
    assert_eq!(
        state.step(CoordinationInput::FinishMigration { fenced: true }),
        CoordinationDecision::Fence
    );
    assert!(state.is_fenced());
    assert!(!state.is_busy());
}

#[test]
fn effect_completion_is_exact_and_duplicate_completions_are_stale() {
    let mut state = CoordinationState::serving(true);
    let first = state.begin_effect(CoordinationEffect::Publication);
    let second = state.begin_effect(CoordinationEffect::Proof);
    assert_ne!(first, second);
    assert!(state.effect_matches(first, CoordinationEffect::Publication));
    assert!(state.effect_matches(second, CoordinationEffect::Proof));

    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id: first,
            effect: CoordinationEffect::Publication,
        }),
        CoordinationDecision::EffectCompleted
    );
    assert!(!state.effect_matches(first, CoordinationEffect::Publication));
    assert!(state.effect_matches(second, CoordinationEffect::Proof));
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id: first,
            effect: CoordinationEffect::Publication,
        }),
        CoordinationDecision::StaleEffect
    );
    assert!(state.effect_matches(second, CoordinationEffect::Proof));
}

#[test]
fn mismatched_effect_family_cannot_release_a_pending_operation() {
    let mut state = CoordinationState::serving(true);
    let effect_id = state.begin_effect(CoordinationEffect::Publication);
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id,
            effect: CoordinationEffect::Renewal,
        }),
        CoordinationDecision::StaleEffect
    );
    assert!(state.effect_matches(effect_id, CoordinationEffect::Publication));
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id,
            effect: CoordinationEffect::Publication,
        }),
        CoordinationDecision::EffectCompleted
    );
}

#[test]
fn deactivation_waits_for_effect_completion_owned_by_the_kernel() {
    let mut state = CoordinationState::serving(true);
    let effect_id = state.begin_effect(CoordinationEffect::Renewal);
    state.step(CoordinationInput::BeginDrain);
    assert!(!state.can_deactivate());
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id,
            effect: CoordinationEffect::Renewal,
        }),
        CoordinationDecision::EffectCompleted
    );
    assert!(state.can_deactivate());
}

#[test]
fn deactivation_requires_adapter_observations_without_repeating_policy() {
    let state = CoordinationState::serving(true);
    assert!(!state.ready_to_deactivate(false, true));
    assert!(!state.ready_to_deactivate(true, false));
    assert!(state.ready_to_deactivate(true, true));
}

#[test]
fn compaction_waits_for_quiet_publication_and_blocks_drain_until_completion() {
    let mut state = CoordinationState::serving(true);
    let begin = |state: &mut CoordinationState, queue_empty, publication_idle| {
        state.step(CoordinationInput::BeginCompaction {
            queue_empty,
            publication_idle,
            publisher_ready: true,
            due: true,
            lease_live: true,
        })
    };
    assert_eq!(
        begin(&mut state, false, true),
        CoordinationDecision::Ignored
    );
    assert_eq!(
        begin(&mut state, true, false),
        CoordinationDecision::Ignored
    );
    assert_eq!(begin(&mut state, true, true), CoordinationDecision::Started);
    let effect = state.begin_effect(CoordinationEffect::Compaction);
    assert_eq!(
        state.step(CoordinationInput::BeginDrain),
        CoordinationDecision::Started
    );
    assert!(!state.ready_to_deactivate(true, false));
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id: effect,
            effect: CoordinationEffect::Compaction,
        }),
        CoordinationDecision::EffectCompleted
    );
    assert_eq!(
        state.step(CoordinationInput::FinishCompaction { fenced: false }),
        CoordinationDecision::ReadyToDeactivate
    );
}

#[test]
fn lease_loss_during_compaction_fences_without_releasing_the_effect() {
    let mut state = CoordinationState::serving(true);
    assert_eq!(
        state.step(CoordinationInput::BeginCompaction {
            queue_empty: true,
            publication_idle: true,
            publisher_ready: true,
            due: true,
            lease_live: true,
        }),
        CoordinationDecision::Started
    );
    let effect = state.begin_effect(CoordinationEffect::Compaction);
    assert_eq!(
        state.step(CoordinationInput::FinishCompaction { fenced: true }),
        CoordinationDecision::Fence
    );
    assert!(!state.ready_to_deactivate(true, true));
    assert_eq!(
        state.step(CoordinationInput::CompleteEffect {
            effect_id: effect,
            effect: CoordinationEffect::Compaction,
        }),
        CoordinationDecision::EffectCompleted
    );
    assert!(state.ready_to_deactivate(true, true));
}
