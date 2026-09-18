use std::{sync::Arc, time::Duration};

use crate::DurabilitySource;

/// Bounded operational events emitted by the Cell durability runtime.
///
/// Implementations must keep labels finite and must not block the Cell actor.
pub trait CellTelemetry: Send + Sync {
    /// Records one completed fleet or object durability proof.
    fn durability_proof(&self, _source: DurabilitySource, _waited: Duration) {}

    /// Records bytes sent to follower append lanes and whether every lane acknowledged them.
    fn node_log_append(&self, _acknowledged: bool, _bytes: u64) {}
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
}
