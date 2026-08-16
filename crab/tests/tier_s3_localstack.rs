//! Integration tests for the S3 lifecycle provider against LocalStack.
//!
//! Requires LocalStack running on `http://localhost:4566`.
//! Run with:
//! ```sh
//! cargo test --features tier-s3 --test tier_s3_localstack -- --ignored
//! ```

#![cfg(feature = "tier-s3")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use crab::tier::classes::StorageClass;
use crab::tier::provider::s3::{S3LifecycleProvider, render};
use crab::tier::{LifecycleProvider, Provider, TierPlan, TierRule, Transition};

/// Build an S3 client pointing at LocalStack.
async fn localstack_client() -> aws_sdk_s3::Client {
    let config = aws_sdk_s3::config::Builder::new()
        .endpoint_url("http://localhost:4566")
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .force_path_style(true)
        .behavior_version_latest()
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

/// Generate a unique test bucket name to avoid collisions.
fn test_bucket_name() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("crab-tier-test-{ts}")
}

/// Create a test bucket on LocalStack.
async fn create_bucket(client: &aws_sdk_s3::Client, bucket: &str) {
    client
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create_bucket should succeed on LocalStack");
}

/// Delete a test bucket on LocalStack (best-effort cleanup).
async fn delete_bucket(client: &aws_sdk_s3::Client, bucket: &str) {
    // Delete lifecycle configuration first (ignore errors).
    let _ = client.delete_bucket_lifecycle().bucket(bucket).send().await;

    // Delete the bucket itself.
    let _ = client.delete_bucket().bucket(bucket).send().await;
}

/// Build a simple TierPlan with an IA transition at the given number of days.
fn ia_tier_plan(days: u32) -> TierPlan {
    TierPlan {
        provider: Provider::S3,
        rules: vec![TierRule {
            id: "crab-xorbs-to-ia".into(),
            prefix: ".crab/xorbs/".into(),
            transitions: vec![Transition {
                days,
                to_class: StorageClass::S3StandardIa,
            }],
            noncurrent_expiration_days: None,
            min_object_size_bytes: None,
        }],
        versioning_enabled: false,
        object_lock_enabled: false,
    }
}

/// Full round-trip: apply → get → diff → apply-again (no-op).
///
/// Verifies that:
/// 1. A rendered lifecycle can be applied to a real S3 bucket.
/// 2. The lifecycle can be read back.
/// 3. The retrieved lifecycle contains the same rule IDs.
/// 4. Applying the same plan again succeeds (idempotent).
#[ignore]
#[tokio::test]
async fn apply_get_diff_apply_again_is_idempotent() {
    let client = localstack_client().await;
    let bucket = test_bucket_name();

    // Setup: create a fresh test bucket.
    create_bucket(&client, &bucket).await;

    let provider = S3LifecycleProvider::from_client(client.clone(), bucket.clone());

    // Step 1: No lifecycle should exist yet.
    let initial = provider
        .get()
        .await
        .expect("get should succeed on empty bucket");
    assert!(
        initial.is_none(),
        "new bucket should have no lifecycle configuration"
    );

    // Step 2: Render a plan with IA transition at 30 days.
    let plan = ia_tier_plan(30);
    let rendered = render(&plan).expect("render should succeed");
    assert!(
        !rendered.body.is_empty(),
        "rendered lifecycle body should not be empty"
    );
    assert_eq!(rendered.rule_ids, vec!["crab-xorbs-to-ia"]);

    // Step 3: Apply the lifecycle.
    let outcome = provider
        .put(&rendered, None)
        .await
        .expect("put should succeed");
    assert!(
        !outcome.applied_at.is_empty(),
        "applied_at timestamp should be set"
    );

    // Step 4: Read back the lifecycle.
    let retrieved = provider
        .get()
        .await
        .expect("get should succeed after put")
        .expect("lifecycle should exist after put");

    // Step 5: Verify the retrieved lifecycle matches what we applied.
    // The rule IDs should contain our crab-prefixed rule.
    assert!(
        retrieved.rule_ids.contains(&"crab-xorbs-to-ia".to_string()),
        "retrieved lifecycle should contain the applied rule ID, got: {:?}",
        retrieved.rule_ids
    );

    // Both bodies should be non-empty XML.
    assert!(
        !retrieved.body.is_empty(),
        "retrieved lifecycle body should not be empty"
    );
    let retrieved_xml =
        String::from_utf8(retrieved.body.clone()).expect("retrieved body should be valid UTF-8");
    assert!(
        retrieved_xml.contains("crab-xorbs-to-ia"),
        "retrieved XML should contain the rule ID"
    );
    assert!(
        retrieved_xml.contains("STANDARD_IA"),
        "retrieved XML should contain the target storage class"
    );

    // Step 6: Apply the same plan again — should be a no-op (no error).
    let rendered_again = render(&plan).expect("second render should succeed");
    let outcome_again = provider
        .put(&rendered_again, None)
        .await
        .expect("second put should succeed (idempotent)");
    assert!(
        !outcome_again.applied_at.is_empty(),
        "second applied_at timestamp should be set"
    );

    // Step 7: Read back again and verify it still matches.
    let retrieved_again = provider
        .get()
        .await
        .expect("get should succeed after second put")
        .expect("lifecycle should still exist after second put");
    assert!(
        retrieved_again
            .rule_ids
            .contains(&"crab-xorbs-to-ia".to_string()),
        "rule ID should persist after idempotent re-apply"
    );

    // Cleanup: delete the test bucket.
    delete_bucket(&client, &bucket).await;
}

