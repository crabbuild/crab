//! Shannon entropy estimation for compressibility probing.

/// Estimates the compressibility of `data` using Shannon entropy.
///
/// Returns a ratio in `[0.0, 1.0]` where `1.0` means incompressible. An empty
/// slice returns `0.0`.
#[must_use]
pub fn entropy_ratio(data: &[u8]) -> f32 {
    let len = data.len();
    if len == 0 {
        return 0.0;
    }

    let mut freq = [0u32; 256];
    for &b in data {
        freq[b as usize] += 1;
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "len fits in f32 for 4 KiB samples"
    )]
    let len_f = len as f32;
    let inv_len = 1.0 / len_f;

    let mut entropy: f32 = 0.0;
    for &count in &freq {
        if count == 0 {
            continue;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "count fits in f32 for 4 KiB samples"
        )]
        let p = count as f32 * inv_len;
        entropy -= p * p.log2();
    }

    (entropy / 8.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_zero() {
        assert_eq!(entropy_ratio(&[]), 0.0);
    }

    #[test]
    fn all_zeros_has_low_entropy() {
        let data = vec![0u8; 4096];
        let ratio = entropy_ratio(&data);
        assert!(
            ratio < 0.01,
            "all-zeros entropy ratio should be near 0.0, got {ratio}"
        );
    }

    #[test]
    fn single_byte_value_has_zero_entropy() {
        let data = vec![0xAB; 1024];
        assert_eq!(entropy_ratio(&data), 0.0);
    }

    #[test]
    fn random_bytes_have_high_entropy() {
        let mut data = Vec::with_capacity(4096);
        for _ in 0..16 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        let ratio = entropy_ratio(&data);
        assert!(
            ratio > 0.99,
            "uniform distribution should have entropy ratio near 1.0, got {ratio}"
        );
    }

    #[test]
    fn repeated_pattern_has_medium_entropy() {
        let mut data = Vec::with_capacity(4096);
        for _ in 0..2048 {
            data.push(0x00);
            data.push(0xFF);
        }
        let ratio = entropy_ratio(&data);
        assert!(
            ratio > 0.1 && ratio < 0.2,
            "two-value pattern should have entropy ratio ~0.125, got {ratio}"
        );
    }

    #[test]
    fn short_input_works() {
        let ratio = entropy_ratio(&[1, 2, 3, 4]);
        assert!(
            (ratio - 0.25).abs() < 0.01,
            "4 distinct equal-frequency bytes should give ratio ~0.25, got {ratio}"
        );
    }
}
