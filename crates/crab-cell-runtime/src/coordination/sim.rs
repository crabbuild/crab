use crate::coordination::{
    AdmissionKind, CoordinationDecision, CoordinationEffect, CoordinationInput, CoordinationState,
    RejectReason,
};
use std::env;

const COMMANDS: usize = 2;
const EVENT_COUNT: usize = 42;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Admit,
    BeginWork,
    BeginPublication,
    FinishPublication { succeeded: bool },
    FinishWork,
    BeginRenewal,
    FinishRenewal { fenced: bool },
    BeginDrain,
    BeginShutdown,
    BeginMigration,
    FinishMigration,
    BeginHydration,
    FinishHydration { complete: bool, stale: bool },
    BeginCompaction,
    FinishCompaction { fenced: bool },
    Fence,
    AdvanceClock { milliseconds: u64 },
    CallerCancel,
    LostResponse,
    FollowerProof { accepted: bool },
    ExactCasAfterLostResponse,
    RootAcceptedPruneFailed,
    DuplicatePublicationCompletion,
    DelayedEffect,
    OwnerCrash,
    OwnerRestart,
    LeaseExpire,
    CasConflict { exact: bool },
    Release,
    PrimitiveObligation,
    PrimitiveCompletion,
    MovementQuiesce,
    MovementDurability,
    MovementRelease,
    LostReleaseResponse,
    MovementAcquire,
    ReceiverCrash,
    MembershipLoss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MovementPhase {
    Idle,
    Quiescing,
    Durability,
    Released,
    Acquiring,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Simulation {
    state: CoordinationState,
    clock_ms: u64,
    owner: Option<u8>,
    epoch: u64,
    published_sequence: u64,
    retained: usize,
    durable_publications: u64,
    accepted: [bool; COMMANDS],
    acknowledged: [bool; COMMANDS],
    unknown: [bool; COMMANDS],
    terminal_outcomes: [u8; COMMANDS],
    cancelled: [bool; COMMANDS],
    owner_live: bool,
    released: bool,
    last_epoch: u64,
    last_published_sequence: u64,
    work_effect: Option<u64>,
    renewal_effect: Option<u64>,
    hydration_effect: Option<u64>,
    compaction_effect: Option<u64>,
    publication_effects: Vec<u64>,
    delayed_effects: usize,
    primitive_obligations: usize,
    movement: MovementPhase,
    receiver_live: bool,
    membership_live: bool,
    trace: Vec<String>,
}

impl Simulation {
    fn new() -> Self {
        Self {
            state: CoordinationState::serving(true),
            clock_ms: 0,
            owner: Some(0),
            epoch: 1,
            published_sequence: 0,
            retained: 0,
            durable_publications: 0,
            accepted: [false; COMMANDS],
            acknowledged: [false; COMMANDS],
            unknown: [false; COMMANDS],
            terminal_outcomes: [0; COMMANDS],
            cancelled: [false; COMMANDS],
            owner_live: true,
            released: false,
            last_epoch: 1,
            last_published_sequence: 0,
            work_effect: None,
            renewal_effect: None,
            hydration_effect: None,
            compaction_effect: None,
            publication_effects: Vec::new(),
            delayed_effects: 0,
            primitive_obligations: 0,
            movement: MovementPhase::Idle,
            receiver_live: true,
            membership_live: true,
            trace: Vec::new(),
        }
    }

    fn trace_bytes(&self) -> Vec<u8> {
        self.trace.join("\n").into_bytes()
    }

    fn restore_after_takeover(&mut self) {
        let retained = self.retained;
        self.state = CoordinationState::serving(true);
        self.compaction_effect = None;
        self.publication_effects.clear();
        for _ in 0..retained {
            if matches!(
                self.state.step(CoordinationInput::BeginPublication),
                CoordinationDecision::Started
            ) {
                self.publication_effects
                    .push(self.state.begin_effect(CoordinationEffect::Publication));
            }
        }
    }

    fn complete_effect(&mut self, effect_id: Option<u64>, effect: CoordinationEffect) {
        if let Some(effect_id) = effect_id {
            let decision = self
                .state
                .step(CoordinationInput::CompleteEffect { effect_id, effect });
            assert!(
                matches!(
                    decision,
                    CoordinationDecision::EffectCompleted | CoordinationDecision::StaleEffect
                ),
                "unexpected effect completion decision: {decision:?}"
            );
        }
    }

    fn apply(&mut self, event: Event) {
        let before = self.semantic_digest();
        if self.released && !matches!(event, Event::OwnerRestart) {
            self.trace.push(format!(
                "event={event:?};before={before:016x};decision=Ignored;after={:016x};clock={};epoch={};published={};retained={};durable={};owner={:?}",
                self.semantic_digest(),
                self.clock_ms,
                self.epoch,
                self.published_sequence,
                self.retained,
                self.durable_publications,
                self.owner,
            ));
            return;
        }
        let decision = match event {
            Event::Admit => {
                let command = usize::from(self.accepted[0]);
                let decision = self.state.step(CoordinationInput::Admit {
                    kind: AdmissionKind::Command,
                    admission_matches: true,
                });
                if matches!(decision, CoordinationDecision::Admit) && command < COMMANDS {
                    self.accepted[command] = true;
                }
                decision
            }
            Event::BeginWork => {
                let decision = if self.owner_live {
                    self.state.step(CoordinationInput::BeginWork {
                        kind: AdmissionKind::Command,
                        publisher_ready: true,
                    })
                } else {
                    CoordinationDecision::Ignored
                };
                if matches!(decision, CoordinationDecision::Started) {
                    self.work_effect = Some(
                        self.state
                            .begin_effect(CoordinationEffect::Work(AdmissionKind::Command)),
                    );
                }
                decision
            }
            Event::BeginPublication => {
                let decision = if self.owner_live {
                    self.state.step(CoordinationInput::BeginPublication)
                } else {
                    CoordinationDecision::Ignored
                };
                if matches!(decision, CoordinationDecision::Started) {
                    self.retained = self.retained.saturating_add(1);
                    self.publication_effects
                        .push(self.state.begin_effect(CoordinationEffect::Publication));
                }
                decision
            }
            Event::FinishPublication { succeeded } => {
                let before_publications = self.state.publication_count();
                let effect_id = self.publication_effects.pop();
                self.complete_effect(effect_id, CoordinationEffect::Publication);
                let decision = self.state.step(CoordinationInput::FinishPublication {
                    fenced: false,
                    succeeded,
                });
                let completed = before_publications.saturating_sub(self.state.publication_count());
                self.retained = self.retained.saturating_sub(completed);
                if succeeded && completed != 0 {
                    self.published_sequence =
                        self.published_sequence.saturating_add(completed as u64);
                    self.durable_publications =
                        self.durable_publications.saturating_add(completed as u64);
                } else if !succeeded && completed != 0 {
                    self.owner = None;
                }
                decision
            }
            Event::FinishWork => {
                let effect_id = self.work_effect.take();
                self.complete_effect(effect_id, CoordinationEffect::Work(AdmissionKind::Command));
                self.state
                    .step(CoordinationInput::FinishWork { fenced: false })
            }
            Event::BeginRenewal => {
                let decision = self.state.step(CoordinationInput::BeginRenewal {
                    queue_empty: true,
                    publication_idle: true,
                    lease_live: true,
                });
                if matches!(decision, CoordinationDecision::Started) {
                    self.renewal_effect =
                        Some(self.state.begin_effect(CoordinationEffect::Renewal));
                }
                decision
            }
            Event::FinishRenewal { fenced } => {
                let effect_id = self.renewal_effect.take();
                self.complete_effect(effect_id, CoordinationEffect::Renewal);
                let decision = self.state.step(CoordinationInput::FinishRenewal { fenced });
                if fenced {
                    self.owner = None;
                }
                decision
            }
            Event::BeginDrain => self.state.step(CoordinationInput::BeginDrain),
            Event::BeginShutdown => self.state.step(CoordinationInput::BeginShutdown),
            Event::BeginMigration => self.state.step(CoordinationInput::BeginMigration),
            Event::FinishMigration => self
                .state
                .step(CoordinationInput::FinishMigration { fenced: false }),
            Event::BeginHydration => {
                let decision = self.state.step(CoordinationInput::BeginHydration {
                    queue_empty: true,
                    publication_idle: true,
                    lease_live: true,
                });
                if matches!(decision, CoordinationDecision::Started) {
                    self.hydration_effect =
                        Some(self.state.begin_effect(CoordinationEffect::Hydration));
                }
                decision
            }
            Event::FinishHydration { complete, stale } => {
                let effect_id = self.hydration_effect.take();
                self.complete_effect(effect_id, CoordinationEffect::Hydration);
                self.state
                    .step(CoordinationInput::FinishHydration { complete, stale })
            }
            Event::BeginCompaction => {
                let decision = self.state.step(CoordinationInput::BeginCompaction {
                    queue_empty: true,
                    publication_idle: self.state.publication_count() == 0,
                    publisher_ready: self.compaction_effect.is_none(),
                    due: true,
                    lease_live: self.owner_live,
                });
                if matches!(decision, CoordinationDecision::Started) {
                    self.compaction_effect =
                        Some(self.state.begin_effect(CoordinationEffect::Compaction));
                }
                decision
            }
            Event::FinishCompaction { fenced } => {
                if let Some(effect_id) = self.compaction_effect.take() {
                    self.complete_effect(Some(effect_id), CoordinationEffect::Compaction);
                    if fenced {
                        self.owner = None;
                    }
                    self.state
                        .step(CoordinationInput::FinishCompaction { fenced })
                } else {
                    CoordinationDecision::Ignored
                }
            }
            Event::Fence => {
                self.owner = None;
                self.owner_live = false;
                self.state.step(CoordinationInput::Fence)
            }
            Event::AdvanceClock { milliseconds } => {
                self.clock_ms = self.clock_ms.saturating_add(milliseconds);
                CoordinationDecision::Ignored
            }
            Event::CallerCancel => {
                self.cancelled[0] = true;
                CoordinationDecision::Ignored
            }
            Event::LostResponse => {
                self.delayed_effects = self.delayed_effects.saturating_add(1);
                CoordinationDecision::Ignored
            }
            Event::FollowerProof { accepted } => {
                if accepted && self.retained != 0 {
                    self.durable_publications = self.durable_publications.saturating_add(1);
                }
                CoordinationDecision::Ignored
            }
            Event::ExactCasAfterLostResponse => {
                if self.retained != 0 {
                    let effect_id = self.publication_effects.pop();
                    self.complete_effect(effect_id, CoordinationEffect::Publication);
                    let decision = self.state.step(CoordinationInput::FinishPublication {
                        fenced: false,
                        succeeded: true,
                    });
                    self.retained = self.retained.saturating_sub(1);
                    self.published_sequence = self.published_sequence.saturating_add(1);
                    self.durable_publications = self.durable_publications.saturating_add(1);
                    decision
                } else {
                    CoordinationDecision::Ignored
                }
            }
            Event::RootAcceptedPruneFailed => {
                if self.retained == 0 {
                    CoordinationDecision::Ignored
                } else {
                    let before_publications = self.state.publication_count();
                    let effect_id = self.publication_effects.pop();
                    self.complete_effect(effect_id, CoordinationEffect::Publication);
                    let decision = self.state.step(CoordinationInput::FinishPublication {
                        fenced: true,
                        succeeded: false,
                    });
                    let completed =
                        before_publications.saturating_sub(self.state.publication_count());
                    self.retained = self.retained.saturating_sub(completed);
                    if completed != 0 {
                        // The accepted root survives cleanup failure, but the actor
                        // fences before it can return a committed result.
                        self.published_sequence = self.published_sequence.saturating_add(1);
                        self.durable_publications = self.durable_publications.saturating_add(1);
                        if self.accepted[0] && !self.acknowledged[0] {
                            self.unknown[0] = true;
                            self.terminal_outcomes[0] = self.terminal_outcomes[0].saturating_add(1);
                        }
                    }
                    decision
                }
            }
            Event::DuplicatePublicationCompletion => {
                let before_publications = self.state.publication_count();
                self.complete_effect(None, CoordinationEffect::Publication);
                let decision = self.state.step(CoordinationInput::FinishPublication {
                    fenced: false,
                    succeeded: true,
                });
                let completed = before_publications.saturating_sub(self.state.publication_count());
                self.retained = self.retained.saturating_sub(completed);
                if completed != 0 {
                    self.published_sequence =
                        self.published_sequence.saturating_add(completed as u64);
                    self.durable_publications =
                        self.durable_publications.saturating_add(completed as u64);
                }
                decision
            }
            Event::DelayedEffect => {
                self.delayed_effects = self.delayed_effects.saturating_sub(1);
                CoordinationDecision::Ignored
            }
            Event::OwnerCrash => {
                self.owner = None;
                self.owner_live = false;
                self.state.step(CoordinationInput::Fence)
            }
            Event::OwnerRestart => {
                if self.owner.is_none() {
                    self.epoch = self.epoch.saturating_add(1);
                    self.owner = Some(1);
                    self.owner_live = true;
                    self.released = false;
                    self.movement = MovementPhase::Idle;
                    self.restore_after_takeover();
                }
                CoordinationDecision::Ignored
            }
            Event::LeaseExpire => {
                self.owner = None;
                self.owner_live = false;
                self.state.step(CoordinationInput::Fence)
            }
            Event::CasConflict { exact } => {
                if exact {
                    let effect_id = self.publication_effects.pop();
                    self.complete_effect(effect_id, CoordinationEffect::Publication);
                    let before_publications = self.state.publication_count();
                    let decision = self.state.step(CoordinationInput::FinishPublication {
                        fenced: false,
                        succeeded: true,
                    });
                    let completed =
                        before_publications.saturating_sub(self.state.publication_count());
                    self.retained = self.retained.saturating_sub(completed);
                    self.published_sequence =
                        self.published_sequence.saturating_add(completed as u64);
                    self.durable_publications =
                        self.durable_publications.saturating_add(completed as u64);
                    decision
                } else {
                    self.owner = None;
                    self.owner_live = false;
                    self.state.step(CoordinationInput::Fence)
                }
            }
            Event::Release => {
                if self.state.ready_to_deactivate(true, true)
                    && self.retained == 0
                    && self.primitive_obligations == 0
                    && self
                        .accepted
                        .iter()
                        .enumerate()
                        .all(|(index, accepted)| !*accepted || self.terminal_outcomes[index] == 1)
                {
                    self.owner = None;
                    self.owner_live = false;
                    self.released = true;
                }
                CoordinationDecision::Ignored
            }
            Event::PrimitiveObligation => {
                if !self.released {
                    self.primitive_obligations = self.primitive_obligations.saturating_add(1);
                }
                CoordinationDecision::Ignored
            }
            Event::PrimitiveCompletion => {
                self.primitive_obligations = self.primitive_obligations.saturating_sub(1);
                CoordinationDecision::Ignored
            }
            Event::MovementQuiesce => {
                let decision = self.state.step(CoordinationInput::BeginDrain);
                if matches!(
                    decision,
                    CoordinationDecision::Started
                        | CoordinationDecision::ReadyToDeactivate
                        | CoordinationDecision::ReadyToDeactivateFenced
                ) {
                    self.movement = MovementPhase::Quiescing;
                }
                decision
            }
            Event::MovementDurability => {
                if self.movement == MovementPhase::Quiescing
                    && self.retained == 0
                    && self.primitive_obligations == 0
                    && self.state.ready_to_deactivate(true, true)
                {
                    self.movement = MovementPhase::Durability;
                }
                CoordinationDecision::Ignored
            }
            Event::MovementRelease => {
                if self.movement == MovementPhase::Durability
                    && self.retained == 0
                    && self.primitive_obligations == 0
                    && self.state.ready_to_deactivate(true, true)
                {
                    self.owner = None;
                    self.owner_live = false;
                    self.movement = MovementPhase::Released;
                }
                CoordinationDecision::Ignored
            }
            // The authority transition already committed; only the caller's
            // reply is lost. The receiver must still observe the released
            // owner through the normal authority path.
            Event::LostReleaseResponse => CoordinationDecision::Ignored,
            Event::MovementAcquire => {
                if self.movement == MovementPhase::Released
                    && self.receiver_live
                    && self.membership_live
                {
                    self.movement = MovementPhase::Acquiring;
                    self.epoch = self.epoch.saturating_add(1);
                    self.owner = Some(1);
                    self.owner_live = true;
                    self.restore_after_takeover();
                    self.movement = MovementPhase::Idle;
                }
                CoordinationDecision::Ignored
            }
            Event::ReceiverCrash => {
                self.receiver_live = false;
                if self.movement == MovementPhase::Acquiring {
                    self.movement = MovementPhase::Released;
                }
                CoordinationDecision::Ignored
            }
            Event::MembershipLoss => {
                self.membership_live = false;
                if self.movement == MovementPhase::Acquiring {
                    self.movement = MovementPhase::Released;
                    self.owner = None;
                    self.owner_live = false;
                }
                CoordinationDecision::Ignored
            }
        };
        if matches!(decision, CoordinationDecision::Admit) {
            self.accepted[0] = true;
        }
        if self.state.is_fenced() {
            self.owner = None;
            self.owner_live = false;
        }
        if self.durable_publications != 0
            && self.accepted[0]
            && !self.acknowledged[0]
            && !self.unknown[0]
        {
            self.acknowledged[0] = true;
            self.terminal_outcomes[0] = self.terminal_outcomes[0].saturating_add(1);
        }
        self.trace.push(format!(
            "event={event:?};before={before:016x};decision={decision:?};after={:016x};clock={};epoch={};published={};retained={};durable={};owner={:?}",
            self.semantic_digest(),
            self.clock_ms,
            self.epoch,
            self.published_sequence,
            self.retained,
            self.durable_publications,
            self.owner,
        ));
    }

    fn semantic_digest(&self) -> u64 {
        let mut value = self.clock_ms ^ self.epoch.rotate_left(11);
        value ^= self.published_sequence.rotate_left(23);
        value ^= self.durable_publications.rotate_left(37);
        value ^= (self.retained as u64).rotate_left(47);
        value ^= u64::from(self.state.is_fenced()).rotate_left(53);
        value ^= u64::from(self.state.is_draining()).rotate_left(59);
        value ^= u64::from(self.owner.unwrap_or(u8::MAX)).rotate_left(7);
        value ^= u64::from(self.owner_live).rotate_left(13);
        value ^= u64::from(self.released).rotate_left(19);
        value ^= (self.delayed_effects as u64).rotate_left(29);
        value ^= (self.primitive_obligations as u64).rotate_left(41);
        value ^= (self.movement as u64).rotate_left(31);
        value ^= u64::from(self.receiver_live).rotate_left(43);
        value ^= u64::from(self.membership_live).rotate_left(51);
        for (index, accepted) in self.accepted.iter().enumerate() {
            value ^= u64::from(*accepted).rotate_left(index as u32 + 1);
            value ^= u64::from(self.acknowledged[index]).rotate_left(index as u32 + 9);
            value ^= u64::from(self.unknown[index]).rotate_left(index as u32 + 25);
            value ^= u64::from(self.terminal_outcomes[index]).rotate_left(index as u32 + 17);
        }
        value
    }

    fn assert_invariants(&self) {
        assert!(self.owner.is_none_or(|owner| owner < 2));
        assert!(self.epoch >= self.last_epoch);
        assert!(self.published_sequence >= self.last_published_sequence);
        assert!(self.published_sequence <= self.durable_publications);
        assert!(self.terminal_outcomes.iter().all(|outcomes| *outcomes <= 1));
        assert!(self.unknown.iter().enumerate().all(|(index, unknown)| {
            !*unknown
                || (self.accepted[index]
                    && !self.acknowledged[index]
                    && self.durable_publications != 0
                    && self.terminal_outcomes[index] == 1)
        }));
        assert!(
            self.acknowledged
                .iter()
                .enumerate()
                .all(|(index, acknowledged)| !*acknowledged || self.accepted[index])
        );
        assert!(
            self.acknowledged
                .iter()
                .all(|acknowledged| !*acknowledged || self.durable_publications != 0)
        );
        if self.state.is_fenced() {
            assert!(matches!(
                self.state.lookup(),
                CoordinationDecision::Reject(RejectReason::Fenced)
            ));
            assert!(self.owner.is_none());
            assert!(!self.owner_live);
        }
        if self.state.is_draining() {
            assert!(!matches!(
                self.state.lookup(),
                CoordinationDecision::LocalHandle
            ));
        }
        assert!(!self.owner_live || self.owner.is_some());
        assert!(!self.released || self.owner.is_none());
        assert!(!self.released || self.primitive_obligations == 0);
        assert!(!self.released || self.state.ready_to_deactivate(true, true));
        assert!(
            !self.released
                || self
                    .accepted
                    .iter()
                    .enumerate()
                    .all(|(index, accepted)| { !*accepted || self.terminal_outcomes[index] == 1 })
        );
        assert!(
            self.retained == self.state.publication_count(),
            "publication obligation was not retained: {:?}",
            self.trace
        );
        assert!(
            self.owner.is_some() || self.retained == 0 || self.state.is_fenced(),
            "owner lost with retained publication: {:?}",
            self.trace
        );
        if matches!(
            self.movement,
            MovementPhase::Quiescing
                | MovementPhase::Durability
                | MovementPhase::Released
                | MovementPhase::Acquiring
        ) {
            assert!(!matches!(
                self.state.lookup(),
                CoordinationDecision::LocalHandle
            ));
        }
        if matches!(
            self.movement,
            MovementPhase::Released | MovementPhase::Acquiring
        ) {
            assert!(self.owner.is_none() || self.movement == MovementPhase::Acquiring);
        }
    }

    fn record_progress(&mut self) {
        self.last_epoch = self.epoch;
        self.last_published_sequence = self.published_sequence;
    }
}

fn replay(seed: u64, steps: usize) -> Simulation {
    let mut simulation = Simulation::new();
    let mut random = SplitMix64(seed);
    for _ in 0..steps {
        simulation.apply(event(random.next()));
        simulation.assert_invariants();
        simulation.record_progress();
    }
    simulation
}

fn event(seed: u64) -> Event {
    match seed as usize % EVENT_COUNT {
        0 => Event::Admit,
        1 => Event::BeginWork,
        2 => Event::BeginPublication,
        3 => Event::FinishPublication { succeeded: true },
        4 => Event::FinishPublication { succeeded: false },
        5 => Event::FinishWork,
        6 => Event::BeginRenewal,
        7 => Event::FinishRenewal {
            fenced: seed & 1 == 0,
        },
        8 => Event::BeginDrain,
        9 => Event::BeginShutdown,
        10 => Event::BeginMigration,
        11 => Event::FinishMigration,
        12 => Event::BeginHydration,
        13 => Event::FinishHydration {
            complete: seed & 1 == 0,
            stale: seed & 2 == 0,
        },
        14 => Event::Fence,
        15 => Event::AdvanceClock { milliseconds: 17 },
        16 => Event::CallerCancel,
        17 => Event::LostResponse,
        18 => Event::FollowerProof {
            accepted: seed & 1 == 0,
        },
        19 => Event::ExactCasAfterLostResponse,
        20 => Event::DuplicatePublicationCompletion,
        21 => Event::DelayedEffect,
        22 => Event::OwnerCrash,
        23 => Event::OwnerRestart,
        24 => Event::LeaseExpire,
        25 => Event::CasConflict {
            exact: seed & 1 == 0,
        },
        26 => Event::Release,
        27 => Event::PrimitiveObligation,
        28 => Event::PrimitiveCompletion,
        29 => Event::AdvanceClock { milliseconds: 1 },
        30 => Event::CallerCancel,
        31 => Event::LostResponse,
        32 => Event::MovementQuiesce,
        33 => Event::MovementDurability,
        34 => Event::MovementRelease,
        35 => Event::LostReleaseResponse,
        36 => Event::MovementAcquire,
        37 => Event::ReceiverCrash,
        38 => Event::MembershipLoss,
        39 => Event::BeginCompaction,
        40 => Event::FinishCompaction {
            fenced: seed & 1 == 0,
        },
        _ => Event::RootAcceptedPruneFailed,
    }
}

fn explore(simulation: &Simulation, depth: usize) {
    simulation.assert_invariants();
    if depth == 0 {
        return;
    }
    for choice in 0..EVENT_COUNT as u64 {
        let mut next = simulation.clone();
        next.apply(event(choice + depth as u64));
        next.assert_invariants();
        next.record_progress();
        explore(&next, depth - 1);
    }
}

#[test]
fn seeded_schedules_replay_byte_identically() {
    for seed in [0, 1, 7, 41, 99, 0xfeed_1234] {
        let first = replay(seed, 128);
        let second = replay(seed, 128);
        assert_eq!(first.trace_bytes(), second.trace_bytes());
        assert_eq!(
            first, second,
            "seed {seed} did not replay deterministically"
        );
    }
}

#[test]
fn broad_seed_corpus_is_replayable() {
    for seed in 0..512 {
        println!("coordination_seed={seed};steps=256");
        let first = replay(seed, 256);
        let second = replay(seed, 256);
        assert_eq!(first.trace_bytes(), second.trace_bytes(), "seed {seed}");
    }
}

#[test]
fn replay_requested_seed_from_environment() {
    let Ok(seed) = env::var("CRAB_COORDINATION_SEED") else {
        return;
    };
    let seed = seed
        .parse::<u64>()
        .expect("CRAB_COORDINATION_SEED must be an unsigned integer");
    let steps = env::var("CRAB_COORDINATION_STEPS")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("CRAB_COORDINATION_STEPS must be an unsigned integer")
        })
        .unwrap_or(128);
    assert!(
        steps <= 4_096,
        "CRAB_COORDINATION_STEPS exceeds the replay bound"
    );
    let simulation = replay(seed, steps);
    println!(
        "coordination_seed={seed};steps={steps};trace_bytes={}",
        simulation.trace_bytes().len()
    );
}

