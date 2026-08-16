//! Adaptive dedup threshold using EWMA over recent push dedup ratios.
//!
//! The threshold controls how aggressively the packer deduplicates:
//! `effective = clamp(0.25 * (1.0 - ewma), 0.05, 0.50)`.
//!
//! When fewer than 3 samples exist, the threshold degrades to the fixed
//! v1 default of 0.25. State is persisted to `perf-state.json` so it
//! survives across sessions.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

/// Maximum number of push dedup ratios retained in the EWMA window.
const MAX_SAMPLES: usize = 16;

/// Minimum number of samples before the adaptive threshold activates.
const MIN_SAMPLES_FOR_ADAPTIVE: usize = 3;

/// Fixed v1 dedup threshold ratio (used as fallback).
const FIXED_THRESHOLD: f64 = 0.25;

/// Minimum effective threshold.
const MIN_SAVINGS: f64 = 0.05;

/// Maximum effective threshold.
const MAX_SAVINGS: f64 = 0.50;

/// Persisted state for the adaptive threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfState {
    /// Recent push dedup ratios (most recent last).
    samples: Vec<f64>,
}

/// Adaptive dedup threshold using EWMA over recent push dedup ratios.
pub struct AdaptiveThreshold {
    samples: Vec<f64>,
    max_samples: usize,
}

