//! Operation vocabulary and the deterministic schedule that produces it.

use serde::{Deserialize, Serialize};

use crate::identity::Digest;
use crate::qualification::profile::{
    QUALIFICATION_CASE_COVERAGE_OPERATIONS, QUALIFICATION_CASES, QUALIFICATION_PRIMITIVES,
    QualificationCase,
};

/// Bounded per-primitive counters emitted by the deterministic workload driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationPrimitiveCounts {
    pub(in crate::qualification) primitive: String,
    pub(in crate::qualification) attempted: u64,
    pub(in crate::qualification) acknowledged: u64,
    pub(in crate::qualification) rejected: u64,
    pub(in crate::qualification) ambiguous: u64,
    pub(in crate::qualification) retried: u64,
    pub(in crate::qualification) verified: u64,
}

impl QualificationPrimitiveCounts {
    pub(in crate::qualification) fn new(primitive: &str) -> Self {
        Self {
            primitive: primitive.to_owned(),
            attempted: 0,
            acknowledged: 0,
            rejected: 0,
            ambiguous: 0,
            retried: 0,
            verified: 0,
        }
    }

    /// Returns the primitive row name.
    #[must_use]
    pub fn primitive(&self) -> &str {
        &self.primitive
    }

    /// Operations attempted for this primitive.
    #[must_use]
    pub const fn attempted(&self) -> u64 {
        self.attempted
    }

    /// Attempts the destination acknowledged.
    #[must_use]
    pub const fn acknowledged(&self) -> u64 {
        self.acknowledged
    }

    /// Attempts the destination rejected.
    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Attempts whose outcome stayed unknown.
    #[must_use]
    pub const fn ambiguous(&self) -> u64 {
        self.ambiguous
    }

    /// Attempts that needed a retry.
    #[must_use]
    pub const fn retried(&self) -> u64 {
        self.retried
    }

    /// Attempts whose recorded result was verified.
    #[must_use]
    pub const fn verified(&self) -> u64 {
        self.verified
    }
}

/// One deterministic operation in a qualification workload.
///
/// The iterator exposes only bounded scalar identity. Payloads and primitive
/// requests remain owned by the application-specific executor, so a scale run
/// does not retain millions of operation bodies in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualificationOperation {
    pub(in crate::qualification) index: u64,
    pub(in crate::qualification) primitive_index: u8,
    pub(in crate::qualification) cell_index: u64,
    pub(in crate::qualification) nonce: u64,
    pub(in crate::qualification) case: QualificationCase,
    pub(in crate::qualification) retry_hint: bool,
    pub(in crate::qualification) rejection_hint: bool,
    pub(in crate::qualification) ambiguous_hint: bool,
}

impl QualificationOperation {
    /// Returns the monotonically increasing logical operation number.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// Returns the primitive selected by the canonical schedule.
    #[must_use]
    pub fn primitive(self) -> &'static str {
        QUALIFICATION_PRIMITIVES[self.primitive_index as usize]
    }

    /// Returns the deterministic Cell ordinal selected by the schedule.
    #[must_use]
    pub const fn cell_index(self) -> u64 {
        self.cell_index
    }

    /// Returns the deterministic nonce for request identity/payload seeding.
    #[must_use]
    pub const fn nonce(self) -> u64 {
        self.nonce
    }

    /// Returns the bounded lifecycle case assigned to this operation.
    #[must_use]
    pub const fn case(self) -> QualificationCase {
        self.case
    }

    /// Returns whether this operation exercises producer/delivery duplicate handling.
    #[must_use]
    pub const fn duplicate_hint(self) -> bool {
        matches!(self.case, QualificationCase::Duplicate)
    }

    /// Returns whether this operation exercises expiry handling.
    #[must_use]
    pub const fn expiry_hint(self) -> bool {
        matches!(self.case, QualificationCase::Expiry)
    }

    /// Returns whether this operation exercises cancellation handling.
    #[must_use]
    pub const fn cancellation_hint(self) -> bool {
        matches!(self.case, QualificationCase::Cancellation)
    }

    /// Returns whether this operation exercises owner-loss handling.
    #[must_use]
    pub const fn owner_loss_hint(self) -> bool {
        matches!(self.case, QualificationCase::OwnerLoss)
    }

    /// Returns whether this operation exercises post-takeover recovery.
    #[must_use]
    pub const fn recovery_hint(self) -> bool {
        matches!(self.case, QualificationCase::Recovery)
    }

    /// Returns whether this operation is scheduled to exercise a retry path.
    #[must_use]
    pub const fn retry_hint(self) -> bool {
        self.retry_hint
    }

    /// Returns whether this operation is scheduled to exercise rejection.
    #[must_use]
    pub const fn rejection_hint(self) -> bool {
        self.rejection_hint
    }

    /// Returns whether this operation is scheduled to exercise ambiguity.
    #[must_use]
    pub const fn ambiguous_hint(self) -> bool {
        self.ambiguous_hint
    }
}

/// Actual outcome reported by one application-specific operation executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualificationOutcome {
    /// The destination accepted the operation.
    Acknowledged,
    /// The destination rejected the operation.
    Rejected,
    /// The outcome could not be determined.
    Ambiguous,
}

/// Bounded result returned by a typed workload executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualificationExecution {
    pub(in crate::qualification) outcome: QualificationOutcome,
    pub(in crate::qualification) verified: bool,
    pub(in crate::qualification) retries: u64,
    pub(in crate::qualification) case: Option<QualificationCase>,
}

