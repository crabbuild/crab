use std::sync::Arc;

use tokio::sync::Mutex;

use crate::repository::OperationLimits;
use crate::{Error, MetricKind, MetricObservation, RemoteGitRuntime, Result};

/// Aggregate work dimensions charged by semantic repository operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetDimension {
    LogicalObjects,
    StorageRequests,
    FetchedBytes,
    InflatedBytes,
    Depth,
    Entries,
    HistoryCommits,
    DiffInputBytes,
    DiffOutputBytes,
    BlameLines,
    BlameComparisons,
    ArchiveEntries,
    ArchiveBytes,
    ResponseBytes,
}

impl BudgetDimension {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LogicalObjects => "logical objects",
            Self::StorageRequests => "storage requests",
            Self::FetchedBytes => "fetched bytes",
            Self::InflatedBytes => "inflated bytes",
            Self::Depth => "traversal depth",
            Self::Entries => "entries",
            Self::HistoryCommits => "history commits",
            Self::DiffInputBytes => "diff input bytes",
            Self::DiffOutputBytes => "diff output bytes",
            Self::BlameLines => "blame lines",
            Self::BlameComparisons => "blame comparison cells",
            Self::ArchiveEntries => "archive entries",
            Self::ArchiveBytes => "archive bytes",
            Self::ResponseBytes => "response bytes",
        }
    }
}

/// Checked aggregate counters for one operation.
#[derive(Debug, Clone)]
pub(crate) struct WorkBudget {
    limits: OperationLimits,
    used: [u64; 14],
}

#[derive(Clone)]
pub(crate) struct OperationBudget {
    work: Arc<Mutex<WorkBudget>>,
    runtime: Arc<RemoteGitRuntime>,
    id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetUsage {
    used: [u64; 14],
}

const CACHED_SEMANTIC_DIMENSIONS: [BudgetDimension; 7] = [
    BudgetDimension::LogicalObjects,
    BudgetDimension::Depth,
    BudgetDimension::Entries,
    BudgetDimension::HistoryCommits,
    BudgetDimension::BlameLines,
    BudgetDimension::BlameComparisons,
    BudgetDimension::ResponseBytes,
];

impl OperationBudget {
    pub(crate) fn new(limits: OperationLimits, runtime: Arc<RemoteGitRuntime>, id: u64) -> Self {
        Self {
            work: Arc::new(Mutex::new(WorkBudget::new(limits))),
            runtime,
            id,
        }
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) async fn charge(&self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        let result = self.work.lock().await.charge(dimension, amount);
        if result.is_err() {
            self.runtime.metrics().record(MetricObservation {
                kind: MetricKind::Budget,
                value: 1,
                duration: None,
                outcome: None,
                cache: None,
            });
        }
        result
    }

    pub(crate) async fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            used: self.work.lock().await.used,
        }
    }

    pub(crate) async fn charge_cached(&self, usage: BudgetUsage) -> Result<()> {
        for dimension in CACHED_SEMANTIC_DIMENSIONS {
            self.charge(dimension, usage.amount(dimension)).await?;
        }
        Ok(())
    }
}

impl BudgetUsage {
    pub(crate) fn semantic_delta(self, earlier: Self, depth: u64) -> Self {
        let mut used = [0; 14];
        for dimension in CACHED_SEMANTIC_DIMENSIONS {
            let index = dimension as usize;
            used[index] = self.used[index].saturating_sub(earlier.used[index]);
        }
        used[BudgetDimension::Depth as usize] = depth;
        Self { used }
    }

    pub(crate) const fn amount(self, dimension: BudgetDimension) -> u64 {
        self.used[dimension as usize]
    }
}

impl WorkBudget {
    pub(crate) const fn new(limits: OperationLimits) -> Self {
        Self {
            limits,
            used: [0; 14],
        }
    }

    pub(crate) fn charge(&mut self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        let index = dimension as usize;
        // Depth is a high-water mark. Summing independent path/tree walks
        // makes ordinary wide repositories fail before their work budgets do.
        let actual = if dimension == BudgetDimension::Depth {
            self.used[index].max(amount)
        } else {
            self.used[index]
                .checked_add(amount)
                .ok_or(Error::LimitExceeded {
                    limit: dimension.label(),
                    actual: u64::MAX,
                    maximum: self.maximum(dimension),
                })?
        };
        let maximum = self.maximum(dimension);
        if actual > maximum {
            return Err(Error::LimitExceeded {
                limit: dimension.label(),
                actual,
                maximum,
            });
        }
        self.used[index] = actual;
        Ok(())
    }

    const fn maximum(&self, dimension: BudgetDimension) -> u64 {
        match dimension {
            BudgetDimension::LogicalObjects => self.limits.max_logical_objects,
            BudgetDimension::StorageRequests => self.limits.max_storage_requests,
            BudgetDimension::FetchedBytes => self.limits.max_fetched_bytes,
            BudgetDimension::InflatedBytes => self.limits.max_inflated_bytes,
            BudgetDimension::Depth => self.limits.max_depth,
            BudgetDimension::Entries => self.limits.max_entries,
            BudgetDimension::HistoryCommits => self.limits.max_history_commits,
            BudgetDimension::DiffInputBytes => self.limits.max_diff_input_bytes,
            BudgetDimension::DiffOutputBytes => self.limits.max_diff_output_bytes,
            BudgetDimension::BlameLines => self.limits.max_blame_lines,
            BudgetDimension::BlameComparisons => self.limits.max_blame_comparison_cells,
            BudgetDimension::ArchiveEntries => self.limits.max_archive_entries,
            BudgetDimension::ArchiveBytes => self.limits.max_archive_bytes,
            BudgetDimension::ResponseBytes => self.limits.max_response_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_overflow_fails_closed_without_changing_other_dimensions() {
        let limits = OperationLimits {
            max_response_bytes: u64::MAX,
            ..OperationLimits::default()
        };
        let mut budget = WorkBudget::new(limits);
        budget
            .charge(BudgetDimension::ResponseBytes, u64::MAX)
            .expect("maximum charge");
        assert!(matches!(
            budget.charge(BudgetDimension::ResponseBytes, 1),
            Err(Error::LimitExceeded {
                limit: "response bytes",
                actual: u64::MAX,
                maximum: u64::MAX,
            })
        ));
        budget
            .charge(BudgetDimension::StorageRequests, 1)
            .expect("other dimension remains usable");
    }

    #[test]
    fn depth_tracks_maximum_observed_nesting_instead_of_total_walks() {
        let limits = OperationLimits {
            max_depth: 3,
            ..OperationLimits::default()
        };
        let mut budget = WorkBudget::new(limits);
        for depth in [1, 2, 3, 1, 3] {
            budget
                .charge(BudgetDimension::Depth, depth)
                .expect("depth within limit");
        }
        assert!(matches!(
            budget.charge(BudgetDimension::Depth, 4),
            Err(Error::LimitExceeded {
                limit: "traversal depth",
                actual: 4,
                maximum: 3,
            })
        ));
    }
}
