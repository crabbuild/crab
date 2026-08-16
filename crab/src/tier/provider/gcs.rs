//! GCS lifecycle provider and restore backend.
//!
//! Produces JSON compatible with the GCS `storage.buckets.patch`
//! lifecycle API. The document shape is:
//!
//! ```json
//! {
//!   "lifecycle": {
//!     "rule": [
//!       {
//!         "action": { "type": "SetStorageClass", "storageClass": "NEARLINE" },
//!         "condition": { "age": 30, "matchesPrefix": [".crab/xorbs/"] }
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Rule order is deterministic (sorted by rule ID) for snapshot-test
//! stability. Rule IDs are tracked in `RenderedLifecycle::rule_ids`
//! but are not part of the GCS lifecycle wire format.
//!
//! The [`GcsLifecycleProvider`] struct implements both
//! [`LifecycleProvider`] (lifecycle rule CRUD via generation-number
//! CAS) and [`RestoreBackend`] (GCS Archive returns `Ready`
//! unconditionally — no restore step needed, per-GB retrieval fee
//! modeled in `cost::pricing`).
//!
//! All code in this module is gated behind `#[cfg(feature = "tier-gcs")]`
//! at the module level (see `provider/mod.rs`).

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tracing::debug;

use crate::core::error::{CrabError, Result};

use super::{
    Format, Guard, LifecycleProvider, ObjectPath, Provider, PutOutcome, RenderedLifecycle,
    RestoreBackend, RestoreHandle, RestoreState, RestoreTier, StorageClass, TierPlan, TierRule,
    Transition,
};

// ── JSON rendering ──────────────────────────────────────────────────

/// Render a [`TierPlan`] into GCS `storage.buckets.patch` lifecycle JSON.
///
/// Rules are sorted by ID before rendering so the output is
/// deterministic regardless of input order.
pub fn render(plan: &TierPlan) -> Result<RenderedLifecycle> {
    let mut sorted_rules: Vec<&TierRule> = plan.rules.iter().collect();
    sorted_rules.sort_by(|a, b| a.id.cmp(&b.id));

    let mut gcs_rules: Vec<GcsRule> = Vec::new();

    for rule in &sorted_rules {
        for transition in &rule.transitions {
            gcs_rules.push(build_gcs_rule(rule, transition));
        }
    }

    let doc = GcsLifecycleDocument {
        lifecycle: GcsLifecycle { rule: gcs_rules },
    };

    let body = serde_json::to_vec_pretty(&doc).map_err(|e| {
        crate::core::error::CrabError::Internal(format!(
            "GCS lifecycle JSON serialization failed: {e}"
        ))
    })?;

    let rule_ids: Vec<String> = sorted_rules.iter().map(|r| r.id.clone()).collect();

    Ok(RenderedLifecycle {
        format: Format::Json,
        body,
        rule_ids,
    })
}

/// Build a single GCS lifecycle rule from a tier rule and transition.
fn build_gcs_rule(rule: &TierRule, transition: &Transition) -> GcsRule {
    GcsRule {
        action: GcsAction {
            r#type: "SetStorageClass".into(),
            storage_class: gcs_class_str(transition.to_class).into(),
        },
        condition: GcsCondition {
            age: transition.days,
            matches_prefix: vec![rule.prefix.clone()],
        },
    }
}

/// Map a [`StorageClass`] to the GCS API wire-format string.
#[expect(
    clippy::match_same_arms,
    reason = "GcsStandard is the canonical arm; non-GCS classes are a defensive fallback"
)]
fn gcs_class_str(class: StorageClass) -> &'static str {
    match class {
        StorageClass::GcsStandard => "STANDARD",
        StorageClass::GcsNearline => "NEARLINE",
        StorageClass::GcsColdline => "COLDLINE",
        StorageClass::GcsArchive => "ARCHIVE",
        // Non-GCS classes should not appear in GCS lifecycle JSON, but
        // we fall back to STANDARD rather than panicking.
        StorageClass::S3Standard
        | StorageClass::S3IntelligentTiering
        | StorageClass::S3StandardIa
        | StorageClass::S3OneZoneIa
        | StorageClass::S3GlacierInstantRetrieval
        | StorageClass::S3GlacierFlexibleRetrieval
        | StorageClass::S3GlacierDeepArchive
        | StorageClass::AzureHot
        | StorageClass::AzureCool
        | StorageClass::AzureCold
        | StorageClass::AzureArchive
        | StorageClass::Unknown => "STANDARD",
    }
}

