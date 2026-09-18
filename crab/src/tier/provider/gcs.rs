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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use google_cloud_storage::http::buckets::get::GetBucketRequest;
use google_cloud_token::TokenSource;
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

/// GCS lifecycle provider backed by the GCS JSON API and
/// `google-cloud-storage` object client.
///
/// Implements both [`LifecycleProvider`] (lifecycle rule CRUD with
/// generation-number CAS) and [`RestoreBackend`] (GCS Archive returns
/// `Ready` unconditionally — per-GB retrieval fee modeled in
/// `cost::pricing`).
///
/// Lifecycle calls use the default GCP credential chain and send the
/// rendered JSON unchanged. This is intentional: the pinned object SDK
/// cannot represent `matchesPrefix` or a conditional bucket patch.
pub struct GcsLifecycleProvider {
    client: google_cloud_storage::client::Client,
    bucket: String,
    rest: Option<GcsRestClient>,
}

/// Small REST adapter for the two GCS bucket operations that the pinned SDK
/// cannot represent without losing `matchesPrefix` or an If-Match guard.
/// Keeping this adapter next to the provider makes the wire contract explicit
/// and lets the SDK continue to own authenticated object operations.
struct GcsRestClient {
    http: reqwest::Client,
    endpoint: String,
    token_source: Arc<dyn TokenSource>,
}

impl GcsRestClient {
    fn bucket_url(&self, bucket: &str) -> String {
        format!(
            "{}/storage/v1/b/{}",
            self.endpoint.trim_end_matches('/'),
            urlencoding::encode(bucket)
        )
    }

    async fn authorization(&self) -> Result<String> {
        self.token_source.token().await.map_err(|error| {
            CrabError::Internal(format!("GCS lifecycle authentication failed: {error}"))
        })
    }

    async fn get(&self, bucket: &str) -> Result<Option<RenderedLifecycle>> {
        let response = self
            .http
            .get(self.bucket_url(bucket))
            .query(&[("fields", "lifecycle,metageneration")])
            .header(reqwest::header::AUTHORIZATION, self.authorization().await?)
            .send()
            .await
            .map_err(|error| CrabError::Internal(format!("GCS lifecycle GET failed: {error}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("GCS lifecycle GET response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(gcs_rest_status_error("GET lifecycle", status, &body));
        }

        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
            CrabError::Internal(format!("GCS lifecycle GET returned invalid JSON: {error}"))
        })?;
        let Some(lifecycle) = value.get("lifecycle") else {
            return Ok(None);
        };
        if lifecycle.is_null() {
            return Ok(None);
        }
        let rules = lifecycle
            .get("rule")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| CrabError::CorruptObject {
                path: format!("gcs://{bucket}/lifecycle"),
                reason: "lifecycle response has no rule array".to_owned(),
            })?;
        if rules.is_empty() {
            return Ok(None);
        }
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "lifecycle": { "rule": rules },
        }))
        .map_err(|error| {
            CrabError::Internal(format!("GCS lifecycle response serialize failed: {error}"))
        })?;
        Ok(Some(RenderedLifecycle {
            format: Format::Json,
            body,
            rule_ids: gcs_rule_ids(rules),
        }))
    }

    async fn patch(
        &self,
        bucket: &str,
        lifecycle: serde_json::Value,
        guard: Option<u64>,
    ) -> Result<PutOutcome> {
        let mut request = self
            .http
            .patch(self.bucket_url(bucket))
            .query(&[("fields", "lifecycle,metageneration")])
            .header(reqwest::header::AUTHORIZATION, self.authorization().await?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&serde_json::json!({ "lifecycle": lifecycle }));
        if let Some(generation) = guard {
            request = request.query(&[("ifMetagenerationMatch", generation.to_string())]);
        }
        let response = request
            .send()
            .await
            .map_err(|error| CrabError::Internal(format!("GCS lifecycle PATCH failed: {error}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("GCS lifecycle PATCH response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(CrabError::CasConflict {
                path: format!("gcs://{bucket}/lifecycle"),
                expected_etag: guard.map(|generation| format!("generation:{generation}")),
            });
        }
        if !status.is_success() {
            return Err(gcs_rest_status_error("PATCH lifecycle", status, &body));
        }
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
            CrabError::Internal(format!(
                "GCS lifecycle PATCH returned invalid JSON: {error}"
            ))
        })?;
        let metageneration = parse_metageneration(
            value.get("metageneration"),
            &format!("gcs://{bucket}/lifecycle"),
        )?;
        Ok(PutOutcome {
            new_guard: Guard::Generation(metageneration),
            applied_at: now_rfc3339(),
        })
    }

    async fn generation(&self, bucket: &str) -> Result<Option<Guard>> {
        let response = self
            .http
            .get(self.bucket_url(bucket))
            .query(&[("fields", "metageneration")])
            .header(reqwest::header::AUTHORIZATION, self.authorization().await?)
            .send()
            .await
            .map_err(|error| CrabError::Internal(format!("GCS bucket GET failed: {error}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("GCS bucket GET response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(gcs_rest_status_error("GET bucket", status, &body));
        }
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
            CrabError::Internal(format!("GCS bucket GET returned invalid JSON: {error}"))
        })?;
        let metageneration =
            parse_metageneration(value.get("metageneration"), &format!("gcs://{bucket}"))?;
        Ok(Some(Guard::Generation(metageneration)))
    }
}