impl QualificationExecution {
    /// Records an acknowledged operation and whether its state was verified.
    #[must_use]
    pub const fn acknowledged(verified: bool) -> Self {
        Self {
            outcome: QualificationOutcome::Acknowledged,
            verified,
            retries: 0,
            case: None,
        }
    }

    /// Records a rejected operation.
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            outcome: QualificationOutcome::Rejected,
            verified: false,
            retries: 0,
            case: None,
        }
    }

    /// Records an ambiguous operation with the number of resolution attempts.
    #[must_use]
    pub const fn ambiguous(retries: u64) -> Self {
        Self {
            outcome: QualificationOutcome::Ambiguous,
            verified: false,
            retries,
            case: None,
        }
    }

    /// Adds a retry count to an execution result.
    #[must_use]
    pub const fn with_retries(mut self, retries: u64) -> Self {
        self.retries = retries;
        self
    }

    /// Binds the executor result to the lifecycle case it actually exercised.
    #[must_use]
    pub const fn with_case(mut self, case: QualificationCase) -> Self {
        self.case = Some(case);
        self
    }

    /// Returns the outcome the executor reported.
    #[must_use]
    pub const fn outcome(self) -> QualificationOutcome {
        self.outcome
    }

    /// Reports whether the result was verified against the record.
    #[must_use]
    pub const fn verified(self) -> bool {
        self.verified
    }

    /// Returns how many retries the operation needed.
    #[must_use]
    pub const fn retries(self) -> u64 {
        self.retries
    }

    /// Returns the matrix case the operation belongs to, when any.
    #[must_use]
    pub const fn case(self) -> Option<QualificationCase> {
        self.case
    }
}

pub(in crate::qualification) fn qualification_workload_outcome_digest(
    seed: u64,
    cells: u64,
    operations: u64,
) -> Digest {
    let mut hasher = blake3::Hasher::new();
    for operation in QualificationOperationIter::new(seed, cells, operations) {
        hasher.update(&operation.index.to_be_bytes());
        hasher.update(&u64::from(operation.primitive_index).to_be_bytes());
        hasher.update(&operation.cell_index.to_be_bytes());
        hasher.update(&operation.nonce.to_be_bytes());
        hasher.update(&[operation.case as u8]);
    }
    Digest::from_bytes(*hasher.finalize().as_bytes())
}

pub(in crate::qualification) fn valid_primitive_counts(
    counts: &QualificationPrimitiveCounts,
) -> bool {
    counts
        .acknowledged
        .checked_add(counts.rejected)
        .and_then(|total| total.checked_add(counts.ambiguous))
        == Some(counts.attempted)
        && counts.retried <= counts.attempted
        && counts.verified <= counts.acknowledged
}

pub(in crate::qualification) const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
pub(in crate::qualification) const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
pub(in crate::qualification) const LCG_SEED_XOR: u64 = 0x9e37_79b9_7f4a_7c15;

/// Streaming operation iterator; it retains only the LCG state and counters.
pub struct QualificationOperationIter {
    pub(in crate::qualification) index: u64,
    pub(in crate::qualification) cells: u64,
    pub(in crate::qualification) operations: u64,
    pub(in crate::qualification) state: u64,
}

impl QualificationOperationIter {
    pub(in crate::qualification) fn new(seed: u64, cells: u64, operations: u64) -> Self {
        Self {
            index: 0,
            cells,
            operations,
            state: seed ^ LCG_SEED_XOR,
        }
    }
}

impl Iterator for QualificationOperationIter {
    type Item = QualificationOperation;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.operations {
            return None;
        }
        self.state = self
            .state
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let operation = operation_from_state(self.index, self.state, self.cells);
        self.index = self.index.saturating_add(1);
        Some(operation)
    }
}

pub(in crate::qualification) fn operation_from_state(
    index: u64,
    state: u64,
    cells: u64,
) -> QualificationOperation {
    let primitive_count = QUALIFICATION_PRIMITIVES.len() as u64;
    let coverage_operations = QUALIFICATION_CASE_COVERAGE_OPERATIONS;
    let (primitive_index, case_index) = if index < coverage_operations {
        (
            (index % primitive_count) as usize,
            (index / primitive_count) as usize,
        )
    } else {
        (
            (state as usize) % QUALIFICATION_PRIMITIVES.len(),
            (state.rotate_left(19) as usize) % QUALIFICATION_CASES.len(),
        )
    };
    QualificationOperation {
        index,
        primitive_index: primitive_index as u8,
        cell_index: state % cells,
        nonce: state,
        case: QUALIFICATION_CASES[case_index],
        retry_hint: state & 0x1f == 0,
        rejection_hint: state & 0x3ff == 0,
        ambiguous_hint: state & 0x7ff == 0x200,
    }
}

pub(in crate::qualification) fn lcg_state_at(seed: u64, steps: u64) -> u64 {
    let mut result = (1_u64, 0_u64);
    let mut base = (LCG_MULTIPLIER, LCG_INCREMENT);
    let mut steps = steps;
    while steps != 0 {
        if steps & 1 != 0 {
            result = compose_affine(base, result);
        }
        base = compose_affine(base, base);
        steps >>= 1;
    }
    result.0.wrapping_mul(seed).wrapping_add(result.1)
}

pub(in crate::qualification) fn compose_affine(
    after: (u64, u64),
    before: (u64, u64),
) -> (u64, u64) {
    (
        after.0.wrapping_mul(before.0),
        after.0.wrapping_mul(before.1).wrapping_add(after.1),
    )
}