// ── Serialization types ─────────────────────────────────────────────

/// Top-level GCS lifecycle document.
#[derive(Debug, Serialize)]
struct GcsLifecycleDocument {
    lifecycle: GcsLifecycle,
}

/// The `lifecycle` object containing the rule array.
#[derive(Debug, Serialize)]
struct GcsLifecycle {
    rule: Vec<GcsRule>,
}

/// A single GCS lifecycle rule with action and condition.
#[derive(Debug, Serialize)]
struct GcsRule {
    action: GcsAction,
    condition: GcsCondition,
}

/// The action to take when the condition is met.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GcsAction {
    r#type: String,
    storage_class: String,
}

/// The condition that triggers the action.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GcsCondition {
    age: u32,
    matches_prefix: Vec<String>,
}

// ── Empty tier list (GCS Archive needs no restore) ──────────────────

/// GCS Archive is readable directly at a per-GB retrieval fee — no
/// restore step is needed. All GCS classes return an empty tier list.
static NO_TIERS: &[RestoreTier] = &[];

// ── GcsLifecycleProvider ────────────────────────────────────────────

/// GCS lifecycle provider backed by `google-cloud-storage`.
///
/// Implements both [`LifecycleProvider`] (lifecycle rule CRUD with
/// generation-number CAS) and [`RestoreBackend`] (GCS Archive returns
/// `Ready` unconditionally — per-GB retrieval fee modeled in
/// `cost::pricing`).
///
/// # Credential adapter
///
/// The real integration with `auth::CredentialProvider` will be wired
/// when the auth adapter shim is available. For now the client is built
/// from the default GCP credential chain.
pub struct GcsLifecycleProvider {
    client: google_cloud_storage::client::Client,
    bucket: String,
}

impl GcsLifecycleProvider {
    /// Build a GCS lifecycle provider for the given bucket.
    ///
    /// Uses the default GCP credential chain. The credential adapter
    /// from `auth::CredentialProvider` will be wired in a follow-up.
    // TODO(crab-storage-economy): wire `auth::CredentialProvider` via
    // a `google_cloud_token::TokenSourceProvider` adapter when the auth
    // shim is available.
    pub async fn new(bucket: String) -> Result<Self> {
        let config = google_cloud_storage::client::ClientConfig::default()
            .with_auth()
            .await
            .map_err(|e| {
                CrabError::Internal(format!("GCS client auth initialization failed: {e}"))
            })?;
        let client = google_cloud_storage::client::Client::new(config);
        Ok(Self { client, bucket })
    }

