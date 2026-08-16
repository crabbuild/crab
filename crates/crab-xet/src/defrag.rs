//! Rolling fragmentation estimation for xorb packing.

use std::collections::VecDeque;

/// Rolling fragmentation estimator with hysteresis.
///
/// Tracks chunks-per-range over a sliding window and decides whether dedup
/// should be allowed on the next range. Suppressing dedup can keep chunks from
/// the same source range together when fragmentation is already high.
pub struct DefragPrevention {
    rolling_last_nranges: VecDeque<usize>,
    rolling_nranges_chunks: usize,
    window_size: usize,
    defrag_at_low_threshold: bool,
    min_chunks_per_range: f32,
    hysteresis_factor: f32,
}

impl DefragPrevention {
    /// Create a new estimator with explicit parameters.
    #[must_use]
    pub fn new(window_size: usize, min_chunks_per_range: f32, hysteresis_factor: f32) -> Self {
        Self {
            rolling_last_nranges: VecDeque::with_capacity(window_size),
            rolling_nranges_chunks: 0,
            window_size,
            defrag_at_low_threshold: true,
            min_chunks_per_range,
            hysteresis_factor,
        }
    }

    /// Record a completed range with `nchunks` chunks.
    pub fn add_range_to_fragmentation_estimate(&mut self, nchunks: usize) {
        self.rolling_last_nranges.push_back(nchunks);
        self.rolling_nranges_chunks += nchunks;
        if self.rolling_last_nranges.len() > self.window_size
            && let Some(evicted) = self.rolling_last_nranges.pop_front()
        {
            self.rolling_nranges_chunks -= evicted;
        }
    }

    fn rolling_chunks_per_range(&self) -> Option<f32> {
        if self.rolling_last_nranges.len() < self.window_size {
            None
        } else {
            Some(self.rolling_nranges_chunks as f32 / self.rolling_last_nranges.len() as f32)
        }
    }

    /// Decide whether dedup should be allowed for the next range.
    pub fn allow_dedup_on_next_range(&mut self, dedup_range_size: usize) -> bool {
        let Some(chunks_per_range) = self.rolling_chunks_per_range() else {
            return true;
        };

        let target_cpr = if self.defrag_at_low_threshold {
            self.min_chunks_per_range * self.hysteresis_factor
        } else {
            self.min_chunks_per_range
        };

        if chunks_per_range < target_cpr {
            if (dedup_range_size as f32) < chunks_per_range {
                self.defrag_at_low_threshold = false;
                return false;
            }
        } else {
            self.defrag_at_low_threshold = true;
        }

        true
    }
}

impl Default for DefragPrevention {
    fn default() -> Self {
        Self::new(10, 16.0, 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_construction() {
        let dp = DefragPrevention::default();
        assert_eq!(dp.window_size, 10);
        assert_eq!(dp.min_chunks_per_range, 16.0);
        assert_eq!(dp.hysteresis_factor, 0.5);
        assert!(dp.defrag_at_low_threshold);
        assert_eq!(dp.rolling_last_nranges.len(), 0);
        assert_eq!(dp.rolling_nranges_chunks, 0);
    }

    #[test]
    fn window_fills_and_rolls_over() {
        let mut dp = DefragPrevention::new(3, 16.0, 0.5);

        dp.add_range_to_fragmentation_estimate(10);
        dp.add_range_to_fragmentation_estimate(20);
        dp.add_range_to_fragmentation_estimate(30);
        assert_eq!(dp.rolling_last_nranges.len(), 3);
        assert_eq!(dp.rolling_nranges_chunks, 60);

        dp.add_range_to_fragmentation_estimate(40);
        assert_eq!(dp.rolling_last_nranges.len(), 3);
        assert_eq!(dp.rolling_nranges_chunks, 90);
    }

    #[test]
    fn allow_dedup_when_window_not_full() {
        let mut dp = DefragPrevention::new(5, 16.0, 0.5);
        dp.add_range_to_fragmentation_estimate(1);
        assert!(dp.allow_dedup_on_next_range(1));
        assert!(dp.allow_dedup_on_next_range(100));
    }

    #[test]
    fn allow_dedup_with_healthy_fragmentation() {
        let mut dp = DefragPrevention::new(3, 10.0, 0.5);
        for _ in 0..3 {
            dp.add_range_to_fragmentation_estimate(20);
        }
        assert!(dp.allow_dedup_on_next_range(5));
        assert!(dp.allow_dedup_on_next_range(1));
    }

    #[test]
    fn suppress_dedup_when_fragmented() {
        let mut dp = DefragPrevention::new(3, 10.0, 0.5);
        for _ in 0..3 {
            dp.add_range_to_fragmentation_estimate(2);
        }
        assert!(!dp.allow_dedup_on_next_range(1));
    }

    #[test]
    fn hysteresis_prevents_oscillation() {
        let mut dp = DefragPrevention::new(3, 10.0, 0.5);

        for _ in 0..3 {
            dp.add_range_to_fragmentation_estimate(4);
        }
        assert!(!dp.allow_dedup_on_next_range(1));
        assert!(!dp.defrag_at_low_threshold);

        dp.add_range_to_fragmentation_estimate(7);
        dp.add_range_to_fragmentation_estimate(7);
        dp.add_range_to_fragmentation_estimate(7);
        assert!(!dp.allow_dedup_on_next_range(5));

        assert!(dp.allow_dedup_on_next_range(10));

        dp.add_range_to_fragmentation_estimate(20);
        dp.add_range_to_fragmentation_estimate(20);
        dp.add_range_to_fragmentation_estimate(20);
        assert!(dp.allow_dedup_on_next_range(1));
        assert!(dp.defrag_at_low_threshold);
    }

    #[test]
    fn empty_window_always_allows() {
        let mut dp = DefragPrevention::new(5, 16.0, 0.5);
        assert!(dp.allow_dedup_on_next_range(0));
        assert!(dp.allow_dedup_on_next_range(100));
    }

    #[test]
    fn single_range_window() {
        let mut dp = DefragPrevention::new(1, 10.0, 0.5);
        dp.add_range_to_fragmentation_estimate(3);
        assert!(!dp.allow_dedup_on_next_range(1));
        assert!(dp.allow_dedup_on_next_range(5));
    }
}
