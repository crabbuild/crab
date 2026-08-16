//! Deterministic prefix sampling via `blake3` hash.
//!
//! Sampling uses `blake3(key)` to produce a stable hash, then checks
//! whether the hash falls within the inclusion threshold for the given
//! ratio. This ensures:
//!
//! - Determinism: the same key always produces the same include/exclude
//!   decision for a given ratio.
//! - Uniformity: `blake3` distributes evenly, so the sample is
//!   representative.
//!
//! The `--sample <ratio>` CLI flag maps to this module.

/// Determines whether a key should be included in the sample.
///
/// Uses `blake3(key)` and checks whether the first 8 bytes, interpreted
/// as a `u64`, fall below `ratio * u64::MAX`.
///
/// # Arguments
///
/// - `key` — the object key to hash.
/// - `ratio` — inclusion probability in `(0.0, 1.0]`. A ratio of `1.0`
///   includes everything; `0.5` includes roughly half.
///
/// # Panics
///
/// Does not panic. A ratio ≤ 0.0 excludes everything; ≥ 1.0 includes
/// everything.
pub fn should_include(key: &str, ratio: f64) -> bool {
    if ratio >= 1.0 {
        return true;
    }
    if ratio <= 0.0 {
        return false;
    }

    let hash = blake3::hash(key.as_bytes());
    let bytes = hash.as_bytes();

    // Take the first 8 bytes as a u64 (big-endian for uniform distribution).
    let value = u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);

    let threshold = (ratio * u64::MAX as f64) as u64;
    value <= threshold
}

/// Compute the Hoeffding bound for a sample estimate.
///
/// Given a sample of `n` items from a population, the true mean is
/// within `±epsilon` of the sample mean with probability at least
/// `1 - 2*exp(-2*n*epsilon^2)`.
///
/// This function returns the `epsilon` for a 95% confidence level:
/// `epsilon = sqrt(ln(2/0.05) / (2*n))`.
///
/// Returns `None` if `n` is zero.
pub fn hoeffding_bound_95(n: u64) -> Option<f64> {
    if n == 0 {
        return None;
    }
    // For 95% confidence: alpha = 0.05, so ln(2/0.05) = ln(40) ≈ 3.689
    let ln_term = (2.0_f64 / 0.05).ln();
    let epsilon = (ln_term / (2.0 * n as f64)).sqrt();
    Some(epsilon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_one_includes_everything() {
        assert!(should_include("any-key", 1.0));
        assert!(should_include("", 1.0));
        assert!(should_include("a/b/c/d/e", 1.0));
    }

    #[test]
    fn ratio_zero_excludes_everything() {
        assert!(!should_include("any-key", 0.0));
        assert!(!should_include("", 0.0));
    }

    #[test]
    fn ratio_negative_excludes_everything() {
        assert!(!should_include("key", -0.5));
    }

    #[test]
    fn ratio_above_one_includes_everything() {
        assert!(should_include("key", 1.5));
        assert!(should_include("key", 2.0));
    }

    #[test]
    fn deterministic_same_key_same_result() {
        let key = ".crab/xorbs/abcdef1234567890";
        let ratio = 0.5;
        let first = should_include(key, ratio);
        let second = should_include(key, ratio);
        assert_eq!(first, second);
    }

    #[test]
    fn approximate_ratio_at_half() {
        // With enough keys, roughly half should be included at ratio 0.5.
        let total = 10_000;
        let included: usize = (0..total)
            .filter(|i| should_include(&format!(".crab/xorbs/{i:016x}"), 0.5))
            .count();

        let ratio = included as f64 / total as f64;
        // Allow ±5% tolerance for statistical variation.
        assert!(
            (0.45..=0.55).contains(&ratio),
            "expected ~50% inclusion, got {ratio:.2}%"
        );
    }

    #[test]
    fn approximate_ratio_at_quarter() {
        let total = 10_000;
        let included: usize = (0..total)
            .filter(|i| should_include(&format!("prefix/{i:016x}"), 0.25))
            .count();

        let ratio = included as f64 / total as f64;
        assert!(
            (0.20..=0.30).contains(&ratio),
            "expected ~25% inclusion, got {ratio:.2}%"
        );
    }

    #[test]
    fn hoeffding_bound_95_returns_none_for_zero() {
        assert!(hoeffding_bound_95(0).is_none());
    }

    #[test]
    fn hoeffding_bound_95_decreases_with_n() {
        let bound_100 = hoeffding_bound_95(100).expect("n=100");
        let bound_1000 = hoeffding_bound_95(1000).expect("n=1000");
        let bound_10000 = hoeffding_bound_95(10000).expect("n=10000");

        assert!(bound_100 > bound_1000);
        assert!(bound_1000 > bound_10000);
    }

    #[test]
    fn hoeffding_bound_95_reasonable_for_large_n() {
        // For n=10000, bound should be small (< 5%).
        let bound = hoeffding_bound_95(10000).expect("n=10000");
        assert!(bound < 0.05, "bound {bound} should be < 0.05 for n=10000");
    }
}
