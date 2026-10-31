//! Bounded, shared plans for canonicalizing dense ref membership bitmaps.

use std::ops::Range;

use super::{Result, corrupt};

// Retain at most one three-word descriptor per 64 dictionary entries. Small
// or unordered ranges keep the original per-set-bit mapping path.
const MIN_RUN_BITS: usize = 64;

#[derive(Debug)]
struct Run {
    source: usize,
    target: usize,
    length: usize,
}

pub(super) struct BitmapRemap<'a> {
    ordinals: &'a [u32],
    runs: Vec<Run>,
}

impl<'a> BitmapRemap<'a> {
    pub(super) fn new(ordinals: &'a [u32]) -> Result<Self> {
        if ordinals
            .iter()
            .any(|ordinal| *ordinal as usize >= ordinals.len())
        {
            return Err(corrupt("visibility ordinal remap escapes its dictionary"));
        }
        let mut runs = Vec::new();
        let mut start = 0;
        for end in 1..=ordinals.len() {
            if end == ordinals.len() || ordinals[end - 1].checked_add(1) != Some(ordinals[end]) {
                let length = end - start;
                if length >= MIN_RUN_BITS {
                    runs.push(Run {
                        source: start,
                        target: ordinals[start] as usize,
                        length,
                    });
                }
                start = end;
            }
        }
        Ok(Self { ordinals, runs })
    }

    pub(super) fn ordinals(&self) -> &[u32] {
        self.ordinals
    }

    // The caller validates exact bitmap length and zero dictionary padding.
    // Every target ordinal is checked once when this shared plan is built.
    pub(super) fn apply(&self, bitmap: &[u8]) -> Vec<u8> {
        let mut output = vec![0; bitmap.len()];
        let mut source = 0;
        for run in &self.runs {
            self.remap_individual(bitmap, source..run.source, &mut output);
            or_run(bitmap, run, &mut output);
            source = run.source + run.length;
        }
        self.remap_individual(bitmap, source..self.ordinals.len(), &mut output);
        output
    }

    fn remap_individual(&self, bitmap: &[u8], range: Range<usize>, output: &mut [u8]) {
        if range.is_empty() {
            return;
        }
        let first = range.start / 8;
        for (byte_index, byte) in bitmap
            .iter()
            .enumerate()
            .take(range.end.div_ceil(8))
            .skip(first)
        {
            let mut bits = *byte;
            if byte_index == first {
                bits &= u8::MAX << (range.start % 8);
            }
            if byte_index == range.end / 8 {
                bits &= (1 << (range.end % 8)) - 1;
            }
            while bits != 0 {
                let position = byte_index * 8 + bits.trailing_zeros() as usize;
                let ordinal = self.ordinals[position] as usize;
                output[ordinal / 8] |= 1 << (ordinal % 8);
                bits &= bits - 1;
            }
        }
    }
}

fn or_run(bitmap: &[u8], run: &Run, output: &mut [u8]) {
    let mut source = run.source;
    let mut target = run.target;
    let mut remaining = run.length;
    // Align source reads; target writes may cross a byte boundary and must OR
    // with bits copied by other runs or individual mappings.
    while remaining != 0 && !source.is_multiple_of(8) {
        or_bit(bitmap, source, output, target);
        source += 1;
        target += 1;
        remaining -= 1;
    }
    let shift = target % 8;
    while remaining >= 64 {
        let word = u64::from_le_bytes(bitmap[source / 8..source / 8 + 8].try_into().unwrap());
        let target_byte = target / 8;
        let bytes = &mut output[target_byte..target_byte + 8];
        let previous = u64::from_le_bytes(bytes[..].try_into().unwrap());
        bytes.copy_from_slice(&(previous | word << shift).to_le_bytes());
        if shift != 0 {
            output[target_byte + 8] |= (word >> (64 - shift)) as u8;
        }
        source += 64;
        target += 64;
        remaining -= 64;
    }
    while remaining >= 8 {
        let byte = bitmap[source / 8];
        output[target / 8] |= byte << shift;
        if shift != 0 {
            output[target / 8 + 1] |= byte >> (8 - shift);
        }
        source += 8;
        target += 8;
        remaining -= 8;
    }
    for offset in 0..remaining {
        or_bit(bitmap, source + offset, output, target + offset);
    }
}

