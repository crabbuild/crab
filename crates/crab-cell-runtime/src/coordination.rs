use std::{collections::BTreeMap, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionKind {
    Command,
    Query,
    Resolve,
    Migration,
}

/// The adapter intent associated with one in-flight effect.
///
/// Keeping the intent in the pure state prevents a completion from one async
/// family (for example, a renewal) from releasing a different family that
/// happens to reuse its local effect identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoordinationEffect {
    Work(AdmissionKind),
    Hydration,
    Inventory,
    Publication,
    Proof,
    Renewal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectReason {
    NotActive,
    Fenced,
    Draining,
    Busy,
    PublicationPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoordinationInput {
    Admit {
        kind: AdmissionKind,
        admission_matches: bool,
    },
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "pure lookup transition is exercised by the simulator"
        )
    )]
    Lookup,
    BeginWork {
        kind: AdmissionKind,
        publisher_ready: bool,
    },
    /// Schedules the next adapter action from actor-owned queue observations.
    ///
    /// The actor supplies queue/publisher observations; lifecycle, busy,
    /// renewal, and fencing policy remains owned by this pure state machine.
    Schedule {
        queue_empty: bool,
        publisher_ready: bool,
        publication_blocked: bool,
        lease_live: bool,
    },
    CompleteEffect {
        effect_id: u64,
        effect: CoordinationEffect,
    },
    FinishWork {
        fenced: bool,
    },
    BeginDrain,
    BeginShutdown,
    BeginMigration,
    FinishMigration {
        fenced: bool,
    },
    BeginRenewal {
        queue_empty: bool,
        publication_idle: bool,
        lease_live: bool,
    },
    FinishRenewal {
        fenced: bool,
    },
    BeginPublication,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "caller cancellation is exercised by the pure transition tests"
        )
    )]
    CallerCancel,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "follower proof is exercised by the pure transition tests"
        )
    )]
    FollowerProof {
        accepted: bool,
    },
    FinishPublication {
        fenced: bool,
        succeeded: bool,
    },
    BeginHydration {
        queue_empty: bool,
        publication_idle: bool,
        lease_live: bool,
    },
    BeginInventory {
        queue_empty: bool,
        publication_idle: bool,
        inventory_unknown: bool,
        refreshing: bool,
        lease_live: bool,
    },
    FinishHydration {
        complete: bool,
        stale: bool,
    },
    Fence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoordinationDecision {
    Admit,
    ResolveUnknown,
    LocalHandle,
    Reject(RejectReason),
    Started,
    EffectCompleted,
    StaleEffect,
    Ignored,
    ReadyToDeactivate,
    StartQueuedWork,
    Fence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Serving,
    Fenced,
    Draining,
    Migrating,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Residency {
    Sparse,
    Hydrating,
    Resident,
}

/// Pure volatile protocol state for one active Cell.
///
/// The state owns lifecycle and in-flight flags. It does not contain Tokio,
/// storage, clocks, SQL handles, bytes, or provider errors; `actor.rs` maps
/// its decisions to those effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoordinationState {
    lifecycle: Lifecycle,
    busy: bool,
    renewing: bool,
    publications: usize,
    follower_proof: bool,
    residency: Residency,
    shutdown_requested: bool,
    next_effect_id: u64,
    pending_effects: BTreeMap<u64, CoordinationEffect>,
}