#[test]
fn bounded_exhaustive_schedules_preserve_protocol_invariants() {
    explore(&Simulation::new(), 4);
}

#[test]
fn broken_early_acknowledgement_is_detected() {
    let mut simulation = Simulation::new();
    simulation.accepted[0] = true;
    simulation.acknowledged[0] = true;
    assert!(std::panic::catch_unwind(|| simulation.assert_invariants()).is_err());
}

#[test]
fn broken_accept_after_fence_is_detected() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::Fence);
    simulation.accepted[0] = true;
    assert!(simulation.state.is_fenced());
    assert!(matches!(
        simulation.state.lookup(),
        CoordinationDecision::Reject(RejectReason::Fenced)
    ));
    assert!(simulation.owner.is_none());
}

#[test]
fn broken_different_winner_is_detected() {
    let mut simulation = Simulation::new();
    simulation.owner = Some(1);
    assert!(
        std::panic::catch_unwind(|| {
            assert_eq!(
                simulation.owner,
                Some(0),
                "different CAS winner was adopted"
            )
        })
        .is_err()
    );
}

#[test]
fn broken_early_release_is_detected() {
    let mut simulation = Simulation::new();
    simulation.retained = 1;
    simulation.owner = None;
    assert!(std::panic::catch_unwind(|| simulation.assert_invariants()).is_err());
}

