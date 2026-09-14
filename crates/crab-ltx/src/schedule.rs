//! Bounded level scheduling adapted from Celld's compaction-level contracts.

use crate::{CrabError, Replica, ReplicaHead, Result};
use std::time::Duration;

/// Caller-driven monotonic compaction schedule; no detached task or owner election.
///
/// Levels 1, 2 and 3 default to 30 seconds, 5 minutes and 1 hour. The owner
/// calls `run_due` on its worker and supervises cancellation/ambiguous CAS.
pub struct CompactionSchedule {
    intervals: Vec<Duration>,
    due: Vec<Duration>,
    observed: Duration,
}

impl CompactionSchedule {
    /// Starts levels 1 through N with nonzero intervals and a monotonic origin.
    pub fn new(intervals: Vec<Duration>, now: Duration) -> Result<Self> {
        if intervals.is_empty() || intervals.len() > 8 || intervals.contains(&Duration::ZERO) {
            return Err(CrabError::InvalidState("invalid compaction intervals"));
        }
        let due = intervals
            .iter()
            .map(|d| {
                now.checked_add(*d)
                    .ok_or(CrabError::Limit("compaction deadline"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            intervals,
            due,
            observed: now,
        })
    }

    /// Returns the next monotonic deadline, allowing one owner timer for many replicas.
    #[must_use]
    pub fn next_due(&self) -> Duration {
        self.due.iter().copied().min().unwrap_or(Duration::MAX)
    }

    /// Compacts at most one due level's contiguous input run, retaining all inputs.
    ///
    /// A run is limited by the replica's file/plan admission and 128 files.
    /// No work returns `None`; failed publication does not advance its deadline.
    pub async fn run_due(
        &mut self,
        replica: &Replica,
        head: &ReplicaHead,
        now: Duration,
    ) -> Result<Option<ReplicaHead>> {
        if now < self.observed {
            return Err(CrabError::InvalidState("compaction clock moved backwards"));
        }
        self.observed = now;
        for index in 0..self.due.len() {
            if self.due[index] > now {
                continue;
            }
            let level = index as u8 + 1;
            let next = now
                .checked_add(self.intervals[index])
                .ok_or(CrabError::Limit("compaction deadline"))?;
            if let Some(range) = replica.compaction_range(head, level)? {
                let output = replica.compact_range(head, range, level).await?;
                self.due[index] = next;
                return Ok(Some(output));
            }
            self.due[index] = next;
        }
        Ok(None)
    }
}

impl Default for CompactionSchedule {
    fn default() -> Self {
        let intervals = vec![
            Duration::from_secs(30),
            Duration::from_secs(300),
            Duration::from_secs(3600),
        ];
        Self {
            due: intervals.clone(),
            intervals,
            observed: Duration::ZERO,
        }
    }
}
