//! Captured workflow results. These re-exported schema-v1 values are part of the
//! SDK contract; workflow schema changes require matching SDK compatibility review.
pub use crab_workflow::results::{
    Attachment, AttachmentKind, Availability, AxisRange, Chart, ChartImage, ChartInteraction,
    ChartReduction, Column, Comparison, ComparisonCell, ComparisonRow, ComparisonSpec,
    ComparisonView, DatasetIdentity, Datum, Direction, EvidenceView, InputWitness, OutputWitness,
    Report, ReportView, ReproductionSpec, ResultOrigin, ResultSnapshot, ResultsRead,
    ResultsResponse, RunPage, RunQuery, RunSelection, RunSummary, RuntimeWitness, SealedResult,
    StageResult,
};

#[derive(Clone, Copy)]
pub struct Results<'a> {
    pub(crate) owner: &'a crate::client::RemoteRepository,
}

impl<'a> Results<'a> {
    /// Read a comparison, report or linked evidence with shared workflow semantics.
    pub fn read(
        &self,
        request: ResultsRead,
    ) -> crate::Request<'a, ResultsResponse, crate::OperationOptions> {
        let owner = self.owner;
        crate::Request::new(move |options| Box::pin(owner.results_read(request, options)))
    }
    /// Read and verify one retained attachment, bounded to the Results object limit.
    pub fn blob(&self, hash: String) -> crate::Request<'a, Vec<u8>, crate::OperationOptions> {
        let owner = self.owner;
        crate::Request::new(move |options| Box::pin(owner.results_blob(hash, options)))
    }
}