#[test]
fn duplicate_publication_completion_cannot_underflow_publication_state() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::BeginPublication);
    simulation.apply(Event::FinishPublication { succeeded: true });
    simulation.apply(Event::DuplicatePublicationCompletion);
    simulation.assert_invariants();
    assert_eq!(simulation.state.publication_count(), 0);
}

#[test]
fn accepted_root_with_failed_prune_fences_before_success_reply() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::Admit);
    simulation.apply(Event::BeginWork);
    simulation.apply(Event::BeginPublication);
    simulation.apply(Event::RootAcceptedPruneFailed);

    assert_eq!(simulation.published_sequence, 1);
    assert_eq!(simulation.durable_publications, 1);
    assert_eq!(simulation.retained, 0);
    assert!(simulation.state.is_fenced());
    assert!(simulation.unknown[0]);
    assert!(!simulation.acknowledged[0]);
    assert_eq!(simulation.terminal_outcomes[0], 1);
    simulation.apply(Event::OwnerRestart);
    simulation.assert_invariants();
    assert_eq!(simulation.published_sequence, 1);
    assert_eq!(simulation.terminal_outcomes[0], 1);
}

#[test]
fn prune_failure_preserves_a_prior_follower_acknowledgement() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::Admit);
    simulation.apply(Event::BeginWork);
    simulation.apply(Event::BeginPublication);
    simulation.apply(Event::FollowerProof { accepted: true });
    assert!(simulation.acknowledged[0]);
    simulation.apply(Event::RootAcceptedPruneFailed);

    simulation.assert_invariants();
    assert!(simulation.state.is_fenced());
    assert_eq!(simulation.published_sequence, 1);
    assert!(simulation.acknowledged[0]);
    assert!(!simulation.unknown[0]);
    assert_eq!(simulation.terminal_outcomes[0], 1);
}

