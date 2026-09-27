//! Node-log durability, fleet proofs, and byte admission.

use super::*;
use crab_cell_runtime::fleet::telemetry::{CellTelemetry, CommandResponseSource};

mod admission;
mod proofs;
mod recovery;

#[derive(Default)]
pub(super) struct RecordingResponses(pub(super) Mutex<Vec<CommandResponseSource>>);

impl CellTelemetry for RecordingResponses {
    fn command_response(
        &self,
        source: CommandResponseSource,
        elapsed: std::time::Duration,
        confirmation: std::time::Duration,
    ) {
        assert!(confirmation <= elapsed);
        if source == CommandResponseSource::Recorded {
            assert!(confirmation.is_zero());
        }
        self.0.lock().unwrap().push(source);
    }
}