/// Two parallel applies: S3 is last-writer-wins at the provider level.
///
/// S3's `PutBucketLifecycleConfiguration` does NOT support conditional
/// writes (no ETag / If-Match header). When two callers apply different
/// plans concurrently, both succeed and the final state reflects
/// whichever write landed last.
///
/// True CAS conflict detection is handled by `tier::apply` (task 5.3),
/// which reads the lifecycle before writing and detects changes between
/// the read and the write. That layer raises `TierLifecycleConflict`
/// when a concurrent mutation is detected. This test documents the raw
/// S3 behavior that the higher-level CAS logic must account for.
#[ignore]
#[tokio::test]
async fn parallel_applies_last_writer_wins_on_s3() {
    let client = localstack_client().await;
    let bucket = test_bucket_name();

    create_bucket(&client, &bucket).await;

    // Two providers sharing the same bucket — simulates two concurrent
    // callers.
    let provider_a = S3LifecycleProvider::from_client(client.clone(), bucket.clone());
    let provider_b = S3LifecycleProvider::from_client(client.clone(), bucket.clone());

    // Plan A: IA transition at 30 days.
    let plan_a = ia_tier_plan(30);
    let rendered_a = render(&plan_a).expect("render plan_a should succeed");

    // Plan B: IA transition at 60 days.
    let plan_b = ia_tier_plan(60);
    let rendered_b = render(&plan_b).expect("render plan_b should succeed");

    // Apply both concurrently. S3 has no CAS on lifecycle, so both
    // succeed — last writer wins.
    let (result_a, result_b) = tokio::join!(
        provider_a.put(&rendered_a, None),
        provider_b.put(&rendered_b, None),
    );

    // Both puts should succeed — S3 does not reject concurrent writes.
    let outcome_a = result_a.expect("put plan_a should succeed");
    let outcome_b = result_b.expect("put plan_b should succeed");
    assert!(!outcome_a.applied_at.is_empty());
    assert!(!outcome_b.applied_at.is_empty());

    // Read back the final lifecycle state.
    let provider_read = S3LifecycleProvider::from_client(client.clone(), bucket.clone());
    let final_lifecycle = provider_read
        .get()
        .await
        .expect("get should succeed")
        .expect("lifecycle should exist after concurrent puts");

    // The final state must match one of the two plans. We can't predict
    // which one won the race, but the rule ID is the same in both plans
    // so it must be present.
    assert!(
        final_lifecycle
            .rule_ids
            .contains(&"crab-xorbs-to-ia".to_string()),
        "final lifecycle should contain the rule ID"
    );

    // Parse the final XML to check the transition days — it must be
    // either 30 (plan A) or 60 (plan B).
    let final_xml =
        String::from_utf8(final_lifecycle.body.clone()).expect("body should be valid UTF-8");
    let has_30_days = final_xml.contains("<Days>30</Days>");
    let has_60_days = final_xml.contains("<Days>60</Days>");
    assert!(
        has_30_days || has_60_days,
        "final lifecycle should have Days=30 or Days=60, got: {final_xml}"
    );
    // Exactly one of the two plans should be present (not a merge).
    assert!(
        has_30_days != has_60_days,
        "final lifecycle should reflect exactly one plan, not a merge"
    );

    // Cleanup.
    delete_bucket(&client, &bucket).await;
}
