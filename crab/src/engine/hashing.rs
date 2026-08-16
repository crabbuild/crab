//! Blake3 hashing helpers with an adaptive serial/parallel split.
//!
//! Large files benefit from `blake3::Hasher::update_rayon` which fans
//! out across available cores. Small files don't — the rayon dispatch
//! overhead dwarfs the hashing work. This module centralises the
//! threshold so both the `git add` filter (clean.rs) and the direct
//! `crab add` CLI (cmd/add.rs) branch the same way.

/// Files at or above this size use rayon-parallel blake3 hashing.
///
/// Below the threshold the serial 128 KiB-block loop is used, matching
/// the historical behaviour and avoiding rayon's fixed dispatch cost
/// on short inputs.
pub const BLAKE3_RAYON_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Update a blake3 hasher with the file's contents, choosing serial or
/// rayon-parallel hashing based on content length.
///
/// The output hash is byte-identical regardless of which branch runs —
/// `update_rayon` is a blake3 API guarantee.
pub fn update_blake3_adaptive(hasher: &mut blake3::Hasher, content: &[u8]) {
    if content.len() as u64 >= BLAKE3_RAYON_THRESHOLD {
        // Parallel path: requires a contiguous slice.
        //
        // Streaming clean (see crab-push-flow-optimization Commit 1)
        // reads incrementally without a contiguous buffer, so it always
        // takes the serial branch below regardless of file size.
        hasher.update_rayon(content);
    } else {
        // Serial path: 128 KiB blocks keep cache behaviour predictable
        // and match the pre-existing loop in callers.
        for block in content.chunks(128 * 1024) {
            hasher.update(block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    /// Above-threshold path: 64 MiB + 1024 bytes of pseudo-random
    /// content hashes to the same value as the one-shot reference.
    #[test]
    fn adaptive_matches_reference_above_threshold() {
        let size = (BLAKE3_RAYON_THRESHOLD as usize) + 1024;
        let mut rng = StdRng::from_seed([0xAB; 32]);
        let mut data = vec![0u8; size];
        rng.fill(&mut data[..]);

        let mut hasher = blake3::Hasher::new();
        update_blake3_adaptive(&mut hasher, &data);
        let got = *hasher.finalize().as_bytes();

        let expected = *blake3::hash(&data).as_bytes();
        assert_eq!(got, expected, "rayon branch must match blake3::hash");
    }

    /// Below-threshold path: 1 MiB hashes correctly via the serial
    /// 128 KiB loop.
    #[test]
    fn adaptive_matches_reference_below_threshold() {
        let size = 1024 * 1024;
        let mut rng = StdRng::from_seed([0xCD; 32]);
        let mut data = vec![0u8; size];
        rng.fill(&mut data[..]);

        let mut hasher = blake3::Hasher::new();
        update_blake3_adaptive(&mut hasher, &data);
        let got = *hasher.finalize().as_bytes();

        let expected = *blake3::hash(&data).as_bytes();
        assert_eq!(got, expected, "serial branch must match blake3::hash");
    }

    /// Pin the threshold to 64 MiB so an accidental change forces a
    /// spec review (both call sites rely on this constant as the
    /// documented serial/parallel split).
    #[test]
    fn threshold_is_64_mib() {
        assert_eq!(BLAKE3_RAYON_THRESHOLD, 64 * 1024 * 1024);
    }
}
