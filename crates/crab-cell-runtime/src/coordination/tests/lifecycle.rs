//! Admission, fencing, drain, cancellation, and shutdown transitions.

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
