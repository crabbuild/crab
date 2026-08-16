//! Property test: `XorbBuilder::push_batch` is byte-identical to serial `push`.
//!
//! For any finite sequence of `(Chunk, RunId)` pairs with `RunId ∈ 0..4`,
//! feeding the sequence to two fresh builders — one via serial `push`
//! one-at-a-time, the other via a single `push_batch` call — must produce
//! the same finalized xorbs: same completion order, same xorb hash, same
//! serialized bytes, and same chunk placements.
//!
//! This is the correctness contract that lets the push pipeline swap in
//! rayon-parallel compression without observable change.
//!
//! **Validates: Requirements R3, R5**

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use bytes::Bytes;
use crab_xet::xorb::builder::{RunId, XorbBuilder, XorbResult};
use crab_xet::xorb::format::Chunk;
use proptest::prelude::*;

/// Builds a deterministic chunk payload from a seed.
///
/// Mixes a repeating low-entropy prefix (so LZ4 actually compresses and the
/// boundary-decision logic sees non-trivial compressed sizes) with a seeded
/// noise tail (so two different seeds produce different chunk hashes).
fn chunk_from_seed(seed: u32, size: usize) -> Chunk {
    // Low-entropy prefix: same 64-byte repeat for any seed. Forces LZ4
    // to produce a shorter compressed form than the raw bytes, which
    // exercises run-continuity and rollover paths more realistically.
    let prefix: Vec<u8> = (0..64).map(|i| (i as u8).wrapping_mul(3)).collect();

    // Seeded noise tail: xorshift-based, fully determined by `seed`.
    let mut state = seed.wrapping_mul(0x9E3779B9).wrapping_add(1);
    let noise: Vec<u8> = (0..size.saturating_sub(prefix.len()))
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();

    let mut data = Vec::with_capacity(size);
    data.extend_from_slice(&prefix[..prefix.len().min(size)]);
    if size > prefix.len() {
        data.extend_from_slice(&noise);
    }
    Chunk::new(Bytes::from(data))
}

/// Strategy for `(Chunk, RunId)` pairs. Seeds and run ids are drawn from
/// small ranges so duplicates appear naturally and exercise the intra-batch
/// dedup path.
fn chunk_spec_strategy() -> impl Strategy<Value = (u32, usize, u64)> {
    (
        // Seed domain of 64 distinct values — guarantees duplicates in
        // sequences longer than 64 and makes shrinking deterministic.
        0u32..64,
        // Chunk size: small enough to keep the test fast, large enough
        // that a batch of 64 can straddle rollover in a shrunken xorb.
        64usize..=4096,
        // RunId ∈ 0..4 per the task spec.
        0u64..4,
    )
}

/// Asserts that two `XorbResult` vectors are byte-identical.
fn assert_results_equal(serial: &[XorbResult], batched: &[XorbResult]) {
    assert_eq!(
        serial.len(),
        batched.len(),
        "completion count differs: serial={}, batched={}",
        serial.len(),
        batched.len(),
    );

    for (i, (a, b)) in serial.iter().zip(batched.iter()).enumerate() {
        assert_eq!(a.hash, b.hash, "xorb[{i}] hash differs");
        assert_eq!(
            a.bytes.as_ref(),
            b.bytes.as_ref(),
            "xorb[{i}] serialized bytes differ (len: serial={}, batched={})",
            a.bytes.len(),
            b.bytes.len(),
        );
        assert_eq!(
            a.placements.len(),
            b.placements.len(),
            "xorb[{i}] placement count differs",
        );
        for (j, (pa, pb)) in a.placements.iter().zip(b.placements.iter()).enumerate() {
            // `ChunkPlacement` has only `Debug, Clone` — compare fields.
            assert_eq!(
                pa.chunk_hash, pb.chunk_hash,
                "xorb[{i}] placement[{j}] chunk_hash differs",
            );
            assert_eq!(
                pa.xorb_hash, pb.xorb_hash,
                "xorb[{i}] placement[{j}] xorb_hash differs",
            );
            assert_eq!(
                pa.chunk_index, pb.chunk_index,
                "xorb[{i}] placement[{j}] chunk_index differs",
            );
            assert_eq!(
                pa.uncompressed_size, pb.uncompressed_size,
                "xorb[{i}] placement[{j}] uncompressed_size differs",
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 16,
        // Default max_shrink_iters is fine; sequences are small.
        ..ProptestConfig::default()
    })]

    /// Serial `push` and single-batch `push_batch` produce byte-identical xorbs.
    #[test]
    fn push_batch_matches_serial_push(
        specs in prop::collection::vec(chunk_spec_strategy(), 1..=64),
    ) {
        // Materialize chunks once so both builders see the same data.
        let batch: Vec<(Chunk, RunId)> = specs
            .iter()
            .map(|(seed, size, run)| (chunk_from_seed(*seed, *size), RunId(*run)))
            .collect();

        // Serial path.
        let mut serial = XorbBuilder::new();
        for (chunk, run_id) in &batch {
            serial.push(chunk, *run_id).unwrap();
        }
        let serial_results = serial.finalize().unwrap();

        // Batch path.
        let mut batched = XorbBuilder::new();
        batched.push_batch(&batch).unwrap();
        let batched_results = batched.finalize().unwrap();

        assert_results_equal(&serial_results, &batched_results);
    }
}
