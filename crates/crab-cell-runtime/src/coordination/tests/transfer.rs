//! Transfer preflight, hydration, and migration transitions.

use super::*;

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
