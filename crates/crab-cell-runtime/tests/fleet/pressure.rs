//! Pressure shedding tests extracted from `src/pressure.rs`.
use std::sync::{Arc, Mutex};

use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::fleet::pressure::{
    MovementBudget, PressureClassifier, PressureSample, PressureState,
};
use crab_cell_runtime::fleet::telemetry::CellTelemetry;
use crab_cell_runtime::identity::SessionId;

/// Records the pressure tiers one node reports to its telemetry sink.
#[derive(Default)]
struct RecordingPressureTelemetry {
    states: Mutex<Vec<PressureState>>,
}

impl CellTelemetry for RecordingPressureTelemetry {
    fn pressure_state(&self, state: PressureState) {
        self.states.lock().unwrap().push(state);
    }
}

impl RecordingPressureTelemetry {
    fn observed(&self) -> Vec<PressureState> {
        self.states.lock().unwrap().clone()
    }
}

fn sample(at_ms: i64, memory: u16) -> PressureSample {
    PressureSample {
        at_ms,
        memory_used_permille: memory,
        disk_used_permille: 100,
        jobs_used_permille: 100,
        stale: false,
    }
}

#[test]
fn pressure_requires_dwell_and_hysteresis() {
    let mut classifier = PressureClassifier::new(800, 600, 10).unwrap();
    assert_eq!(
        classifier.observe(sample(0, 850)).unwrap(),
        PressureState::Normal
    );
    assert_eq!(
        classifier.observe(sample(9, 850)).unwrap(),
        PressureState::Normal
    );
    assert_eq!(
        classifier.observe(sample(10, 850)).unwrap(),
        PressureState::Shedding
    );
    assert_eq!(
        classifier.observe(sample(11, 700)).unwrap(),
        PressureState::Shedding
    );
    assert_eq!(
        classifier.observe(sample(20, 500)).unwrap(),
        PressureState::Shedding
    );
    assert_eq!(
        classifier.observe(sample(29, 500)).unwrap(),
        PressureState::Shedding
    );
    assert_eq!(
        classifier.observe(sample(30, 500)).unwrap(),
        PressureState::Normal
    );
}

#[test]
fn stale_samples_cannot_recover_or_move_time_backwards() {
    let mut classifier = PressureClassifier::new(800, 600, 10).unwrap();
    classifier.observe(sample(0, 850)).unwrap();
    assert_eq!(
        classifier
            .observe(PressureSample {
                stale: true,
                ..sample(10, 500)
            })
            .unwrap(),
        PressureState::Constrained
    );
    assert!(classifier.observe(sample(9, 500)).is_err());
}

#[test]
fn movement_budget_bounds_concurrency_and_rate() {
    let mut budget = MovementBudget::new(1, 100).unwrap();
    let mut permit = budget.try_start(0).unwrap();
    assert!(budget.try_start(0).is_err());
    budget.complete(&mut permit);
    assert!(budget.try_start(0).is_err());
    let _next = budget.try_start(100).unwrap();
    assert_eq!(budget.in_flight(), 1);
}

#[tokio::test]
async fn node_reports_every_pressure_tier_it_classifies() {
    let session = SessionId::from_bytes([123; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1_024, session).unwrap();
    let recording = Arc::new(RecordingPressureTelemetry::default());
    runtime.install_telemetry(recording.clone()).unwrap();

    // The actor samples this node's own ledger on the wall clock, so keep the
    // observations ahead of anything its tick could already have reported.
    let base_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
        + 5_000;
    let high = PressureSample {
        at_ms: base_ms,
        memory_used_permille: 900,
        disk_used_permille: 100,
        jobs_used_permille: 100,
        stale: false,
    };
    assert_eq!(
        runtime.observe_pressure(high).await.unwrap(),
        PressureState::Normal
    );
    assert_eq!(
        runtime
            .observe_pressure(PressureSample {
                at_ms: base_ms + 1_000,
                ..high
            })
            .await
            .unwrap(),
        PressureState::Shedding
    );

    let observed = recording.observed();
    assert!(observed.contains(&PressureState::Normal), "{observed:?}");
    assert!(observed.contains(&PressureState::Shedding), "{observed:?}");
    runtime.shutdown().await.unwrap();
}