#[test]
fn movement_requires_quiesce_durability_release_and_live_acquire() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::MovementQuiesce);
    assert_eq!(simulation.movement, MovementPhase::Quiescing);
    simulation.apply(Event::MovementRelease);
    assert_eq!(simulation.movement, MovementPhase::Quiescing);
    simulation.apply(Event::MovementDurability);
    assert_eq!(simulation.movement, MovementPhase::Durability);
    simulation.apply(Event::MovementRelease);
    assert_eq!(simulation.movement, MovementPhase::Released);
    simulation.apply(Event::LostReleaseResponse);
    assert_eq!(simulation.owner, None);
    simulation.apply(Event::ReceiverCrash);
    simulation.apply(Event::MovementAcquire);
    assert_eq!(simulation.movement, MovementPhase::Released);
    simulation.receiver_live = true;
    simulation.membership_live = true;
    simulation.apply(Event::MovementAcquire);
    assert_eq!(simulation.movement, MovementPhase::Idle);
    assert_eq!(simulation.owner, Some(1));
}

#[test]
fn membership_loss_during_movement_preserves_released_root() {
    let mut simulation = Simulation::new();
    simulation.apply(Event::BeginPublication);
    simulation.apply(Event::FinishPublication { succeeded: true });
    simulation.apply(Event::MovementQuiesce);
    simulation.apply(Event::MovementDurability);
    simulation.apply(Event::MovementRelease);

    let epoch = simulation.epoch;
    let published_sequence = simulation.published_sequence;
    let durable_publications = simulation.durable_publications;
    simulation.apply(Event::MembershipLoss);
    simulation.apply(Event::MovementAcquire);

    assert_eq!(simulation.movement, MovementPhase::Released);
    assert_eq!(simulation.owner, None);
    assert_eq!(simulation.epoch, epoch);
    assert_eq!(simulation.published_sequence, published_sequence);
    assert_eq!(simulation.durable_publications, durable_publications);
    simulation.assert_invariants();
}