    /// Build a GCS lifecycle provider from an existing client.
    ///
    /// Useful for testing with a client configured to point at
    /// `fake-gcs-server` or other test doubles.
    pub fn from_client(client: google_cloud_storage::client::Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    /// Return a reference to the underlying GCS client.
    pub fn client(&self) -> &google_cloud_storage::client::Client {
        &self.client
    }

    /// Return the bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

#[async_trait]
impl LifecycleProvider for GcsLifecycleProvider {
    fn kind(&self) -> Provider {
        Provider::Gcs
    }

    fn render(&self, plan: &TierPlan) -> Result<RenderedLifecycle> {
        render(plan)
    }

    async fn get(&self) -> Result<Option<RenderedLifecycle>> {
        // Fetch the bucket metadata via `storage.buckets.get`. We reach
        // for the full bucket rather than a projected subset because the
        // metageneration (needed for CAS) lives on the top-level Bucket
        // object alongside the optional lifecycle config.
        use google_cloud_storage::http::buckets::get::GetBucketRequest;

        let req = GetBucketRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };

        match self.client.get_bucket(&req).await {
            Ok(bucket) => {
                let Some(lifecycle) = bucket.lifecycle else {
                    debug!(
                        bucket = %self.bucket,
                        metageneration = bucket.metageneration,
                        "GCS get lifecycle: bucket has no lifecycle configured"
                    );
                    return Ok(None);
                };

                if lifecycle.rule.is_empty() {
                    debug!(
                        bucket = %self.bucket,
                        "GCS get lifecycle: lifecycle is empty"
                    );
                    return Ok(None);
                }

                // Serialize the SDK-returned lifecycle back to our
                // canonical JSON shape so the caller receives a
                // `RenderedLifecycle` identical in format to what
                // `render()` produces. We cannot round-trip back to
                // `TierPlan` through the SDK's typed `Condition`: the
                // pinned crate version (0.24) omits `matches_prefix`
                // from its `Condition` struct, so a typed round-trip
                // would silently drop the prefix filter. Serializing
                // the SDK's `Vec<Rule>` through serde_json preserves
                // whatever the server returned for fields the SDK
                // doesn't model, because the `#[serde(flatten)]`-like
                // behavior is a no-op here — unknown fields are
                // already lost at deserialization. Callers that need
                // prefix-aware read-back should compare rule IDs
                // rather than body bytes.
                let body = serde_json::to_vec_pretty(&serde_json::json!({
                    "lifecycle": { "rule": lifecycle.rule },
                }))
                .map_err(|e| {
                    CrabError::Internal(format!("GCS lifecycle response serialize: {e}"))
                })?;

                // Extract rule IDs as a best effort. This SDK version
                // does not expose rule IDs on `Rule`, so we fall back
                // to an empty list and rely on callers to match by
                // content.
                let rule_ids: Vec<String> = Vec::new();

                debug!(
                    bucket = %self.bucket,
                    metageneration = bucket.metageneration,
                    rules = lifecycle.rule.len(),
                    "GCS get lifecycle: parsed SDK response"
                );

                Ok(Some(RenderedLifecycle {
                    format: Format::Json,
                    body,
                    rule_ids,
                }))
            }
            Err(err) => {
                // The SDK surfaces `NotFound` via the response-code
                // branch of `google_cloud_storage::http::Error`.
                // Anything else is a real error.
                if is_gcs_not_found(&err) {
                    debug!(
                        bucket = %self.bucket,
                        "GCS get lifecycle: bucket not found"
                    );
                    return Ok(None);
                }
                Err(CrabError::Internal(format!(
                    "GCS get_bucket for {} failed: {err}",
                    self.bucket
                )))
            }
        }
    }

    async fn put(&self, doc: &RenderedLifecycle, guard: Option<Guard>) -> Result<PutOutcome> {
        // The pinned `google-cloud-storage 0.24` crate's typed
        // `buckets::lifecycle::rule::Condition` does not carry a
        // `matches_prefix` field, which is the entire reason crab
        // lifecycle rules exist (per-prefix transitions for
        // `.crab/xorbs/`, `.crab/shards/`, etc). Calling
        // `patch_bucket` with the SDK's typed `Lifecycle` would serialize
        // a rule without `matchesPrefix`, making it apply to every object
        // in the bucket — a silent and potentially destructive semantic
        // change (xorbs in every project prefix would transition).
        //
        // Rather than ship a silently-regressing PUT, this path returns
        // a structured error that tells the operator exactly what to do:
        // upgrade the crate or stick with the S3 path for now. The
        // `render()` method still produces valid JSON that can be
        // uploaded via `gcloud storage buckets update --lifecycle-file`
        // out-of-band.
        //
        // See also: the accompanying doc comment on
        // `GcsLifecycleProvider`. Fix plan:
        //
        //   1. Bump `google-cloud-storage` to a version whose
        //      `Condition` exposes `matches_prefix` (tracked upstream).
        //   2. Or route `put` through a hand-rolled HTTP PATCH that
        //      sends our rendered JSON body verbatim, preserving the
        //      prefix filter end-to-end.
        //
        // Until then `put` refuses rather than silently widening the
        // lifecycle rule's scope.
        let _ = (doc, guard);
        Err(CrabError::Internal(
            "GCS lifecycle put is not yet wired: the pinned \
             `google-cloud-storage` crate omits `matches_prefix` from \
             its typed `Condition`, so a typed PATCH would drop the \
             per-prefix filter and widen every rule to the whole \
             bucket. Upload the rendered JSON out-of-band via \
             `gcloud storage buckets update --lifecycle-file` or \
             upgrade the crate."
                .into(),
        ))
    }

    async fn cas_guard(&self) -> Result<Option<Guard>> {
        // CAS on GCS lifecycle uses the bucket's metageneration as the
        // guard. `get_bucket` is the cheapest call that returns it —
        // projected fields aren't available in the pinned crate — so we
        // pay one full-bucket GET per push. Metageneration is an `i64`
        // but always non-negative in practice; we widen to `u64` via a
        // clamped cast so the `Guard::Generation` variant stays unsigned.
        use google_cloud_storage::http::buckets::get::GetBucketRequest;

        let req = GetBucketRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };

        match self.client.get_bucket(&req).await {
            Ok(bucket) => {
                let metageneration: u64 = bucket.metageneration.try_into().unwrap_or(0);
                debug!(
                    bucket = %self.bucket,
                    metageneration,
                    "GCS cas_guard: real metageneration"
                );
                Ok(Some(Guard::Generation(metageneration)))
            }
            Err(err) => {
                if is_gcs_not_found(&err) {
                    Ok(None)
                } else {
                    Err(CrabError::Internal(format!(
                        "GCS cas_guard get_bucket for {} failed: {err}",
                        self.bucket
                    )))
                }
            }
        }
    }
}

