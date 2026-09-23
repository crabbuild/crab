//! Pressure shedding tests extracted from `src/pressure.rs`.

use crab_cell_runtime::*;

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
