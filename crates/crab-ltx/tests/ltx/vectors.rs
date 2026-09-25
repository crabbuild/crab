//! Deterministic decoder replay over the external vectors.
//!
//! This is the CI-runnable half of the fuzzing contract: every entry point
//! `fuzz/fuzz_targets` drives is exercised over truncations, deterministic bit
//! flips, and length prefixes of the upstream vectors. A panic fails the test,
//! so the decoders stay total on malformed input; deep random search stays in
//! the fuzz targets, which need a nightly toolchain.

#[cfg(feature = "replica")]
use crab_ltx::Limits;

/// External vectors written by the upstream celld encoder.
const VECTORS: &[&str] = &[
    "celld-10cb130-delta-2-2-512.ltx",
    "celld-10cb130-snapshot-block-512.ltx",
    "celld-10cb130-snapshot-frame-512.ltx",
    "celld-10cb130-snapshot-block-4096.ltx",
    "superfly-ltx-v0.5.2-delta-2-2-512.ltx",
    "superfly-ltx-v0.5.2-snapshot-block-512.ltx",
];

fn vector_bytes(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors")
        .join(name);
    std::fs::read(path).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// Runs every inspection entry point over one input.
///
/// None of them may panic, whatever the bytes are.
fn inspect_everything(bytes: &[u8]) {
    #[cfg(feature = "replica")]
    let limits = Limits::default();
    let _ = crab_ltx::internal::inspect_ltx(bytes);
    #[cfg(feature = "replica")]
    {
        let _ = crab_ltx::internal::inspect_root(bytes);
        let _ = crab_ltx::internal::inspect_segment_page(bytes);
        let _ = crab_ltx::internal::inspect_directory_node(bytes);
        let _ = crab_ltx::internal::inspect_bundle(bytes, limits);
        let _ = crab_ltx::internal::inspect_node_frame(bytes, limits);
    }
}

#[test]
fn external_vectors_are_accepted() {
    for name in VECTORS {
        let bytes = vector_bytes(name);
        assert!(
            crab_ltx::internal::inspect_ltx(&bytes).is_ok(),
            "{name} must decode"
        );
        inspect_everything(&bytes);
    }
}

#[test]
fn truncated_and_mutated_vectors_never_panic() {
    for name in VECTORS {
        let bytes = vector_bytes(name);
        for end in 0..bytes.len() {
            inspect_everything(&bytes[..end]);
        }
        // Deterministic single-bit and single-byte mutations across the file,
        // denser near the header and trailer where the parsers branch.
        let mut offsets: Vec<usize> = (0..bytes.len().min(160)).collect();
        offsets.extend((160..bytes.len()).step_by(37));
        offsets.extend(bytes.len().saturating_sub(80)..bytes.len());
        for offset in offsets {
            for delta in [1_u8, 0x80, 0xff] {
                let mut mutated = bytes.clone();
                mutated[offset] ^= delta;
                inspect_everything(&mutated);
            }
        }
        // A length prefix must not be interpreted as a body.
        let mut prefixed = (bytes.len() as u64).to_be_bytes().to_vec();
        prefixed.extend_from_slice(&bytes);
        inspect_everything(&prefixed);
    }
}