#[async_trait]
impl RestoreBackend for GcsLifecycleProvider {
    async fn restore(
        &self,
        path: &ObjectPath,
        _tier: RestoreTier,
        _duration: Duration,
    ) -> Result<RestoreHandle> {
        // GCS Archive does not require a restore step — objects are
        // readable directly at a per-GB retrieval fee. Return a handle
        // immediately.
        debug!(
            bucket = %self.bucket,
            key = %path,
            "GCS restore: no-op (Archive readable directly)"
        );
        Ok(RestoreHandle {
            id: format!("gcs-noop-{path}"),
        })
    }

    async fn state(&self, path: &ObjectPath) -> Result<RestoreState> {
        // GCS Archive is always readable — return Ready unconditionally.
        debug!(
            bucket = %self.bucket,
            key = %path,
            "GCS restore state: Ready (Archive readable directly)"
        );
        Ok(RestoreState::Ready)
    }

    fn supported_tiers(&self, _class: &StorageClass) -> &'static [RestoreTier] {
        // GCS Archive does not need restore — no supported tiers.
        NO_TIERS
    }
}

// ── Helper functions ────────────────────────────────────────────────

/// Return the current time as an RFC 3339 string.
///
/// Currently unused — `put` errors out before reaching the outcome
/// construction. Kept in place so the eventual real-PUT path can use
/// it without reintroducing the helper. See the block comment on the
/// `put` implementation for why PUT is intentionally gated.
#[allow(
    dead_code,
    reason = "reused when put() is wired against an updated SDK"
)]
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", duration.as_secs())
}