impl CoordinationState {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "test and simulator constructor for the canonical kernel"
        )
    )]
    pub(crate) const fn serving(_publisher_ready: bool) -> Self {
        Self {
            lifecycle: Lifecycle::Serving,
            busy: false,
            renewing: false,
            publications: 0,
            follower_proof: false,
            residency: Residency::Resident,
            shutdown_requested: false,
            next_effect_id: 0,
            pending_effects: BTreeMap::new(),
        }
    }

    pub(crate) const fn serving_with_residency(
        _publisher_ready: bool,
        residency: Residency,
    ) -> Self {
        Self {
            lifecycle: Lifecycle::Serving,
            busy: false,
            renewing: false,
            publications: 0,
            follower_proof: false,
            residency,
            shutdown_requested: false,
            next_effect_id: 0,
            pending_effects: BTreeMap::new(),
        }
    }

    pub(crate) fn is_fenced(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Fenced)
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.shutdown_requested
            || matches!(
                self.lifecycle,
                Lifecycle::Draining | Lifecycle::Migrating | Lifecycle::Shutdown
            )
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown_requested || matches!(self.lifecycle, Lifecycle::Shutdown)
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    pub(crate) fn is_renewing(&self) -> bool {
        self.renewing
    }

    pub(crate) fn publication_count(&self) -> usize {
        self.publications
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "follower proof is asserted by the pure transition tests"
        )
    )]
    pub(crate) fn follower_proof(&self) -> bool {
        self.follower_proof
    }

    pub(crate) fn residency(&self) -> Residency {
        self.residency
    }

    /// Allocates a collision-free effect identity for one active generation.
    ///
    /// The identity is local to this coordination state. Completions must be
    /// submitted through `CompleteEffect` with the same intent; unknown,
    /// mismatched, or duplicate identities cannot release a newer operation.
    pub(crate) fn begin_effect(&mut self, effect: CoordinationEffect) -> u64 {
        loop {
            self.next_effect_id = self.next_effect_id.wrapping_add(1).max(1);
            if !self.pending_effects.contains_key(&self.next_effect_id) {
                self.pending_effects.insert(self.next_effect_id, effect);
                return self.next_effect_id;
            }
        }
    }

    pub(crate) fn effect_matches(&self, effect_id: u64, effect: CoordinationEffect) -> bool {
        self.pending_effects.get(&effect_id) == Some(&effect)
    }

    pub(crate) fn lookup(&self) -> CoordinationDecision {
        if self.is_fenced() {
            CoordinationDecision::Reject(RejectReason::Fenced)
        } else if self.is_draining() {
            CoordinationDecision::Reject(RejectReason::Draining)
        } else {
            CoordinationDecision::LocalHandle
        }
    }

    pub(crate) fn can_deactivate(&self) -> bool {
        !self.busy && !self.renewing && self.publications == 0 && self.pending_effects.is_empty()
    }

    /// Combines kernel-owned quiescence with adapter observations needed to
    /// close a Cell. The adapter may report queue and publisher state, but it
    /// cannot reimplement the lifecycle predicates above.
    pub(crate) fn ready_to_deactivate(&self, queue_empty: bool, publisher_ready: bool) -> bool {
        queue_empty && publisher_ready && self.can_deactivate()
    }

    pub(crate) fn step(&mut self, input: CoordinationInput) -> CoordinationDecision {
        match input {
            CoordinationInput::Admit {
                kind,
                admission_matches,
            } => self.admit(kind, admission_matches),
            CoordinationInput::Lookup => self.lookup(),
            CoordinationInput::BeginWork {
                kind,
                publisher_ready,
            } => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.busy {
                    CoordinationDecision::Reject(RejectReason::Busy)
                } else if matches!(kind, AdmissionKind::Migration)
                    && (self.publications != 0 || !publisher_ready)
                {
                    CoordinationDecision::Reject(RejectReason::PublicationPending)
                } else {
                    self.busy = true;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::Schedule {
                queue_empty,
                publisher_ready,
                publication_blocked,
                lease_live,
            } => {
                let can_deactivate = self.ready_to_deactivate(queue_empty, publisher_ready);
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    self.renewing = false;
                    if self.ready_to_deactivate(queue_empty, publisher_ready) {
                        CoordinationDecision::ReadyToDeactivate
                    } else {
                        CoordinationDecision::Fence
                    }
                } else if can_deactivate && (self.is_fenced() || self.is_draining()) && queue_empty
                {
                    CoordinationDecision::ReadyToDeactivate
                } else if self.is_fenced()
                    || self.busy
                    || self.renewing
                    || self
                        .pending_effects
                        .values()
                        .any(|effect| matches!(effect, CoordinationEffect::Inventory))
                    || queue_empty
                    || publication_blocked
                {
                    CoordinationDecision::Ignored
                } else {
                    CoordinationDecision::StartQueuedWork
                }
            }
            CoordinationInput::CompleteEffect { effect_id, effect } => {
                if self.pending_effects.get(&effect_id) == Some(&effect) {
                    self.pending_effects.remove(&effect_id);
                    CoordinationDecision::EffectCompleted
                } else {
                    CoordinationDecision::StaleEffect
                }
            }
            CoordinationInput::FinishWork { fenced } => {
                self.busy = false;
                if fenced || self.is_fenced() {
                    self.lifecycle = Lifecycle::Fenced;
                    return CoordinationDecision::Fence;
                }
                if self.can_deactivate() && self.is_draining() {
                    CoordinationDecision::ReadyToDeactivate
                } else {
                    CoordinationDecision::Ignored
                }
            }
            CoordinationInput::BeginDrain => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else {
                    self.lifecycle = Lifecycle::Draining;
                    if self.can_deactivate() {
                        CoordinationDecision::ReadyToDeactivate
                    } else {
                        CoordinationDecision::Started
                    }
                }
            }
            CoordinationInput::BeginShutdown => {
                if self.is_shutdown() {
                    CoordinationDecision::Ignored
                } else if self.is_fenced() {
                    self.shutdown_requested = true;
                    if self.can_deactivate() {
                        CoordinationDecision::ReadyToDeactivate
                    } else {
                        CoordinationDecision::Started
                    }
                } else if matches!(self.lifecycle, Lifecycle::Migrating) {
                    self.shutdown_requested = true;
                    CoordinationDecision::Started
                } else {
                    self.shutdown_requested = true;
                    self.lifecycle = Lifecycle::Shutdown;
                    if self.can_deactivate() {
                        CoordinationDecision::ReadyToDeactivate
                    } else {
                        CoordinationDecision::Started
                    }
                }
            }
            CoordinationInput::BeginMigration => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else {
                    self.lifecycle = Lifecycle::Migrating;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::FinishMigration { fenced } => {
                self.busy = false;
                if fenced || self.is_fenced() {
                    self.lifecycle = Lifecycle::Fenced;
                    return CoordinationDecision::Fence;
                }
                if matches!(self.lifecycle, Lifecycle::Migrating) {
                    self.lifecycle = if self.shutdown_requested {
                        Lifecycle::Shutdown
                    } else {
                        Lifecycle::Serving
                    };
                    if self.can_deactivate() && self.is_draining() {
                        return CoordinationDecision::ReadyToDeactivate;
                    }
                }
                CoordinationDecision::Ignored
            }
            CoordinationInput::BeginRenewal {
                queue_empty,
                publication_idle,
                lease_live,
            } => {
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    self.renewing = false;
                    CoordinationDecision::Fence
                } else if self.is_fenced() || self.is_draining() || self.renewing {
                    CoordinationDecision::Reject(if self.is_fenced() {
                        RejectReason::Fenced
                    } else {
                        RejectReason::Draining
                    })
                } else if self.busy || !queue_empty || !publication_idle {
                    CoordinationDecision::Ignored
                } else {
                    self.renewing = true;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::FinishRenewal { fenced } => {
                self.renewing = false;
                if fenced || self.is_fenced() {
                    self.lifecycle = Lifecycle::Fenced;
                    CoordinationDecision::Fence
                } else {
                    CoordinationDecision::Ignored
                }
            }
            CoordinationInput::BeginPublication => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() && !self.busy {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else {
                    self.publications = self.publications.saturating_add(1);
                    self.follower_proof = false;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::CallerCancel => CoordinationDecision::Ignored,
            CoordinationInput::FollowerProof { accepted } => {
                if accepted && self.publications != 0 {
                    self.follower_proof = true;
                }
                CoordinationDecision::Ignored
            }
            CoordinationInput::FinishPublication { fenced, succeeded } => {
                self.publications = self.publications.saturating_sub(1);
                let succeeded = succeeded || self.follower_proof;
                self.follower_proof = false;
                if fenced || !succeeded {
                    self.lifecycle = Lifecycle::Fenced;
                    return CoordinationDecision::Fence;
                }
                if self.can_deactivate() && self.is_draining() {
                    CoordinationDecision::ReadyToDeactivate
                } else {
                    CoordinationDecision::Ignored
                }
            }
            CoordinationInput::BeginHydration {
                queue_empty,
                publication_idle,
                lease_live,
            } => {
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    CoordinationDecision::Fence
                } else if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.busy {
                    CoordinationDecision::Reject(RejectReason::Busy)
                } else if !queue_empty || !publication_idle || self.residency != Residency::Sparse {
                    CoordinationDecision::Ignored
                } else {
                    self.busy = true;
                    self.residency = Residency::Hydrating;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::BeginInventory {
                queue_empty,
                publication_idle,
                inventory_unknown,
                refreshing,
                lease_live,
            } => {
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    self.renewing = false;
                    CoordinationDecision::Fence
                } else if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.busy {
                    CoordinationDecision::Reject(RejectReason::Busy)
                } else if !queue_empty || !publication_idle || !inventory_unknown || refreshing {
                    CoordinationDecision::Ignored
                } else {
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::FinishHydration { complete, stale } => {
                self.busy = false;
                if !stale {
                    self.residency = if complete {
                        Residency::Resident
                    } else {
                        Residency::Sparse
                    };
                } else {
                    self.residency = Residency::Sparse;
                    return CoordinationDecision::Fence;
                }
                CoordinationDecision::Ignored
            }
            CoordinationInput::Fence => {
                self.lifecycle = Lifecycle::Fenced;
                self.busy = false;
                self.renewing = false;
                CoordinationDecision::Ignored
            }
        }
    }

    fn admit(&self, kind: AdmissionKind, admission_matches: bool) -> CoordinationDecision {
        if !admission_matches {
            return CoordinationDecision::Reject(RejectReason::NotActive);
        }
        if self.is_fenced() {
            return if matches!(kind, AdmissionKind::Resolve) {
                CoordinationDecision::ResolveUnknown
            } else {
                CoordinationDecision::Reject(RejectReason::Fenced)
            };
        }
        // Migration swaps the admission capability before its durable cut is
        // published. The successor capability may queue work, but `busy` keeps
        // it from executing until `FinishMigration` returns to Serving.
        if matches!(self.lifecycle, Lifecycle::Migrating) && !self.shutdown_requested {
            return CoordinationDecision::Admit;
        }
        if self.is_draining() {
            return CoordinationDecision::Reject(RejectReason::Draining);
        }
        CoordinationDecision::Admit
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotActive => "not active",
            Self::Fenced => "fenced",
            Self::Draining => "draining",
            Self::Busy => "busy",
            Self::PublicationPending => "publication pending",
        };
        formatter.write_str(text)
    }
}

#[cfg(test)]
mod tests {
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
            CoordinationDecision::ReadyToDeactivate
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
}
