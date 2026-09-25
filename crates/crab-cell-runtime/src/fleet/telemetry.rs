//! Bounded operational telemetry emitted by the runtime.
use std::{sync::Arc, time::Duration};

use crate::fleet::pressure::PressureState;
use crate::node::log::DurabilitySource;

/// Outcome of an actor-owned resident route lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentRouteOutcome {
    /// The route was resident and usable.
    Hit,
    /// No resident route exists for the Cell.
    Miss,
    /// A resident route exists but refused the request.
    Refused,
}

/// Outcome of one commit's attempt to use the node's enrolled follower lane.
///
/// A commit that cannot use its lane still succeeds through object coverage, so
/// `Unavailable` and `Rejected` are the only signals that a node intended fleet
/// durability and silently fell back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilitySubmissionOutcome {
    /// An enrolled lane accepted the captured commit for shipping.
    Fleet,
    /// This host installs no node-log durability provider at all.
    Unsupported,
    /// A provider exists, but no lane is enrolled yet.
    Unavailable,
    /// The enrolled lane refused or fenced the submission.
    Rejected,
}

/// Kind of one registered primitive call observed at the execution boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveOperationKind {
    /// A registered command.
    Command,
    /// A registered query.
    Query,
}

/// Terminal outcome of one registered primitive call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveOperationOutcome {
    /// The handler committed a result the caller consumes as success.
    Success,
    /// The handler committed a rejection the caller consumes as the result.
    Rejected,
    /// The operation failed before a committed result existed.
    Failed,
}

impl From<&crate::Result<crate::cell::executor::HandlerOutcome>> for PrimitiveOperationOutcome {
    fn from(result: &crate::Result<crate::cell::executor::HandlerOutcome>) -> Self {
        match result {
            Ok(crate::cell::executor::HandlerOutcome::Success(_)) => Self::Success,
            Ok(crate::cell::executor::HandlerOutcome::Rejected(_)) => Self::Rejected,
            Err(_) => Self::Failed,
        }
    }
}

/// Bounded operational events emitted by the Cell durability runtime.
///
/// Implementations must keep labels finite and must not block the Cell actor.
pub trait CellTelemetry: Send + Sync {
    /// Records one registered primitive call by owning module and outcome.
    fn primitive_operation(
        &self,
        _module: &'static str,
        _kind: PrimitiveOperationKind,
        _outcome: PrimitiveOperationOutcome,
        _elapsed: Duration,
    ) {
    }

    /// Records one completed fleet or object durability proof.
    fn durability_proof(&self, _source: DurabilitySource, _waited: Duration) {}

    /// Records how one commit's node-log submission resolved.
    fn durability_submission(&self, _outcome: DurabilitySubmissionOutcome) {}

    /// Records the immutable objects and bytes one preparation attempt uploaded.
    ///
    /// The count covers every object a Cell root needs — segment bodies,
    /// indexes, directory nodes, root documents, segment pages, bundle bodies,
    /// and compaction outputs — so an operator can size object-store cost per
    /// command instead of inferring it from the database size. Failed attempts
    /// record the objects they did upload.
    fn publication_cost(&self, _objects: u64, _bytes: u64) {}

    /// Records bytes sent to follower append lanes and whether every lane acknowledged them.
    fn node_log_append(&self, _acknowledged: bool, _bytes: u64) {}

    /// Records a bounded-cardinality resident route result.
    fn resident_route(&self, _outcome: ResidentRouteOutcome) {}

    /// Records one finite LTX phase outcome.
    fn ltx_phase(&self, _phase: crab_ltx::LtxPhase, _elapsed: Duration, _succeeded: bool) {}

    /// Records one logical read attributed to a bounded residency class.
    fn ltx_logical_read(&self, _origin: crab_ltx::LtxReadOrigin) {}

    /// Records one provider attempt and the bytes returned before its outcome.
    fn ltx_origin_request(
        &self,
        _origin: crab_ltx::LtxReadOrigin,
        _outcome: crab_ltx::LtxRequestOutcome,
        _bytes: u64,
    ) {
    }

    /// Aggregates one fixed-size capture ledger without dynamic labels.
    fn ltx_capture(&self, _timing: &crab_ltx::CaptureTiming, _succeeded: bool) {}

    /// Records the node's current hysteretic pressure tier.
    ///
    /// One node reports one tier at a time, and the classifier only hands out
    /// `Normal`, `Constrained`, `Shedding`, or `Critical`, so a sink can render
    /// this as a bounded gauge family instead of a growing label set. The tier a
    /// node reports is the one that decides whether it sheds settled Cells.
    fn pressure_state(&self, _state: PressureState) {}
}

/// Shared late-bound telemetry sink used by runtime components.
#[derive(Clone, Default)]
pub struct CellTelemetryHandle {
    inner: Arc<std::sync::OnceLock<Arc<dyn CellTelemetry>>>,
}

impl CellTelemetryHandle {
    pub(crate) fn install(&self, telemetry: Arc<dyn CellTelemetry>) -> crate::Result<()> {
        self.inner
            .set(telemetry)
            .map_err(|_| crate::Error::Control("Cell telemetry was initialized twice"))
    }