/// True when the SDK error represents a 404 (`NoSuchBucket` or missing
/// resource). Centralised here so the `get` and `cas_guard` paths agree
/// on what "bucket absent" looks like.
fn is_gcs_not_found(err: &google_cloud_storage::http::Error) -> bool {
    matches!(err, google_cloud_storage::http::Error::Response(resp) if resp.code == 404)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::provider::{Provider, TierPlan, TierRule, Transition};

    /// Helper to render a plan and return the JSON as a string.
    fn render_json(plan: &TierPlan) -> String {
        let rendered = render(plan).expect("render should succeed");
        assert_eq!(rendered.format, Format::Json);
        String::from_utf8(rendered.body).expect("JSON should be valid UTF-8")
    }

    fn nearline_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::GcsNearline,
        }
    }

    fn archive_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::GcsArchive,
        }
    }

    // ── Snapshot: basic Nearline transition ──────────────────────────

    #[test]
    fn snapshot_basic_nearline_transition() {
        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![TierRule {
                id: "crab-xorbs-to-nearline".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![nearline_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let json = render_json(&plan);
        insta::assert_snapshot!("gcs_basic_nearline_transition", json);
    }

    // ── Snapshot: multiple transitions (Nearline + Archive) ─────────

    #[test]
    fn snapshot_multiple_transitions() {
        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![TierRule {
                id: "crab-xorbs-tiering".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![nearline_transition(30), archive_transition(365)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let json = render_json(&plan);
        insta::assert_snapshot!("gcs_multiple_transitions", json);
    }

    // ── JSON validity ───────────────────────────────────────────────

    #[test]
    fn output_is_valid_json_with_expected_fields() {
        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![TierRule {
                id: "crab-xorbs-to-nearline".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![nearline_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        let parsed: serde_json::Value =
            serde_json::from_slice(&rendered.body).expect("should be valid JSON");

        // Verify top-level structure.
        let lifecycle = parsed.get("lifecycle").expect("should have lifecycle key");
        let rules = lifecycle
            .get("rule")
            .expect("should have rule key")
            .as_array()
            .expect("rule should be an array");
        assert_eq!(rules.len(), 1);

        // Verify rule structure.
        let rule = &rules[0];
        let action = rule.get("action").expect("should have action");
        assert_eq!(
            action.get("type").and_then(|v| v.as_str()),
            Some("SetStorageClass")
        );
        assert_eq!(
            action.get("storageClass").and_then(|v| v.as_str()),
            Some("NEARLINE")
        );

        let condition = rule.get("condition").expect("should have condition");
        assert_eq!(condition.get("age").and_then(|v| v.as_u64()), Some(30));
        let prefixes = condition
            .get("matchesPrefix")
            .and_then(|v| v.as_array())
            .expect("matchesPrefix should be an array");
        assert_eq!(prefixes.len(), 1);
        assert_eq!(prefixes[0].as_str(), Some(".crab/xorbs/"));
    }

    // ── Rendered format is Json ─────────────────────────────────────

    #[test]
    fn rendered_format_is_json() {
        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![nearline_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        assert_eq!(rendered.format, Format::Json);
        assert!(!rendered.body.is_empty());
    }

    // ── Rule IDs are sorted deterministically ───────────────────────

    #[test]
    fn rule_ids_sorted_deterministically() {
        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![
                TierRule {
                    id: "crab-z-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![nearline_transition(30)],
                    noncurrent_expiration_days: None,
                    min_object_size_bytes: None,
                },
                TierRule {
                    id: "crab-a-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![archive_transition(365)],
                    noncurrent_expiration_days: None,
                    min_object_size_bytes: None,
                },
            ],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        assert_eq!(rendered.rule_ids, vec!["crab-a-rule", "crab-z-rule"]);
    }

    // ── GCS class string mapping ────────────────────────────────────

    #[test]
    fn gcs_class_str_mapping() {
        assert_eq!(gcs_class_str(StorageClass::GcsStandard), "STANDARD");
        assert_eq!(gcs_class_str(StorageClass::GcsNearline), "NEARLINE");
        assert_eq!(gcs_class_str(StorageClass::GcsColdline), "COLDLINE");
        assert_eq!(gcs_class_str(StorageClass::GcsArchive), "ARCHIVE");
    }

    #[test]
    fn gcs_class_str_non_gcs_falls_back_to_standard() {
        assert_eq!(gcs_class_str(StorageClass::S3Standard), "STANDARD");
        assert_eq!(gcs_class_str(StorageClass::AzureHot), "STANDARD");
        assert_eq!(gcs_class_str(StorageClass::Unknown), "STANDARD");
    }

    // ── GcsLifecycleProvider: kind ──────────────────────────────────

    #[tokio::test]
    async fn provider_kind_is_gcs() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "test-bucket".into());
        assert_eq!(provider.kind(), Provider::Gcs);
    }

    // ── GcsLifecycleProvider: render delegates ──────────────────────

    #[tokio::test]
    async fn provider_render_delegates_to_module_render() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "test-bucket".into());

        let plan = TierPlan {
            provider: Provider::Gcs,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![nearline_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = provider.render(&plan).expect("render should succeed");
        assert_eq!(rendered.format, Format::Json);
        assert!(!rendered.body.is_empty());
        assert_eq!(rendered.rule_ids, vec!["crab-test"]);
    }

    // ── GcsLifecycleProvider: bucket accessor ───────────────────────

    #[test]
    fn provider_bucket_accessor() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "my-bucket".into());
        assert_eq!(provider.bucket(), "my-bucket");
    }

    // ── RestoreBackend: state always Ready ──────────────────────────

    #[tokio::test]
    async fn restore_state_always_ready() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "test-bucket".into());

        let state = provider
            .state(&"some/object/path".to_string())
            .await
            .expect("state should succeed");
        assert_eq!(state, RestoreState::Ready);
    }

    // ── RestoreBackend: restore returns handle immediately ──────────

    #[tokio::test]
    async fn restore_returns_handle_immediately() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "test-bucket".into());

        let handle = provider
            .restore(
                &"some/object/path".to_string(),
                RestoreTier::Standard,
                Duration::from_secs(86_400 * 7),
            )
            .await
            .expect("restore should succeed");
        assert!(handle.id.contains("gcs-noop-"));
    }

    // ── RestoreBackend: supported_tiers always empty ────────────────

    #[test]
    fn supported_tiers_always_empty() {
        let config = google_cloud_storage::client::ClientConfig::default().anonymous();
        let client = google_cloud_storage::client::Client::new(config);
        let provider = GcsLifecycleProvider::from_client(client, "test-bucket".into());

        assert!(
            provider
                .supported_tiers(&StorageClass::GcsArchive)
                .is_empty()
        );
        assert!(
            provider
                .supported_tiers(&StorageClass::GcsStandard)
                .is_empty()
        );
        assert!(
            provider
                .supported_tiers(&StorageClass::GcsNearline)
                .is_empty()
        );
        assert!(
            provider
                .supported_tiers(&StorageClass::GcsColdline)
                .is_empty()
        );
    }
}