fn or_bit(bitmap: &[u8], source: usize, output: &mut [u8], target: usize) {
    let bit = (bitmap[source / 8] >> (source % 8)) & 1;
    output[target / 8] |= bit << (target % 8);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::git_visibility::GitVisibilityClosure;

    fn oracle(bitmap: &[u8], ordinals: &[u32]) -> Vec<u8> {
        let mut output = vec![0; bitmap.len()];
        for (position, target) in ordinals.iter().enumerate() {
            if bitmap[position / 8] & (1 << (position % 8)) != 0 {
                let target = *target as usize;
                output[target / 8] |= 1 << (target % 8);
            }
        }
        output
    }

    fn assert_mapping(bitmap: Vec<u8>, ordinals: &[u32]) {
        let plan = BitmapRemap::new(ordinals).unwrap();
        let expected = oracle(&bitmap, ordinals);
        let actual = GitVisibilityClosure::Bitmap(bitmap).remap(&plan).unwrap();
        assert_eq!(actual, GitVisibilityClosure::Bitmap(expected));
        actual.validate(ordinals.len()).unwrap();
    }

    #[test]
    fn runs_copy_all_bit_offsets_and_word_boundaries() {
        let count: usize = 517;
        for source in 0..8 {
            for target in 0..8 {
                for length in [64, 65, 71, 72, 127, 128, 129, 192, 193, 256, 257] {
                    let mut other =
                        (0..count).filter(|ordinal| !(target..target + length).contains(ordinal));
                    let ordinals = (0..count)
                        .map(|position| {
                            if (source..source + length).contains(&position) {
                                (target + position - source) as u32
                            } else {
                                other.next().unwrap() as u32
                            }
                        })
                        .collect::<Vec<_>>();
                    for pattern in [0xff, 0xa5, 0x42] {
                        let mut bitmap = vec![pattern; count.div_ceil(8)];
                        *bitmap.last_mut().unwrap() &= (1 << (count % 8)) - 1;
                        assert_mapping(bitmap, &ordinals);
                    }
                }
            }
        }
    }

    #[test]
    fn disjoint_runs_and_repeated_targets_keep_exact_union() {
        let count: usize = 8_195;
        let order = (0..count as u32).collect::<Vec<_>>();
        let ordinals = order
            .chunks(64)
            .rev()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        let mut bitmap = vec![0xb5; count.div_ceil(8)];
        *bitmap.last_mut().unwrap() &= (1 << (count % 8)) - 1;
        let plan = BitmapRemap::new(&ordinals).unwrap();
        assert_eq!(plan.runs.len(), count / MIN_RUN_BITS);
        assert_mapping(bitmap.clone(), &ordinals);
        assert_mapping(bitmap, &vec![7; count]);
    }

    #[test]
    fn unstructured_permutation_uses_bounded_fallback() {
        let ordinals = (0..4_096).rev().collect::<Vec<_>>();
        let plan = BitmapRemap::new(&ordinals).unwrap();
        assert!(plan.runs.is_empty());
        assert_mapping(vec![0xc3; 512], &ordinals);
        assert_mapping(Vec::new(), &[]);
    }

    #[test]
    fn invalid_targets_and_bitmap_padding_are_rejected() {
        assert!(BitmapRemap::new(&[1]).is_err());
        assert!(BitmapRemap::new(&[u32::MAX]).is_err());
        let ordinals = (0..9).collect::<Vec<_>>();
        let plan = BitmapRemap::new(&ordinals).unwrap();
        assert!(GitVisibilityClosure::Bitmap(vec![0]).remap(&plan).is_err());
        assert!(
            GitVisibilityClosure::Bitmap(vec![0, 2])
                .remap(&plan)
                .is_err()
        );
        assert_mapping(vec![0, 1], &ordinals);
    }

    #[test]
    fn sparse_closures_keep_sorted_exact_positions() {
        let ordinals = (0..512)
            .map(|position| (position + 37) % 512)
            .collect::<Vec<_>>();
        let plan = BitmapRemap::new(&ordinals).unwrap();
        let closure = GitVisibilityClosure::Sparse(vec![0, 17, 511]);
        assert_eq!(closure.remap(&plan).unwrap().positions(), vec![36, 37, 54]);
    }

    proptest! {
        #[test]
        fn permutations_match_independent_bit_oracle(
            entries in prop::collection::vec((any::<u16>(), any::<bool>()), 0..512),
        ) {
            let mut ordinals = (0..entries.len() as u32).collect::<Vec<_>>();
            ordinals.sort_unstable_by_key(|position| (entries[*position as usize].0, *position));
            let mut bitmap = vec![0; entries.len().div_ceil(8)];
            for (position, (_, present)) in entries.iter().enumerate() {
                if *present {
                    bitmap[position / 8] |= 1 << (position % 8);
                }
            }
            assert_mapping(bitmap, &ordinals);
        }
    }
}
