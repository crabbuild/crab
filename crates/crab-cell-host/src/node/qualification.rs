//! Qualification runs over the node's runtime.

use super::*;

impl CellNode {
    /// Runs a deterministic qualification workload while this node is ready.
    ///
    /// The typed executor remains responsible for primitive requests and
    /// verification. The host only admits a run while serving and rejects a
    /// result if shutdown or a supervisor failure removed readiness during the
    /// run, so callers cannot retain evidence from a draining node.
    pub async fn run_qualification<E>(
        &self,
        workload: &QualificationWorkload,
        executor: &mut E,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload.run_with_case_coverage(executor).await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
    }
    /// Runs an observed workload while this node is ready without asserting
    /// that every scheduled lifecycle case was exercised.
    ///
    /// This entry point is for local wiring and smoke evidence. Its result must
    /// not be promoted to a protected profile unless the resulting artifact
    /// proves the profile's required case coverage independently.
    pub async fn run_qualification_observed<E>(
        &self,
        workload: &QualificationWorkload,
        executor: &mut E,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload.run(executor).await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
    }
    /// Runs independent qualification operations with bounded concurrency.
    ///
    /// The application-specific executor must make each scheduled operation
    /// independent or idempotent. Readiness is checked before admission and
    /// after all in-flight work drains, so a result from a draining or failed
    /// node is never accepted as qualification evidence.
    pub async fn run_qualification_concurrent<E>(
        &self,
        workload: &QualificationWorkload,
        executor: E,
        concurrency: usize,
    ) -> crab_cell_runtime::Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor + Clone + Send + 'static,
    {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        let summary = workload
            .run_concurrent_with_case_coverage(executor, concurrency)
            .await?;
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        Ok(summary)
    }
}