impl Default for AdaptiveThreshold {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveThreshold {
    /// Create a new threshold with no history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
            max_samples: MAX_SAMPLES,
        }
    }

    /// Load persisted state from a `perf-state.json` file.
    ///
    /// If the file doesn't exist or is malformed, returns a fresh instance
    /// and logs a warning.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Io`] only on unexpected I/O failures (not
    /// file-not-found, which is handled gracefully).
    pub fn load(path: &Path) -> Result<Self> {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no perf-state.json, starting fresh");
                return Ok(Self::new());
            }
            Err(e) => return Err(CrabError::Io(e)),
        };

        match serde_json::from_str::<PerfState>(&data) {
            Ok(state) => {
                let mut samples = state.samples;
                samples.truncate(MAX_SAMPLES);
                debug!(
                    path = %path.display(),
                    samples = samples.len(),
                    "loaded adaptive threshold state"
                );
                Ok(Self {
                    samples,
                    max_samples: MAX_SAMPLES,
                })
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "corrupt perf-state.json, starting fresh"
                );
                Ok(Self::new())
            }
        }
    }

    /// Persist current state to a `perf-state.json` file.
    ///
    /// Creates parent directories if needed.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Io`] on filesystem failure or
    /// [`CrabError::Internal`] on serialization failure.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let state = PerfState {
            samples: self.samples.clone(),
        };
        let json = serde_json::to_string_pretty(&state)
            .map_err(|e| CrabError::Internal(format!("serialize perf-state: {e}")))?;

        // Atomic write via tempfile + rename.
        let parent = path
            .parent()
            .ok_or_else(|| CrabError::Internal("perf-state path has no parent".into()))?;
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        std::fs::write(tmp.path(), json.as_bytes())?;
        tmp.persist(path)
            .map_err(|e| CrabError::Internal(format!("persist perf-state: {e}")))?;

        debug!(path = %path.display(), samples = self.samples.len(), "saved adaptive threshold state");
        Ok(())
    }

    /// Record a push's dedup ratio (0.0 = no dedup, 1.0 = full dedup).
    pub fn record(&mut self, dedup_ratio: f64) {
        self.samples.push(dedup_ratio.clamp(0.0, 1.0));
        if self.samples.len() > self.max_samples {
            self.samples.remove(0);
        }
    }

    /// Compute the effective dedup threshold.
    ///
    /// `effective = clamp(0.25 * (1.0 - ewma), 0.05, 0.50)`
    ///
    /// Degrades to the fixed 0.25 when fewer than 3 samples exist.
    #[must_use]
    pub fn effective(&self) -> f64 {
        if self.samples.len() < MIN_SAMPLES_FOR_ADAPTIVE {
            return FIXED_THRESHOLD;
        }

        let ewma = self.ewma();
        (FIXED_THRESHOLD * (1.0 - ewma)).clamp(MIN_SAVINGS, MAX_SAVINGS)
    }

    /// Compute the EWMA over the sample window.
    ///
    /// Uses a smoothing factor of `2 / (n + 1)` where `n` is the number
    /// of samples, applied in chronological order.
    #[must_use]
    pub fn ewma(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }

        let n = self.samples.len();
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample count is at most 16, well within f64 precision"
        )]
        let alpha = 2.0 / (n as f64 + 1.0);

        let mut ewma = self.samples[0];
        for &sample in &self.samples[1..] {
            ewma = alpha * sample + (1.0 - alpha) * ewma;
        }
        ewma
    }

    /// Number of recorded samples.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_threshold_has_no_samples() {
        let at = AdaptiveThreshold::new();
        assert_eq!(at.sample_count(), 0);
    }

    #[test]
    fn effective_degrades_to_fixed_with_few_samples() {
        let mut at = AdaptiveThreshold::new();
        assert_eq!(at.effective(), FIXED_THRESHOLD);

        at.record(0.5);
        assert_eq!(at.effective(), FIXED_THRESHOLD);

        at.record(0.8);
        assert_eq!(at.effective(), FIXED_THRESHOLD);
    }

    #[test]
    fn effective_activates_at_three_samples() {
        let mut at = AdaptiveThreshold::new();
        at.record(0.5);
        at.record(0.5);
        at.record(0.5);

        // With 3 samples all at 0.5, ewma ≈ 0.5.
        // effective = clamp(0.25 * (1.0 - 0.5), 0.05, 0.50) = 0.125
        let eff = at.effective();
        assert!(eff > MIN_SAVINGS);
        assert!(eff < MAX_SAVINGS);
        assert!(eff != FIXED_THRESHOLD);
    }

    #[test]
    fn effective_clamps_to_min() {
        let mut at = AdaptiveThreshold::new();
        // Very high dedup ratio → low threshold, clamped to MIN_SAVINGS.
        for _ in 0..5 {
            at.record(0.99);
        }
        let eff = at.effective();
        assert!((eff - MIN_SAVINGS).abs() < 1e-10);
    }

    #[test]
    fn effective_clamps_to_max() {
        let mut at = AdaptiveThreshold::new();
        // Very low dedup ratio → high threshold, but 0.25*(1-0) = 0.25 < MAX_SAVINGS.
        // To get MAX_SAVINGS we'd need negative ewma, which is clamped.
        // With ewma = 0.0: effective = 0.25 * 1.0 = 0.25.
        for _ in 0..5 {
            at.record(0.0);
        }
        let eff = at.effective();
        assert!((eff - 0.25).abs() < 1e-10);
    }

    #[test]
    fn record_caps_at_max_samples() {
        let mut at = AdaptiveThreshold::new();
        for i in 0..20 {
            #[expect(clippy::cast_precision_loss, reason = "test values")]
            at.record(i as f64 / 20.0);
        }
        assert_eq!(at.sample_count(), MAX_SAMPLES);
    }

    #[test]
    fn record_clamps_input() {
        let mut at = AdaptiveThreshold::new();
        at.record(-0.5);
        at.record(1.5);
        assert_eq!(at.samples[0], 0.0);
        assert_eq!(at.samples[1], 1.0);
    }

    #[test]
    fn ewma_empty_is_zero() {
        let at = AdaptiveThreshold::new();
        assert_eq!(at.ewma(), 0.0);
    }

    #[test]
    fn ewma_single_sample() {
        let mut at = AdaptiveThreshold::new();
        at.record(0.7);
        assert!((at.ewma() - 0.7).abs() < 1e-10);
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("perf-state.json");

        let mut at = AdaptiveThreshold::new();
        at.record(0.3);
        at.record(0.5);
        at.record(0.7);
        at.save(&path).expect("save");

        let loaded = AdaptiveThreshold::load(&path).expect("load");
        assert_eq!(loaded.sample_count(), 3);
        assert!((loaded.ewma() - at.ewma()).abs() < 1e-10);
        assert!((loaded.effective() - at.effective()).abs() < 1e-10);
    }

    #[test]
    fn load_missing_file_returns_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent.json");
        let at = AdaptiveThreshold::load(&path).expect("load");
        assert_eq!(at.sample_count(), 0);
    }

    #[test]
    fn load_corrupt_file_returns_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("perf-state.json");
        std::fs::write(&path, "not valid json {{{").expect("write");

        let at = AdaptiveThreshold::load(&path).expect("load");
        assert_eq!(at.sample_count(), 0);
    }

    #[test]
    fn effective_bounds_always_hold() {
        // Test a range of dedup ratios to verify bounds.
        for ratio_pct in 0..=100 {
            let mut at = AdaptiveThreshold::new();
            #[expect(clippy::cast_precision_loss, reason = "test values")]
            let ratio = ratio_pct as f64 / 100.0;
            for _ in 0..5 {
                at.record(ratio);
            }
            let eff = at.effective();
            assert!(
                eff >= MIN_SAVINGS,
                "effective {eff} < MIN_SAVINGS {MIN_SAVINGS} for ratio {ratio}"
            );
            assert!(
                eff <= MAX_SAVINGS,
                "effective {eff} > MAX_SAVINGS {MAX_SAVINGS} for ratio {ratio}"
            );
        }
    }
}
