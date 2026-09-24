//! Publication outcomes, CAS loss, durability proofs, and compaction.

use super::*;

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
