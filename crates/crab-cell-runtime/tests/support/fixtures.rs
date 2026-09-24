//! Fixtures shared by more than one integration suite.
//!
//! Every suite compiles this module through `mod support;`, so a fixture that
//! one suite never calls is expected rather than dead code.
#![allow(dead_code)]

use std::fmt;

use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, WireValue};
use crab_cell_runtime::identity::RequestId;

/// Returns the current wall-clock time in milliseconds.
pub fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Returns one mutation identity that stays valid for a minute from now.
pub fn mutation_identity(byte: u8) -> MutationIdentity {
    let issued_at_ms = now_ms();
    mutation_identity_window(byte, issued_at_ms, issued_at_ms + 60_000)
}

/// Returns one mutation identity with an explicit validity window.
///
/// Suites that drive a clock use this: a fixed window keeps a case reproducible
/// while every other case can start from [`now_ms`].
pub fn mutation_identity_window(
    byte: u8,
    issued_at_ms: i64,
    expires_at_ms: i64,
) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms,
        expires_at_ms,
    }
}

/// Encodes and decodes one bounded wire value, asserting an exact round trip.
pub fn codec_roundtrip<T: WireValue + PartialEq + fmt::Debug>(value: T) {
    let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    value.encode(&mut encoder).unwrap();
    let bytes = encoder.finish();
    let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
    assert_eq!(T::decode(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
}
