//! Effect completion, deactivation, and adapter observations.

use super::*;

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
