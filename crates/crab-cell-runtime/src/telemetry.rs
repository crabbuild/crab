use std::{sync::Arc, time::Duration};

use crate::DurabilitySource;

/// Outcome of an actor-owned resident route lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentRouteOutcome {
    Hit,
    Miss,
    Refused,
}

/// Bounded operational events emitted by the Cell durability runtime.
///
/// Implementations must keep labels finite and must not block the Cell actor.
pub trait CellTelemetry: Send + Sync {
    /// Records one completed fleet or object durability proof.
    fn durability_proof(&self, _source: DurabilitySource, _waited: Duration) {}

    /// Records bytes sent to follower append lanes and whether every lane acknowledged them.
    fn node_log_append(&self, _acknowledged: bool, _bytes: u64) {}

    /// Records a bounded-cardinality resident route result.
    fn resident_route(&self, _outcome: ResidentRouteOutcome) {}

    /// Records one finite LTX phase outcome.
    fn ltx_phase(&self, _phase: crab_ltx::LtxPhase, _elapsed: Duration, _succeeded: bool) {}

    /// Records bounded origin read requests and returned bytes.
    fn ltx_origin_read(&self, _origin: crab_ltx::LtxReadOrigin, _requests: u64, _bytes: u64) {}

    /// Aggregates one fixed-size capture ledger without dynamic labels.
    fn ltx_capture(&self, _timing: &crab_ltx::CaptureTiming) {}
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

    pub(crate) fn node_log_append(&self, acknowledged: bool, bytes: u64) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.node_log_append(acknowledged, bytes);
        }
    }

    pub(crate) fn resident_route(&self, outcome: ResidentRouteOutcome) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.resident_route(outcome);
            if outcome == ResidentRouteOutcome::Hit {
                telemetry.ltx_origin_read(crab_ltx::LtxReadOrigin::Resident, 1, 0);
            }
        }
    }

    fn ltx_phase(&self, phase: crab_ltx::LtxPhase, elapsed: Duration, succeeded: bool) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_phase(phase, elapsed, succeeded);
        }
    }

    fn ltx_origin_read(&self, origin: crab_ltx::LtxReadOrigin, requests: u64, bytes: u64) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_origin_read(origin, requests, bytes);
        }
    }

    pub(crate) fn ltx_capture(&self, timing: &crab_ltx::CaptureTiming) {
        if let Some(telemetry) = self.inner.get() {
            telemetry.ltx_capture(timing);
        }
    }
}

impl crab_ltx::LtxTelemetry for CellTelemetryHandle {
    fn phase(&self, phase: crab_ltx::LtxPhase, elapsed: Duration, succeeded: bool) {
        self.ltx_phase(phase, elapsed, succeeded);
    }

    fn origin_read(&self, origin: crab_ltx::LtxReadOrigin, requests: u64, bytes: u64) {
        self.ltx_origin_read(origin, requests, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingTelemetry {
        phases: Mutex<Vec<(crab_ltx::LtxPhase, bool)>>,
        reads: Mutex<Vec<(crab_ltx::LtxReadOrigin, u64, u64)>>,
    }

    impl CellTelemetry for RecordingTelemetry {
        fn ltx_phase(&self, phase: crab_ltx::LtxPhase, _: Duration, succeeded: bool) {
            self.phases.lock().unwrap().push((phase, succeeded));
        }

        fn ltx_origin_read(&self, origin: crab_ltx::LtxReadOrigin, requests: u64, bytes: u64) {
            self.reads.lock().unwrap().push((origin, requests, bytes));
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
        crab_ltx::LtxTelemetry::origin_read(&handle, crab_ltx::LtxReadOrigin::Hydrating, 3, 4_096);
        handle.resident_route(ResidentRouteOutcome::Hit);

        assert_eq!(
            *recording.phases.lock().unwrap(),
            vec![(crab_ltx::LtxPhase::Directory, true)]
        );
        assert_eq!(
            *recording.reads.lock().unwrap(),
            vec![
                (crab_ltx::LtxReadOrigin::Hydrating, 3, 4_096),
                (crab_ltx::LtxReadOrigin::Resident, 1, 0),
            ]
        );
    }
}