fn gcs_rest_status_error(operation: &str, status: reqwest::StatusCode, body: &[u8]) -> CrabError {
    let detail = String::from_utf8_lossy(body);
    CrabError::Internal(format!("GCS {operation} returned HTTP {status}: {detail}"))
}

/// Decode the JSON API's int64 metageneration, which is serialized as a
/// decimal string by GCS but may be a JSON number in compatible emulators.
fn parse_metageneration(value: Option<&serde_json::Value>, path: &str) -> Result<u64> {
    let value = value.ok_or_else(|| CrabError::CorruptObject {
        path: path.to_owned(),
        reason: "GCS response omitted metageneration".to_owned(),
    })?;
    let generation = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
        .ok_or_else(|| CrabError::CorruptObject {
            path: path.to_owned(),
            reason: "GCS response has an invalid metageneration".to_owned(),
        })?;
    if generation == 0 {
        return Err(CrabError::CorruptObject {
            path: path.to_owned(),
            reason: "GCS response has a zero metageneration".to_owned(),
        });
    }
    Ok(generation)
}

/// GCS lifecycle rules have no wire-level IDs. Use stable synthetic IDs for
/// conflict handling, treating only Crab's exact xorb prefix as managed. A
/// user rule is therefore never silently discarded by a non-merge apply.
fn gcs_rule_ids(rules: &[serde_json::Value]) -> Vec<String> {
    rules
        .iter()
        .map(|rule| {
            let digest = blake3::hash(&serde_json::to_vec(rule).unwrap_or_default());
            let managed = rule
                .get("condition")
                .and_then(|condition| condition.get("matchesPrefix"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|prefixes| {
                    !prefixes.is_empty()
                        && prefixes
                            .iter()
                            .all(|prefix| prefix.as_str() == Some(".crab/xorbs/"))
                });
            if managed {
                format!("crab-gcs-{}", digest.to_hex())
            } else {
                format!("gcs-user-{}", digest.to_hex())
            }
        })
        .collect()
}

impl GcsLifecycleProvider {
    /// Build a GCS lifecycle provider for the given bucket.
    ///
    /// Uses the default GCP credential chain for both object and lifecycle
    /// requests.
    pub async fn new(bucket: String) -> Result<Self> {
        let config = google_cloud_storage::client::ClientConfig::default()
            .with_auth()
            .await
            .map_err(|e| {
                CrabError::Internal(format!("GCS client auth initialization failed: {e}"))
            })?;
        let endpoint = config.storage_endpoint.clone();
        let token_source = config
            .token_source_provider
            .as_ref()
            .map(|provider| provider.token_source())
            .ok_or_else(|| CrabError::Configuration {
                key: "tier.gcs.credentials".to_owned(),
                origin: "GCS authentication did not provide a token source".to_owned(),
            })?;
        let client = google_cloud_storage::client::Client::new(config);
        Ok(Self {
            client,
            bucket,
            rest: Some(GcsRestClient {
                http: reqwest::Client::new(),
                endpoint,
                token_source,
            }),
        })
    }

    /// Build a GCS lifecycle provider from an existing client.
    ///
    /// Useful for testing with a client configured to point at
    /// `fake-gcs-server` or other test doubles.
    pub fn from_client(client: google_cloud_storage::client::Client, bucket: String) -> Self {
        Self {
            client,
            bucket,
            rest: None,
        }
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
        if let Some(rest) = &self.rest {
            return rest.get(&self.bucket).await;
        }

        // Fetch the bucket metadata via `storage.buckets.get`. We reach
        // for the full bucket rather than a projected subset because the
        // metageneration (needed for CAS) lives on the top-level Bucket
        // object alongside the optional lifecycle config.
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

                // The SDK's typed `Condition` omits `matchesPrefix`, so a
                // configured lifecycle cannot be safely represented through
                // this constructor. Real providers use the REST adapter above;
                // fail closed for SDK-only test clients rather than widening a
                // rule's scope on the next apply.
                Err(CrabError::Configuration {
                    key: "tier.gcs.lifecycle_client".to_owned(),
                    origin: "configured lifecycle requires the authenticated REST adapter; construct the provider with GcsLifecycleProvider::new".to_owned(),
                })
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
        let Some(rest) = &self.rest else {
            return Err(CrabError::Configuration {
                key: "tier.gcs.lifecycle_client".to_owned(),
                origin: "lifecycle writes require the authenticated REST adapter; construct the provider with GcsLifecycleProvider::new".to_owned(),
            });
        };
        if doc.format != Format::Json {
            return Err(CrabError::IncompatibleFormat {
                required: "GCS lifecycle JSON".to_owned(),
                found: format!("{:?}", doc.format),
            });
        }
        let value: serde_json::Value =
            serde_json::from_slice(&doc.body).map_err(|error| CrabError::Configuration {
                key: "tier.gcs.lifecycle".to_owned(),
                origin: format!("rendered lifecycle is not valid JSON: {error}"),
            })?;
        let lifecycle =
            value
                .get("lifecycle")
                .cloned()
                .ok_or_else(|| CrabError::Configuration {
                    key: "tier.gcs.lifecycle".to_owned(),
                    origin: "rendered lifecycle is missing the lifecycle object".to_owned(),
                })?;
        let generation = match guard {
            None => None,
            Some(Guard::Generation(generation)) => Some(generation),
            Some(Guard::Etag(_) | Guard::None) => {
                return Err(CrabError::Configuration {
                    key: "tier.gcs.lifecycle.guard".to_owned(),
                    origin: "GCS lifecycle writes require a generation guard".to_owned(),
                });
            }
        };
        rest.patch(&self.bucket, lifecycle, generation).await
    }

    async fn delete(&self, guard: Option<Guard>) -> Result<PutOutcome> {
        let Some(rest) = &self.rest else {
            return Err(CrabError::Configuration {
                key: "tier.gcs.lifecycle_client".to_owned(),
                origin: "lifecycle deletion requires the authenticated REST adapter; construct the provider with GcsLifecycleProvider::new".to_owned(),
            });
        };
        let generation = match guard {
            Some(Guard::Generation(generation)) => Some(generation),
            None => {
                return Err(CrabError::TierProviderUnsupported {
                    provider: "GCS lifecycle deletion requires a generation guard".to_owned(),
                });
            }
            Some(Guard::Etag(_) | Guard::None) => {
                return Err(CrabError::Configuration {
                    key: "tier.gcs.lifecycle.guard".to_owned(),
                    origin: "GCS lifecycle deletion requires a generation guard".to_owned(),
                });
            }
        };
        rest.patch(&self.bucket, serde_json::Value::Null, generation)
            .await
    }

    fn equivalent(
        &self,
        current: &RenderedLifecycle,
        intended: &RenderedLifecycle,
    ) -> Result<bool> {
        if current.format != Format::Json || intended.format != Format::Json {
            return Ok(false);
        }
        let current: serde_json::Value =
            serde_json::from_slice(&current.body).map_err(|error| CrabError::CorruptObject {
                path: format!("gcs://{}/lifecycle", self.bucket),
                reason: format!("current lifecycle is not valid JSON: {error}"),
            })?;
        let intended: serde_json::Value =
            serde_json::from_slice(&intended.body).map_err(|error| CrabError::Configuration {
                key: "tier.gcs.lifecycle".to_owned(),
                origin: format!("intended lifecycle is not valid JSON: {error}"),
            })?;
        Ok(current == intended)
    }

    async fn cas_guard(&self) -> Result<Option<Guard>> {
        if let Some(rest) = &self.rest {
            return rest.generation(&self.bucket).await;
        }

        // CAS on GCS lifecycle uses the bucket's metageneration as the
        // guard. `get_bucket` is the cheapest call that returns it —
        // projected fields aren't available in the pinned crate — so we
        // pay one full-bucket GET per push. Metageneration is an `i64`
        // but always non-negative in practice; we widen to `u64` via a
        // clamped cast so the `Guard::Generation` variant stays unsigned.
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

    #[test]
    fn metageneration_accepts_gcs_string_encoding() {
        let value = serde_json::json!("17");
        assert_eq!(
            parse_metageneration(Some(&value), "gcs://bucket").unwrap(),
            17
        );
    }

    #[test]
    fn metageneration_rejects_zero_or_malformed_values() {
        for value in [serde_json::json!(0), serde_json::json!("nope")] {
            assert!(parse_metageneration(Some(&value), "gcs://bucket").is_err());
        }
    }

    #[test]
    fn synthetic_rule_ids_keep_user_rules_managed() {
        let rules = vec![
            serde_json::json!({
                "action": {"type": "SetStorageClass", "storageClass": "NEARLINE"},
                "condition": {"age": 30, "matchesPrefix": [".crab/xorbs/"]}
            }),
            serde_json::json!({
                "action": {"type": "Delete"},
                "condition": {"age": 365, "matchesPrefix": ["backups/"]}
            }),
        ];

        let ids = gcs_rule_ids(&rules);

        assert!(ids[0].starts_with("crab-gcs-"));
        assert!(ids[1].starts_with("gcs-user-"));
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
