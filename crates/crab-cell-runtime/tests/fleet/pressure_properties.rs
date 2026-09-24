//! Properties of the hysteretic pressure classifier that drives shedding.
//!
//! The classifier turns a node's own ledger samples into the shedding decision,
//! so its contracts are worth pinning beyond the scenario cases: a single
//! elevated sample cannot shed, critical pressure needs memory and disk, a calm
//! trace never sheds, and a stale sample cannot report a recovered node.

use crab_cell_runtime::fleet::pressure::{PressureClassifier, PressureSample, PressureState};
use proptest::prelude::*;

/// The thresholds the actor builds its classifier with.
const ENTER_PERMILLE: u16 = 800;
const EXIT_PERMILLE: u16 = 600;
const DWELL_MS: i64 = 1_000;

type Reading = (u16, u16, u16);

fn trace() -> impl Strategy<Value = Vec<Reading>> {
    prop::collection::vec((0u16..1_000, 0u16..1_000, 0u16..1_000), 1..8)
}

fn sample(at_ms: i64, (memory, disk, jobs): Reading) -> PressureSample {
    PressureSample {
        at_ms,
        memory_used_permille: memory,
        disk_used_permille: disk,
        jobs_used_permille: jobs,
        stale: false,
    }
}

fn elevated((memory, disk, jobs): Reading) -> bool {
    memory >= ENTER_PERMILLE || disk >= ENTER_PERMILLE || jobs >= ENTER_PERMILLE
}

fn classifier() -> PressureClassifier {
    PressureClassifier::new(ENTER_PERMILLE, EXIT_PERMILLE, DWELL_MS)
        .expect("the production thresholds are valid")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Shedding needs sustained evidence: the tier can only appear at least one
    /// dwell window after the first elevated sample, so a brief spike cannot
    /// evict a Cell.
    #[test]
    fn shedding_needs_a_dwell_after_the_first_elevated_sample(
        readings in trace(),
        step_ms in 1i64..2_000,
    ) {
        let mut classifier = classifier();
        let mut first_elevated_at = None;
        for (index, reading) in readings.iter().enumerate() {
            let at_ms = index as i64 * step_ms;
            if elevated(*reading) && first_elevated_at.is_none() {
                first_elevated_at = Some(at_ms);
            }
            let state = classifier.observe(sample(at_ms, *reading)).expect("time advances");
            if matches!(state, PressureState::Shedding | PressureState::Critical) {
                let first = first_elevated_at.expect("shedding implies an elevated sample");
                prop_assert!(at_ms - first >= DWELL_MS, "shed at {} after {}", at_ms, first);
            }
        }
    }

    /// Critical pressure is reserved for a node whose memory *and* disk crossed
    /// the enter threshold together and then held for a dwell window. The tier
    /// deliberately lags the current sample while it waits out that window, so
    /// the assertion is about sustained evidence rather than the latest reading.
    #[test]
    fn critical_pressure_needs_sustained_memory_and_disk(
        readings in trace(),
        step_ms in 1i64..2_000,
    ) {
        let mut classifier = classifier();
        let mut first_critical_evidence = None;
        for (index, reading) in readings.iter().enumerate() {
            let at_ms = index as i64 * step_ms;
            if reading.0 >= ENTER_PERMILLE
                && reading.1 >= ENTER_PERMILLE
                && first_critical_evidence.is_none()
            {
                first_critical_evidence = Some(at_ms);
            }
            let state = classifier
                .observe(sample(at_ms, *reading))
                .expect("time advances");
            if state == PressureState::Critical {
                let first = first_critical_evidence
                    .expect("critical pressure implies memory and disk were elevated");
                prop_assert!(
                    at_ms - first >= DWELL_MS,
                    "critical at {} after {}",
                    at_ms,
                    first
                );
            }
        }
    }

    /// A trace that never crosses the enter threshold never sheds.
    #[test]
    fn a_calm_trace_never_sheds(
        readings in prop::collection::vec(
            (
                0u16..ENTER_PERMILLE,
                0u16..ENTER_PERMILLE,
                0u16..ENTER_PERMILLE,
            ),
            1..8,
        ),
        step_ms in 1i64..2_000,
    ) {
        let mut classifier = classifier();
        for (index, reading) in readings.iter().enumerate() {
            let state = classifier
                .observe(sample(index as i64 * step_ms, *reading))
                .expect("time advances");
            prop_assert!(!matches!(state, PressureState::Shedding | PressureState::Critical));
        }
    }

    /// A stale sample can pace work but can never report a recovered node.
    #[test]
    fn a_stale_sample_never_reports_normal(readings in trace()) {
        let mut classifier = classifier();
        for (index, reading) in readings.iter().enumerate() {
            let mut stale = sample(index as i64 * DWELL_MS, *reading);
            stale.stale = true;
            let state = classifier.observe(stale).expect("time advances");
            prop_assert_ne!(state, PressureState::Normal);
        }
    }
}
