//! Kernel scheduling decisions and observation requirements.

use super::*;

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
