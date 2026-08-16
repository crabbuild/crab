//! Integration tests for the GCS lifecycle provider against `fake-gcs-server`.
//!
//! Requires `fake-gcs-server` running on `http://localhost:4443`.
//! Start it with:
//! ```sh
//! docker run -d --name fake-gcs-server -p 4443:4443 \
//!     fsouza/fake-gcs-server -scheme http
//! ```
//!
//! Run with:
//! ```sh
//! cargo test --features tier-gcs --test tier_gcs_fake_server -- --ignored
//! ```

#![cfg(feature = "tier-gcs")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use crab::tier::classes::StorageClass;
use crab::tier::provider::gcs::GcsLifecycleProvider;
use crab::tier::{
    LifecycleProvider, Provider, RestoreBackend, RestoreState, RestoreTier, TierPlan, TierRule,
    Transition,
};

use std::time::Duration;

/// Build a GCS client pointing at `fake-gcs-server`.
fn fake_gcs_client() -> google_cloud_storage::client::Client {
    let config = google_cloud_storage::client::ClientConfig {
        storage_endpoint: "http://localhost:4443".into(),
        ..google_cloud_storage::client::ClientConfig::default().anonymous()
    };
    google_cloud_storage::client::Client::new(config)
}

/// Generate a unique test bucket name to avoid collisions.
fn test_bucket_name() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("crab-gcs-tier-test-{ts}")
}

/// Build a simple TierPlan with a Nearline transition at the given days.
fn nearline_tier_plan(days: u32) -> TierPlan {
    TierPlan {
        provider: Provider::Gcs,
        rules: vec![TierRule {
            id: "crab-xorbs-to-nearline".into(),
            prefix: ".crab/xorbs/".into(),
            transitions: vec![Transition {
                days,
                to_class: StorageClass::GcsNearline,
            }],
            noncurrent_expiration_days: None,
            min_object_size_bytes: None,
        }],
        versioning_enabled: false,
        object_lock_enabled: false,
    }
}

/// Verify the provider can be constructed and reports the correct kind.
#[ignore]
#[tokio::test]
async fn provider_construction_and_kind() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    assert_eq!(provider.kind(), Provider::Gcs);
}

/// Verify render produces valid JSON for a Nearline transition plan.
#[ignore]
#[tokio::test]
async fn render_produces_valid_json() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    let plan = nearline_tier_plan(30);
    let rendered = provider.render(&plan).expect("render should succeed");

    assert_eq!(rendered.format, crab::tier::Format::Json);
    assert!(!rendered.body.is_empty());
    assert_eq!(rendered.rule_ids, vec!["crab-xorbs-to-nearline"]);

    // Verify the body is valid JSON with expected structure.
    let parsed: serde_json::Value =
        serde_json::from_slice(&rendered.body).expect("should be valid JSON");
    let lifecycle = parsed.get("lifecycle").expect("should have lifecycle key");
    let rules = lifecycle
        .get("rule")
        .expect("should have rule key")
        .as_array()
        .expect("rule should be an array");
    assert_eq!(rules.len(), 1);
}

/// Verify `cas_guard` returns a generation-number guard.
#[ignore]
#[tokio::test]
async fn cas_guard_returns_generation() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    let guard = provider
        .cas_guard()
        .await
        .expect("cas_guard should succeed");
    assert!(guard.is_some(), "cas_guard should return Some");
    match guard.unwrap() {
        crab::tier::Guard::Generation(_) => {} // expected
        other => panic!("expected Guard::Generation, got {other:?}"),
    }
}

/// Verify GCS Archive restore state is always Ready.
#[ignore]
#[tokio::test]
async fn restore_state_always_ready_for_gcs_archive() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    let state = provider
        .state(&"some/archived/object".to_string())
        .await
        .expect("state should succeed");
    assert_eq!(state, RestoreState::Ready);
}

/// Verify GCS restore returns a handle immediately (no-op).
#[ignore]
#[tokio::test]
async fn restore_returns_handle_immediately() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    let handle = provider
        .restore(
            &"some/archived/object".to_string(),
            RestoreTier::Standard,
            Duration::from_secs(86_400 * 7),
        )
        .await
        .expect("restore should succeed");
    assert!(
        handle.id.contains("gcs-noop-"),
        "handle ID should indicate no-op restore"
    );
}

/// Verify supported_tiers is empty for all GCS classes.
#[ignore]
#[tokio::test]
async fn supported_tiers_empty_for_all_gcs_classes() {
    let client = fake_gcs_client();
    let bucket = test_bucket_name();
    let provider = GcsLifecycleProvider::from_client(client, bucket);

    let gcs_classes = [
        StorageClass::GcsStandard,
        StorageClass::GcsNearline,
        StorageClass::GcsColdline,
        StorageClass::GcsArchive,
    ];

    for class in gcs_classes {
        assert!(
            provider.supported_tiers(&class).is_empty(),
            "GCS {class:?} should have no supported restore tiers"
        );
    }
}
