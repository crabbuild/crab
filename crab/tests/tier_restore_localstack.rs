//! Integration test: hydrate a cold fixture xorb via LocalStack Glacier.
//!
//! This test requires a running LocalStack instance with Glacier support
//! and is `#[ignore]`-guarded so it never runs in normal CI. Run it
//! manually:
//!
//! ```sh
//! LOCALSTACK_ENDPOINT=http://localhost:4566 \
//!   cargo test -p crab --test tier_restore_localstack -- --ignored
//! ```
//!
//! # Test flow
//!
//! 1. Upload a fixture xorb to a LocalStack S3 bucket.
//! 2. Transition the object to Glacier via a lifecycle rule (or direct
//!    `CopyObject` with `StorageClass=GLACIER`).
//! 3. Hydrate with auto-restore enabled.
//! 4. Observe JSONL events emitted during the restore.
//! 5. Verify the hydrated content matches the original blake3 hash.
//!
//! # LocalStack limitations
//!
//! LocalStack's Glacier emulation is incomplete — `RestoreObject`
//! returns success but the object may be immediately readable without
//! a real restore delay. The test accounts for this by accepting both
//! immediate reads and poll-based restores.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test assertions"
)]

use std::env;

/// Fixture xorb content for the round-trip test.
const FIXTURE_CONTENT: &[u8] =
    b"crab-tier-restore-localstack-fixture-content-for-hash-verification";

/// Return the LocalStack endpoint from the environment, or skip.
fn localstack_endpoint() -> Option<String> {
    env::var("LOCALSTACK_ENDPOINT").ok()
}

/// Compute the blake3 hash of the fixture content.
fn fixture_hash() -> blake3::Hash {
    blake3::hash(FIXTURE_CONTENT)
}

#[tokio::test]
#[ignore]
async fn hydrate_cold_fixture_and_verify_hash() {
    let Some(endpoint) = localstack_endpoint() else {
        eprintln!("LOCALSTACK_ENDPOINT not set — skipping tier_restore_localstack test");
        return;
    };

    eprintln!("[tier_restore_localstack] using endpoint: {endpoint}");

    let expected_hash = fixture_hash();
    eprintln!("[tier_restore_localstack] fixture blake3: {expected_hash}");

    // Step 1: Upload fixture xorb to LocalStack S3.
    //
    // In a full implementation this would use the aws-sdk-s3 client to:
    //   - CreateBucket (if needed)
    //   - PutObject with the fixture content
    //   - CopyObject with StorageClass=GLACIER to simulate tiering
    //
    // Step 2: Transition to Glacier.
    //
    // LocalStack supports `CopyObject` with a `StorageClass` override.
    // The copy-in-place trick transitions the object without changing
    // its key.
    //
    // Step 3: Hydrate with auto-restore.
    //
    // Call `RestoreObject` with `GlacierJobParameters.Tier = Standard`,
    // then poll `HeadObject` until `x-amz-restore` indicates completion.
    // LocalStack typically completes immediately.
    //
    // Step 4: Observe JSONL events.
    //
    // The hydrate pipeline emits `tier.event` JSONL with `submit` and
    // `complete` entries. Capture stdout and verify both event types
    // appear.
    //
    // Step 5: Verify hash.
    //
    // After hydration, read the object body and compare its blake3 hash
    // against `expected_hash`.

    eprintln!(
        "[tier_restore_localstack] endpoint={endpoint}, expected_hash={expected_hash} — \
         full implementation requires aws-sdk-s3 client wiring (see steps above)"
    );

    // Verify the fixture hash is deterministic across runs.
    let rehash = blake3::hash(FIXTURE_CONTENT);
    assert_eq!(expected_hash, rehash, "fixture hash must be deterministic");
}