    pub(crate) fn durability_proof(&self, source: DurabilitySource, waited: Duration) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.durability_proof(source, waited);
        }
    }

    pub(crate) fn primitive_operation(
        &self,
        module: &'static str,
        kind: PrimitiveOperationKind,
        outcome: PrimitiveOperationOutcome,
        elapsed: Duration,
    ) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.primitive_operation(module, kind, outcome, elapsed);
        }
    }

    pub(crate) fn durability_submission(&self, outcome: DurabilitySubmissionOutcome) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.durability_submission(outcome);
        }
    }

    pub(crate) fn publication_cost(&self, objects: u64, bytes: u64) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.publication_cost(objects, bytes);
        }
    }

    pub(crate) fn node_log_append(&self, acknowledged: bool, bytes: u64) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.node_log_append(acknowledged, bytes);
        }
    }

    pub(crate) fn pressure_state(&self, state: PressureState) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.pressure_state(state);
        }
    }

    pub(crate) fn resident_route(&self, outcome: ResidentRouteOutcome) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.resident_route(outcome);
            if outcome == ResidentRouteOutcome::Hit {
                telemetry.ltx_logical_read(crab_ltx::LtxReadOrigin::Resident);
            }
        }
    }

    fn ltx_phase(&self, phase: crab_ltx::LtxPhase, elapsed: Duration, succeeded: bool) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_phase(phase, elapsed, succeeded);
        }
    }

    fn ltx_logical_read(&self, origin: crab_ltx::LtxReadOrigin) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_logical_read(origin);
        }
    }

    fn ltx_origin_request(
        &self,
        origin: crab_ltx::LtxReadOrigin,
        outcome: crab_ltx::LtxRequestOutcome,
        bytes: u64,
    ) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_origin_request(origin, outcome, bytes);
        }
    }

    fn ltx_capture(&self, timing: &crab_ltx::CaptureTiming, succeeded: bool) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_capture(timing, succeeded);
        }
    }
}

impl crab_ltx::LtxTelemetry for CellTelemetryHandle {
    fn phase(&self, phase: crab_ltx::LtxPhase, elapsed: Duration, succeeded: bool) {
        self.ltx_phase(phase, elapsed, succeeded);
    }

    fn logical_read(&self, origin: crab_ltx::LtxReadOrigin) {
        self.ltx_logical_read(origin);
    }

    fn origin_request(
        &self,
        origin: crab_ltx::LtxReadOrigin,
        outcome: crab_ltx::LtxRequestOutcome,
        bytes: u64,
    ) {
        self.ltx_origin_request(origin, outcome, bytes);
    }

    fn capture(&self, timing: &crab_ltx::CaptureTiming, succeeded: bool) {
        self.ltx_capture(timing, succeeded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingTelemetry {
        phases: Mutex<Vec<(crab_ltx::LtxPhase, bool)>>,
        logical_reads: Mutex<Vec<crab_ltx::LtxReadOrigin>>,
        requests: Mutex<Vec<(crab_ltx::LtxReadOrigin, crab_ltx::LtxRequestOutcome, u64)>>,
        submissions: Mutex<Vec<DurabilitySubmissionOutcome>>,
    }

    impl CellTelemetry for RecordingTelemetry {
        fn durability_submission(&self, outcome: DurabilitySubmissionOutcome) {
            self.submissions.lock().unwrap().push(outcome);
        }

        fn ltx_phase(&self, phase: crab_ltx::LtxPhase, _: Duration, succeeded: bool) {
            self.phases.lock().unwrap().push((phase, succeeded));
        }

        fn ltx_logical_read(&self, origin: crab_ltx::LtxReadOrigin) {
            self.logical_reads.lock().unwrap().push(origin);
        }

        fn ltx_origin_request(
            &self,
            origin: crab_ltx::LtxReadOrigin,
            outcome: crab_ltx::LtxRequestOutcome,
            bytes: u64,
        ) {
            self.requests.lock().unwrap().push((origin, outcome, bytes));
        }
    }

    #[test]
    fn ltx_bridge_preserves_only_finite_runtime_dimensions() {
        let handle = CellTelemetryHandle::default();
        let recording = Arc::new(RecordingTelemetry::default());
        handle.install(recording.clone()).unwrap();

        crab_ltx::LtxTelemetry::phase(
            &handle,
            crab_ltx::LtxPhase::Directory,
            Duration::from_millis(2),
            true,
        );
        crab_ltx::LtxTelemetry::origin_request(
            &handle,
            crab_ltx::LtxReadOrigin::Hydrating,
            crab_ltx::LtxRequestOutcome::Failed,
            4_096,
        );
        handle.resident_route(ResidentRouteOutcome::Hit);
        handle.durability_submission(DurabilitySubmissionOutcome::Unavailable);
        handle.durability_submission(DurabilitySubmissionOutcome::Fleet);

        assert_eq!(
            *recording.phases.lock().unwrap(),
            vec![(crab_ltx::LtxPhase::Directory, true)]
        );
        assert_eq!(
            *recording.logical_reads.lock().unwrap(),
            vec![crab_ltx::LtxReadOrigin::Resident]
        );
        assert_eq!(
            *recording.requests.lock().unwrap(),
            vec![(
                crab_ltx::LtxReadOrigin::Hydrating,
                crab_ltx::LtxRequestOutcome::Failed,
                4_096,
            )]
        );
        assert_eq!(
            *recording.submissions.lock().unwrap(),
            vec![
                DurabilitySubmissionOutcome::Unavailable,
                DurabilitySubmissionOutcome::Fleet,
            ]
        );
    }
}
