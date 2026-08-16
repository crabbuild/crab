//! Integration tests for the Azure lifecycle provider against Azurite.
//!
//! Requires Azurite running on `http://127.0.0.1:10000` (blob service).
//! Start it with:
//! ```sh
//! docker run -d --name azurite -p 10000:10000 -p 10001:10001 -p 10002:10002 \
//!     mcr.microsoft.com/azure-storage/azurite
//! ```
//!
//! Or install and run locally:
//! ```sh
//! npm install -g azurite
//! azurite --silent --location /tmp/azurite
//! ```
//!
//! Run with:
//! ```sh
//! cargo test --features tier-azure --test tier_azure_azurite -- --ignored
//! ```

#![cfg(feature = "tier-azure")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use crab::tier::classes::StorageClass;
use crab::tier::provider::azure::AzureLifecycleProvider;
use crab::tier::{
    Format, Guard, LifecycleProvider, Provider, RestoreBackend, RestoreState, RestoreTier,
    TierPlan, TierRule, Transition,
};

use std::time::Duration;

/// Build a simple TierPlan with a Cool transition at the given days.
fn cool_tier_plan(days: u32) -> TierPlan {
    TierPlan {
        provider: Provider::Azure,
        rules: vec![TierRule {
            id: "crab-xorbs-to-cool".into(),
            prefix: ".crab/xorbs/".into(),
            transitions: vec![Transition {
                days,
                to_class: StorageClass::AzureCool,
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
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );
    assert_eq!(provider.kind(), Provider::Azure);
}

/// Verify render produces valid JSON for a Cool transition plan.
#[ignore]
#[tokio::test]
async fn render_produces_valid_json() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let plan = cool_tier_plan(30);
    let rendered = provider.render(&plan).expect("render should succeed");

    assert_eq!(rendered.format, Format::Json);
    assert!(!rendered.body.is_empty());
    assert_eq!(rendered.rule_ids, vec!["crab-xorbs-to-cool"]);

    // Verify the body is valid JSON with expected structure.
    let parsed: serde_json::Value =
        serde_json::from_slice(&rendered.body).expect("should be valid JSON");
    let rules = parsed
        .get("rules")
        .expect("should have rules key")
        .as_array()
        .expect("rules should be an array");
    assert_eq!(rules.len(), 1);
}

/// Verify `cas_guard` returns an ETag guard.
#[ignore]
#[tokio::test]
async fn cas_guard_returns_etag() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let guard = provider
        .cas_guard()
        .await
        .expect("cas_guard should succeed");
    assert!(guard.is_some(), "cas_guard should return Some");
    match guard.unwrap() {
        Guard::Etag(_) => {} // expected
        other => panic!("expected Guard::Etag, got {other:?}"),
    }
}

/// Verify Azure Archive supported tiers include Standard and High only.
#[ignore]
#[tokio::test]
async fn supported_tiers_archive_has_standard_and_high() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let tiers = provider.supported_tiers(&StorageClass::AzureArchive);
    assert_eq!(tiers.len(), 2);
    assert!(tiers.contains(&RestoreTier::Standard));
    assert!(tiers.contains(&RestoreTier::High));
    assert!(!tiers.contains(&RestoreTier::Bulk));
    assert!(!tiers.contains(&RestoreTier::Expedited));
}

/// Verify non-archive Azure classes have no supported restore tiers.
#[ignore]
#[tokio::test]
async fn supported_tiers_empty_for_non_archive_classes() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let azure_non_archive = [
        StorageClass::AzureHot,
        StorageClass::AzureCool,
        StorageClass::AzureCold,
    ];

    for class in azure_non_archive {
        assert!(
            provider.supported_tiers(&class).is_empty(),
            "Azure {class:?} should have no supported restore tiers"
        );
    }
}

/// Verify restore stub returns a handle.
#[ignore]
#[tokio::test]
async fn restore_returns_handle() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let handle = provider
        .restore(
            &"some/blob/path".to_string(),
            RestoreTier::High,
            Duration::from_secs(86_400 * 7),
        )
        .await
        .expect("restore should succeed");
    assert!(
        handle.id.contains("azure-restore-"),
        "handle ID should indicate Azure restore"
    );
}

/// Verify restore state stub returns NotRequested.
#[ignore]
#[tokio::test]
async fn restore_state_returns_not_requested() {
    let provider = AzureLifecycleProvider::new(
        "devstoreaccount1".into(),
        "testcontainer".into(),
        "sub-0000".into(),
        "rg-test".into(),
    );

    let state = provider
        .state(&"some/blob/path".to_string())
        .await
        .expect("state should succeed");
    assert_eq!(state, RestoreState::NotRequested);
}
