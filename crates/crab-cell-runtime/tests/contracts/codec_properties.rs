//! Properties every bounded wire decoder must hold for arbitrary input.
//!
//! The decoders parse peer, client, and storage bytes, so two properties matter
//! beyond the fixtures: a decode never panics, and whatever it accepts whole is
//! canonical — re-encoding the value reproduces exactly the input bytes.

use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, WireValue};
use crab_cell_runtime::primitives::effects::{
    EffectAckRequest, EffectClaimRequest, EffectLeaseRequest, EffectStatusRequest,
    EffectValidateRequest,
};
use crab_cell_runtime::primitives::kv::{KvGetRequest, KvListRequest};
use crab_cell_runtime::primitives::queue::{
    QueueClaimRequest, QueueInfoRequest, QueueLeaseRequest, QueueValidateRequest,
};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::primitives::workflow::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    WorkflowActivityClaimRequest, WorkflowActivityExtendRequest, WorkflowActivityValidateRequest,
};
use proptest::prelude::*;

const LIMIT: u32 = 4 * 1024 * 1024;

/// Decodes arbitrary bytes, then re-encodes anything the decoder accepted whole.
fn accepts_only_canonical<T: WireValue>(bytes: &[u8]) {
    let Ok(mut decoder) = BoundedDecoder::new(bytes, LIMIT) else {
        return;
    };
    let Ok(value) = T::decode(&mut decoder) else {
        return;
    };
    if decoder.finish().is_err() {
        // A value that does not consume its input is not the canonical encoding
        // of anything; `finish` is what rejects it.
        return;
    }
    let mut encoder = BoundedEncoder::new(LIMIT).expect("the limit is valid");
    value
        .encode(&mut encoder)
        .expect("an accepted value re-encodes");
    assert_eq!(encoder.finish(), bytes, "accepted input is not canonical");
}

/// One decoder per index, so one property covers every public wire shape.
fn check(index: usize, bytes: &[u8]) {
    match index {
        0 => accepts_only_canonical::<KvGetRequest>(bytes),
        1 => accepts_only_canonical::<KvListRequest>(bytes),
        2 => accepts_only_canonical::<QueueClaimRequest>(bytes),
        3 => accepts_only_canonical::<QueueInfoRequest>(bytes),
        4 => accepts_only_canonical::<QueueLeaseRequest>(bytes),
        5 => accepts_only_canonical::<QueueValidateRequest>(bytes),
        6 => accepts_only_canonical::<SqlValue>(bytes),
        7 => accepts_only_canonical::<SqlStatement>(bytes),
        8 => accepts_only_canonical::<SqlBatch>(bytes),
        9 => accepts_only_canonical::<EffectAckRequest>(bytes),
        10 => accepts_only_canonical::<EffectClaimRequest>(bytes),
        11 => accepts_only_canonical::<EffectLeaseRequest>(bytes),
        12 => accepts_only_canonical::<EffectStatusRequest>(bytes),
        13 => accepts_only_canonical::<EffectValidateRequest>(bytes),
        14 => accepts_only_canonical::<ActivityClaim>(bytes),
        15 => accepts_only_canonical::<ActivityCompletion>(bytes),
        16 => accepts_only_canonical::<ActivityLeaseOutcome>(bytes),
        17 => accepts_only_canonical::<ActivityCompletionOutcome>(bytes),
        18 => accepts_only_canonical::<WorkflowActivityClaimRequest>(bytes),
        19 => accepts_only_canonical::<WorkflowActivityExtendRequest>(bytes),
        20 => accepts_only_canonical::<WorkflowActivityValidateRequest>(bytes),
        _ => unreachable!("every decoder under test has an index"),
    }
}

/// A valid encoding of one representative value per index, for the mutation
/// property below: random bytes rarely reach a decoder's deeper paths.
fn fixture_encoding(index: usize) -> Vec<u8> {
    let mut encoder = BoundedEncoder::new(LIMIT).expect("the limit is valid");
    match index {
        0 => KvGetRequest {
            scope: b"tenant".to_vec(),
            key: b"key".to_vec(),
        }
        .encode(&mut encoder)
        .expect("a bounded value encodes"),
        _ => QueueClaimRequest {
            limit: 4,
            lease_ms: 30_000,
        }
        .encode(&mut encoder)
        .expect("a bounded value encodes"),
    }
    encoder.finish()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Arbitrary bytes never panic a decoder, and an accepted value re-encodes to
    /// exactly the bytes it was decoded from.
    #[test]
    fn primitive_decoders_accept_only_canonical_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        index in 0usize..21,
    ) {
        check(index, &bytes);
    }

    /// A valid encoding with a run of bytes replaced must still decode without
    /// panicking, and anything it accepts whole must stay canonical.
    #[test]
    fn mutated_valid_encodings_stay_total_and_canonical(
        index in 0usize..2,
        position in any::<usize>(),
        replacement in any::<u8>(),
        run in 1usize..8,
    ) {
        let mut bytes = fixture_encoding(index);
        if !bytes.is_empty() {
            let mut at = position % bytes.len();
            for _ in 0..run {
                bytes[at] = replacement;
                at = (at + 1) % bytes.len();
            }
        }
        match index {
            0 => accepts_only_canonical::<KvGetRequest>(&bytes),
            _ => accepts_only_canonical::<QueueClaimRequest>(&bytes),
        }
    }
}

/// Both branches of the property above, spelled out: a canonical encoding passes
/// the check, and one with a trailing byte is rejected by `finish` rather than
/// being reported as non-canonical.
#[test]
fn canonical_encodings_roundtrip_and_trailing_bytes_are_rejected() {
    let bytes = fixture_encoding(0);
    accepts_only_canonical::<KvGetRequest>(&bytes);

    let mut extended = bytes.clone();
    extended.push(0);
    accepts_only_canonical::<KvGetRequest>(&extended);
}
