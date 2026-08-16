//! Observability boundary for workflow execution.

/// Receives workflow executor counter updates.
///
/// The CLI implements this trait with its process-wide metrics collector, while
/// library consumers can omit the sink entirely.
pub trait WorkflowMetrics: Send + Sync {
    /// Records a locally executed stage.
    fn inc_workflow_stages_executed(&self);

    /// Records a stage restored from the local cache.
    fn inc_workflow_stage_cache_hits_local(&self);

    /// Records a stage restored from a remote cache.
    fn inc_workflow_stage_cache_hits_remote(&self);

    /// Records a failed stage attempt.
    fn inc_workflow_stages_failed(&self);

    /// Records a retry attempt after the first stage attempt.
    fn inc_workflow_stage_retry_attempts(&self);
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    use super::WorkflowMetrics;

    #[derive(Default)]
    pub(crate) struct TestWorkflowMetrics {
        workflow_stages_executed: AtomicU64,
        workflow_stage_cache_hits_local: AtomicU64,
        workflow_stage_cache_hits_remote: AtomicU64,
        workflow_stages_failed: AtomicU64,
        workflow_stage_retry_attempts: AtomicU64,
    }

    impl TestWorkflowMetrics {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn snapshot(&self) -> TestWorkflowMetricsSnapshot {
            TestWorkflowMetricsSnapshot {
                workflow_stages_executed: self.workflow_stages_executed.load(Relaxed),
                workflow_stage_cache_hits_local: self.workflow_stage_cache_hits_local.load(Relaxed),
                workflow_stage_cache_hits_remote: self
                    .workflow_stage_cache_hits_remote
                    .load(Relaxed),
                workflow_stages_failed: self.workflow_stages_failed.load(Relaxed),
                workflow_stage_retry_attempts: self.workflow_stage_retry_attempts.load(Relaxed),
            }
        }
    }

    impl WorkflowMetrics for TestWorkflowMetrics {
        fn inc_workflow_stages_executed(&self) {
            self.workflow_stages_executed.fetch_add(1, Relaxed);
        }

        fn inc_workflow_stage_cache_hits_local(&self) {
            self.workflow_stage_cache_hits_local.fetch_add(1, Relaxed);
        }

        fn inc_workflow_stage_cache_hits_remote(&self) {
            self.workflow_stage_cache_hits_remote.fetch_add(1, Relaxed);
        }

        fn inc_workflow_stages_failed(&self) {
            self.workflow_stages_failed.fetch_add(1, Relaxed);
        }

        fn inc_workflow_stage_retry_attempts(&self) {
            self.workflow_stage_retry_attempts.fetch_add(1, Relaxed);
        }
    }

    pub(crate) struct TestWorkflowMetricsSnapshot {
        pub(crate) workflow_stages_executed: u64,
        pub(crate) workflow_stage_cache_hits_local: u64,
        pub(crate) workflow_stage_cache_hits_remote: u64,
        pub(crate) workflow_stages_failed: u64,
        pub(crate) workflow_stage_retry_attempts: u64,
    }
}
