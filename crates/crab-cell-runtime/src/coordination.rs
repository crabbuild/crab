use std::{collections::BTreeMap, fmt};

#[cfg(test)]
mod sim;

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
    Compaction,
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
    BeginTransferPreflight {
        queue_empty: bool,
        publication_idle: bool,
        lease_live: bool,
    },
    ConfirmTransfer,
    AbortTransfer,
    BeginCompaction {
        queue_empty: bool,
        publication_idle: bool,
        publisher_ready: bool,
        due: bool,
        lease_live: bool,
    },
    FinishCompaction {
        fenced: bool,
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
    ReadyToDeactivateFenced,
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
    transfer_preparing: bool,
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
            transfer_preparing: false,
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
            transfer_preparing: false,
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

    pub(crate) fn is_transfer_preparing(&self) -> bool {
        self.transfer_preparing
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
        } else if self.is_draining() || self.transfer_preparing {
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
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    self.renewing = false;
                    if self.ready_to_deactivate(queue_empty, publisher_ready) {
                        CoordinationDecision::ReadyToDeactivateFenced
                    } else {
                        CoordinationDecision::Fence
                    }
                } else if (self.is_fenced() || self.is_draining())
                    && self.ready_to_deactivate(queue_empty, publisher_ready)
                {
                    if self.is_fenced() {
                        CoordinationDecision::ReadyToDeactivateFenced
                    } else {
                        CoordinationDecision::ReadyToDeactivate
                    }
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
                self.transfer_preparing = false;
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
                } else if self.is_fenced()
                    || self.is_draining()
                    || self.transfer_preparing
                    || self.renewing
                {
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
                } else if self.is_draining() || self.transfer_preparing {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.busy {
                    CoordinationDecision::Reject(RejectReason::Busy)
                } else if !queue_empty || !publication_idle || self.residency != Residency::Sparse {
                    CoordinationDecision::Ignored
                } else {
                    // Fetch owns an effect, not the foreground slot. The worker
                    // serializes installation with SQL; the effect keeps drain
                    // and transfer from releasing the activation during fetch.
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
                } else if self.is_draining() || self.transfer_preparing {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.busy {
                    CoordinationDecision::Reject(RejectReason::Busy)
                } else if !queue_empty || !publication_idle || !inventory_unknown || refreshing {
                    CoordinationDecision::Ignored
                } else {
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::BeginTransferPreflight {
                queue_empty: _,
                publication_idle: _,
                lease_live,
            } => {
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    self.busy = false;
                    self.renewing = false;
                    CoordinationDecision::Fence
                } else if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_draining() || self.transfer_preparing {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.renewing
                    || self.pending_effects.values().any(|effect| {
                        matches!(
                            effect,
                            CoordinationEffect::Hydration
                                | CoordinationEffect::Inventory
                                | CoordinationEffect::Compaction
                                | CoordinationEffect::Renewal
                        )
                    })
                {
                    CoordinationDecision::Ignored
                } else {
                    // Admission closes before these observations become quiescent so
                    // work already accepted by the actor can finish without loss.
                    self.transfer_preparing = true;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::ConfirmTransfer => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_shutdown() || self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if !self.transfer_preparing || !self.can_deactivate() {
                    CoordinationDecision::Ignored
                } else {
                    self.transfer_preparing = false;
                    self.lifecycle = Lifecycle::Draining;
                    if self.can_deactivate() {
                        CoordinationDecision::ReadyToDeactivate
                    } else {
                        CoordinationDecision::Started
                    }
                }
            }
            CoordinationInput::AbortTransfer => {
                if self.is_fenced() {
                    CoordinationDecision::Reject(RejectReason::Fenced)
                } else if self.is_shutdown() || self.is_draining() {
                    CoordinationDecision::Reject(RejectReason::Draining)
                } else if self.transfer_preparing {
                    self.transfer_preparing = false;
                    CoordinationDecision::Ignored
                } else {
                    CoordinationDecision::Ignored
                }
            }
            CoordinationInput::BeginCompaction {
                queue_empty,
                publication_idle,
                publisher_ready,
                due,
                lease_live,
            } => {
                if !lease_live {
                    self.lifecycle = Lifecycle::Fenced;
                    CoordinationDecision::Fence
                } else if self.is_fenced()
                    || self.is_draining()
                    || self.transfer_preparing
                    || self.busy
                    || self.renewing
                    || !queue_empty
                    || !publication_idle
                    || !publisher_ready
                    || !due
                    || !self.pending_effects.is_empty()
                {
                    CoordinationDecision::Ignored
                } else {
                    // The publisher token is exclusive, and busy keeps new SQL
                    // from building unpublished cuts behind a long promotion.
                    self.busy = true;
                    CoordinationDecision::Started
                }
            }
            CoordinationInput::FinishCompaction { fenced } => {
                self.busy = false;
                if fenced || self.is_fenced() {
                    self.lifecycle = Lifecycle::Fenced;
                    CoordinationDecision::Fence
                } else if self.can_deactivate() && self.is_draining() {
                    CoordinationDecision::ReadyToDeactivate
                } else {
                    CoordinationDecision::Ignored
                }
            }
            CoordinationInput::FinishHydration { complete, stale } => {
                // A command may have started while this fetch was in flight.
                // Hydration completion must not release its exclusive slot.
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
                self.transfer_preparing = false;
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
        if self.transfer_preparing {
            return CoordinationDecision::Reject(RejectReason::Draining);
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
mod tests;
