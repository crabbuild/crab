//! Pressure classification with hysteresis.
use crate::{Error, Result};

const MAX_PERMILLE: u16 = 1_000;

/// Node pressure state used by placement and paced drain policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureState {
    Normal,
    Constrained,
    Shedding,
    Recovering,
    Critical,
}

/// Bounded measured utilization sample. Values are permille, not floats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureSample {
    pub at_ms: i64,
    pub memory_used_permille: u16,
    pub disk_used_permille: u16,
    pub jobs_used_permille: u16,
    pub stale: bool,
}

impl PressureSample {
    fn validate(self) -> Result<()> {
        if self.at_ms < 0
            || self.memory_used_permille > MAX_PERMILLE
            || self.disk_used_permille > MAX_PERMILLE
            || self.jobs_used_permille > MAX_PERMILLE
        {
            return Err(Error::Control("pressure sample is invalid"));
        }
        Ok(())
    }

    fn elevated(self, threshold: u16) -> bool {
        self.memory_used_permille >= threshold
            || self.disk_used_permille >= threshold
            || self.jobs_used_permille >= threshold
    }

    fn recovered(self, threshold: u16) -> bool {
        self.memory_used_permille <= threshold
            && self.disk_used_permille <= threshold
            && self.jobs_used_permille <= threshold
    }
}

/// Hysteretic classifier requiring a sustained sample before changing state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureClassifier {
    enter_permille: u16,
    exit_permille: u16,
    dwell_ms: i64,
    state: PressureState,
    pending: Option<(PressureState, i64)>,
    last_at_ms: Option<i64>,
}

impl PressureClassifier {
    /// Creates a classifier with separate enter/exit thresholds.
    pub fn new(enter_permille: u16, exit_permille: u16, dwell_ms: i64) -> Result<Self> {
        if enter_permille > MAX_PERMILLE || exit_permille >= enter_permille || dwell_ms <= 0 {
            return Err(Error::Control("pressure thresholds are invalid"));
        }
        Ok(Self {
            enter_permille,
            exit_permille,
            dwell_ms,
            state: PressureState::Normal,
            pending: None,
            last_at_ms: None,
        })
    }

    #[must_use]
    pub const fn state(self) -> PressureState {
        self.state
    }

    /// Advances the classifier; stale samples cannot declare recovery.
    pub fn observe(&mut self, sample: PressureSample) -> Result<PressureState> {
        sample.validate()?;
        if self.last_at_ms.is_some_and(|last| sample.at_ms < last) {
            return Err(Error::Control("pressure sample time regressed"));
        }
        self.last_at_ms = Some(sample.at_ms);
        if sample.stale {
            self.pending = None;
            if self.state == PressureState::Normal {
                self.state = PressureState::Constrained;
            }
            return Ok(self.state);
        }

        let desired = if sample.elevated(self.enter_permille) {
            if sample.memory_used_permille >= self.enter_permille
                && sample.disk_used_permille >= self.enter_permille
            {
                PressureState::Critical
            } else {
                PressureState::Shedding
            }
        } else if sample.recovered(self.exit_permille) {
            PressureState::Normal
        } else {
            PressureState::Constrained
        };

        if desired == self.state {
            self.pending = None;
            return Ok(self.state);
        }
        let Some((pending, started)) = self.pending else {
            self.pending = Some((desired, sample.at_ms));
            return Ok(self.state);
        };
        if pending != desired {
            self.pending = Some((desired, sample.at_ms));
            return Ok(self.state);
        }
        if sample.at_ms.saturating_sub(started) >= self.dwell_ms {
            self.state = if desired == PressureState::Normal {
                PressureState::Recovering
            } else {
                desired
            };
            self.pending = None;
            if self.state == PressureState::Recovering {
                self.state = PressureState::Normal;
            }
        }
        Ok(self.state)
    }
}

/// One bounded movement operation admitted by a node-local drain controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovementKind {
    Quiesce,
    Release,
    Receive,
}

/// Synchronous concurrency/rate budget for pressure movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MovementBudget {
    limit: u32,
    used: u32,
    interval_ms: i64,
    window_started_ms: i64,
    completed_in_window: u32,
}

impl MovementBudget {
    pub fn new(limit: u32, interval_ms: i64) -> Result<Self> {
        if limit == 0 || interval_ms <= 0 {
            return Err(Error::Capacity("movement budget"));
        }
        Ok(Self {
            limit,
            used: 0,
            interval_ms,
            window_started_ms: 0,
            completed_in_window: 0,
        })
    }

    pub fn try_start(&mut self, now_ms: i64) -> Result<MovementPermit> {
        if now_ms < self.window_started_ms {
            return Err(Error::Control("movement time regressed"));
        }
        if now_ms.saturating_sub(self.window_started_ms) >= self.interval_ms {
            self.window_started_ms = now_ms;
            self.completed_in_window = 0;
        }
        if self.used >= self.limit || self.completed_in_window >= self.limit {
            return Err(Error::Capacity("movement budget"));
        }
        self.used += 1;
        Ok(MovementPermit { completed: false })
    }

    pub fn complete(&mut self, permit: &mut MovementPermit) {
        if permit.completed {
            return;
        }
        permit.completed = true;
        self.used = self.used.saturating_sub(1);
        self.completed_in_window = self.completed_in_window.saturating_add(1);
    }

    #[must_use]
    pub const fn in_flight(self) -> u32 {
        self.used
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MovementPermit {
    completed: bool,
}
