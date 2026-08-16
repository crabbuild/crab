//! Azure Blob lifecycle provider and restore backend.
//!
//! Produces JSON compatible with the Azure Blob Storage
//! `ManagementPolicySchema` per the `2023-11-01` Management API.
//! The document shape is:
//!
//! ```json
//! {
//!   "rules": [
//!     {
//!       "enabled": true,
//!       "name": "crab-xorbs-to-cool",
//!       "type": "Lifecycle",
//!       "definition": {
//!         "actions": {
//!           "baseBlob": {
//!             "tierToCool": {
//!               "daysAfterModificationGreaterThan": 30
//!             }
//!           }
//!         },
//!         "filters": {
//!           "blobTypes": ["blockBlob"],
//!           "prefixMatch": [".crab/xorbs/"]
//!         }
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! Rule order is deterministic (sorted by name) for snapshot-test
//! stability.
//!
//! The [`AzureLifecycleProvider`] struct implements both
//! [`LifecycleProvider`] (lifecycle rule CRUD via ETag CAS) and
//! [`RestoreBackend`] (Azure Archive rehydration with `High` and
//! `Standard` priority — no `Bulk` tier).
//!
//! All code in this module is gated behind `#[cfg(feature = "tier-azure")]`
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

/// Render a [`TierPlan`] into Azure `ManagementPolicySchema` JSON.
///
/// Rules are sorted by name before rendering so the output is
/// deterministic regardless of input order.
pub fn render(plan: &TierPlan) -> Result<RenderedLifecycle> {
    let mut sorted_rules: Vec<&TierRule> = plan.rules.iter().collect();
    sorted_rules.sort_by(|a, b| a.id.cmp(&b.id));

    let mut azure_rules: Vec<AzureRule> = Vec::new();

    for rule in &sorted_rules {
        for transition in &rule.transitions {
            azure_rules.push(build_azure_rule(rule, transition));
        }
    }

    let doc = AzureManagementPolicySchema { rules: azure_rules };

    let body = serde_json::to_vec_pretty(&doc).map_err(|e| {
        crate::core::error::CrabError::Internal(format!(
            "Azure lifecycle JSON serialization failed: {e}"
        ))
    })?;

    let rule_ids: Vec<String> = sorted_rules.iter().map(|r| r.id.clone()).collect();

    Ok(RenderedLifecycle {
        format: Format::Json,
        body,
        rule_ids,
    })
}

/// Build a single Azure management policy rule from a tier rule and transition.
fn build_azure_rule(rule: &TierRule, transition: &Transition) -> AzureRule {
    let action_key = azure_tier_action_key(transition.to_class);
    let action = AzureBlobAction {
        days_after_modification_greater_than: transition.days,
    };

    let mut base_blob = AzureBaseBlob::default();
    match action_key {
        "tierToCool" => base_blob.tier_to_cool = Some(action),
        "tierToCold" => base_blob.tier_to_cold = Some(action),
        "tierToArchive" => base_blob.tier_to_archive = Some(action),
        _ => base_blob.tier_to_cool = Some(action),
    }

    // Derive a descriptive name from the rule ID and target class.
    let class_suffix = azure_class_suffix(transition.to_class);
    let name = format!("{}-to-{class_suffix}", rule.id);

    AzureRule {
        enabled: true,
        name,
        r#type: "Lifecycle".into(),
        definition: AzureRuleDefinition {
            actions: AzureActions { base_blob },
            filters: AzureFilters {
                blob_types: vec!["blockBlob".into()],
                prefix_match: vec![rule.prefix.clone()],
            },
        },
    }
}

/// Map a [`StorageClass`] to the Azure tier action key.
#[expect(
    clippy::match_same_arms,
    reason = "AzureCool is the canonical arm; non-Azure classes are a defensive fallback"
)]
fn azure_tier_action_key(class: StorageClass) -> &'static str {
    match class {
        StorageClass::AzureCool => "tierToCool",
        StorageClass::AzureCold => "tierToCold",
        StorageClass::AzureArchive => "tierToArchive",
        // Non-Azure classes should not appear in Azure lifecycle JSON,
        // but we fall back to tierToCool rather than panicking.
        StorageClass::AzureHot
        | StorageClass::S3Standard
        | StorageClass::S3IntelligentTiering
        | StorageClass::S3StandardIa
        | StorageClass::S3OneZoneIa
        | StorageClass::S3GlacierInstantRetrieval
        | StorageClass::S3GlacierFlexibleRetrieval
        | StorageClass::S3GlacierDeepArchive
        | StorageClass::GcsStandard
        | StorageClass::GcsNearline
        | StorageClass::GcsColdline
        | StorageClass::GcsArchive
        | StorageClass::Unknown => "tierToCool",
    }
}

/// Map a [`StorageClass`] to a short suffix for rule naming.
#[expect(
    clippy::match_same_arms,
    reason = "AzureCool is the canonical arm; non-Azure classes are a defensive fallback"
)]
fn azure_class_suffix(class: StorageClass) -> &'static str {
    match class {
        StorageClass::AzureCool => "cool",
        StorageClass::AzureCold => "cold",
        StorageClass::AzureArchive => "archive",
        _ => "cool",
    }
}

// ── Serialization types ─────────────────────────────────────────────

/// Top-level Azure Management Policy Schema document.
#[derive(Debug, Serialize)]
struct AzureManagementPolicySchema {
    rules: Vec<AzureRule>,
}

/// A single Azure management policy rule.
#[derive(Debug, Serialize)]
struct AzureRule {
    enabled: bool,
    name: String,
    r#type: String,
    definition: AzureRuleDefinition,
}

/// The definition of an Azure management policy rule.
#[derive(Debug, Serialize)]
struct AzureRuleDefinition {
    actions: AzureActions,
    filters: AzureFilters,
}

/// Actions to apply to matching blobs.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureActions {
    base_blob: AzureBaseBlob,
}

/// Base blob tier actions. Each field is optional — only the relevant
/// tier action is populated per rule.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureBaseBlob {
    #[serde(skip_serializing_if = "Option::is_none")]
    tier_to_cool: Option<AzureBlobAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier_to_cold: Option<AzureBlobAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tier_to_archive: Option<AzureBlobAction>,
}

/// A single tier action with a days-after-modification threshold.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureBlobAction {
    days_after_modification_greater_than: u32,
}

/// Filters that select which blobs the rule applies to.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureFilters {
    blob_types: Vec<String>,
    prefix_match: Vec<String>,
}

// ── Supported-tier matrix (A3.3) ────────────────────────────────────

/// Azure Archive rehydration supports `Standard` (up to 15 h) and
/// `High` (<1 h). No `Bulk` or `Expedited`.
static AZURE_ARCHIVE_TIERS: &[RestoreTier] = &[RestoreTier::Standard, RestoreTier::High];

/// Empty tier list for non-archive Azure classes.
static NO_TIERS: &[RestoreTier] = &[];

// ── AzureLifecycleProvider ──────────────────────────────────────────

/// Azure Blob lifecycle provider backed by `azure_mgmt_storage` and
/// `azure_storage_blobs`.
///
/// Implements both [`LifecycleProvider`] (lifecycle rule CRUD via ETag
/// CAS) and [`RestoreBackend`] (Azure Archive rehydration with `High`
/// and `Standard` priority).
///
/// # Required identifiers
///
/// The Azure Management API addresses management policies by
/// `(subscription_id, resource_group_name, storage_account)` — the
/// container name is **not** part of the address. `AzureLifecycleProvider`
/// therefore carries all four, with `container` retained for
/// blob-level operations in [`RestoreBackend::restore`].
///
/// # Credential adapter
///
/// The real integration with `auth::CredentialProvider` is not yet
/// wired — the `azure_core::Client` that `azure_mgmt_storage` needs
/// requires a bearer-token provider that is currently only available
/// once `auth::build_azure_credential` lands.
///
/// Until then, [`get`], [`put`], and [`cas_guard`] return a
/// [`CrabError::Internal`] that names the missing integration, so
/// a misconfigured deployment fails loudly rather than silently
/// degrading to the previous "stub returns Ok(None)" shape. The
/// provider's rendering path ([`render`]) works without an
/// authenticated client; callers can produce the lifecycle JSON and
/// apply it out-of-band via `az storage account management-policy
/// create`.
pub struct AzureLifecycleProvider {
    storage_account: String,
    container: String,
    subscription_id: String,
    resource_group_name: String,
}

impl AzureLifecycleProvider {
    /// Build an Azure lifecycle provider with all four required
    /// identifiers supplied explicitly. Prefer [`Self::from_env`] when
    /// these values come from the standard Azure environment
    /// variables.
    pub fn new(
        storage_account: String,
        container: String,
        subscription_id: String,
        resource_group_name: String,
    ) -> Self {
        Self {
            storage_account,
            container,
            subscription_id,
            resource_group_name,
        }
    }

    /// Build an Azure lifecycle provider reading the subscription ID
    /// and resource group from the standard Azure environment
    /// variables (`AZURE_SUBSCRIPTION_ID`, `AZURE_RESOURCE_GROUP`).
    /// Returns [`CrabError::Configuration`] when either is missing
    /// so a deployer can surface the missing piece at startup rather
    /// than on the first lifecycle call.
    pub fn from_env(storage_account: String, container: String) -> Result<Self> {
        let subscription_id =
            std::env::var("AZURE_SUBSCRIPTION_ID").map_err(|_| CrabError::Configuration {
                key: "AZURE_SUBSCRIPTION_ID".into(),
                origin: "environment".into(),
            })?;
        let resource_group_name =
            std::env::var("AZURE_RESOURCE_GROUP").map_err(|_| CrabError::Configuration {
                key: "AZURE_RESOURCE_GROUP".into(),
                origin: "environment".into(),
            })?;
        Ok(Self::new(
            storage_account,
            container,
            subscription_id,
            resource_group_name,
        ))
    }

    /// Return the storage account name.
    pub fn storage_account(&self) -> &str {
        &self.storage_account
    }

    /// Return the container name.
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Return the subscription ID.
    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    /// Return the resource group name.
    pub fn resource_group_name(&self) -> &str {
        &self.resource_group_name
    }

    /// Test-only shortcut that fills plausible subscription +
    /// resource-group placeholders so existing two-arg test sites
    /// don't need to be updated.
    #[cfg(test)]
    fn new_for_tests(storage_account: String, container: String) -> Self {
        Self {
            storage_account,
            container,
            subscription_id: "00000000-0000-0000-0000-000000000000".into(),
            resource_group_name: "test-rg".into(),
        }
    }
}

/// Error returned by the Azure SDK-facing paths until the
/// `auth::build_azure_credential` shim lands. Keeps the message
/// uniform across `get`, `put`, and `cas_guard` so operators see the
/// same diagnostic regardless of which call they hit first.
fn azure_auth_not_wired(op: &str) -> CrabError {
    CrabError::Internal(format!(
        "Azure {op}: `auth::CredentialProvider` → `azure_core::TokenCredential` \
         adapter is not yet wired. Apply the lifecycle JSON out-of-band via \
         `az storage account management-policy create` or wait for the auth \
         shim to land. See tier/provider/azure.rs for the SDK wiring plan."
    ))
}

#[async_trait]
impl LifecycleProvider for AzureLifecycleProvider {
    fn kind(&self) -> Provider {
        Provider::Azure
    }

    fn render(&self, plan: &TierPlan) -> Result<RenderedLifecycle> {
        render(plan)
    }

    async fn get(&self) -> Result<Option<RenderedLifecycle>> {
        // The read path requires an authenticated `azure_core::Client`
        // with a `TokenCredential`. That shim is tracked under
        // `crab-storage-economy` and blocks on `auth::build_store`
        // growing an Azure adapter. Until then we surface a structured
        // error that names the missing piece — silently returning
        // `Ok(None)` (the previous stub shape) made operators believe
        // no policy was configured, which misleads the CAS loop in
        // `tier::apply` into performing an unconditional PUT.
        debug!(
            account = %self.storage_account,
            container = %self.container,
            subscription_id = %self.subscription_id,
            resource_group = %self.resource_group_name,
            "Azure get lifecycle: auth shim not wired"
        );
        Err(azure_auth_not_wired("get lifecycle"))
    }

    async fn put(&self, doc: &RenderedLifecycle, _guard: Option<Guard>) -> Result<PutOutcome> {
        debug!(
            account = %self.storage_account,
            container = %self.container,
            subscription_id = %self.subscription_id,
            resource_group = %self.resource_group_name,
            rules = ?doc.rule_ids,
            "Azure put lifecycle: auth shim not wired"
        );
        Err(azure_auth_not_wired("put lifecycle"))
    }

    async fn cas_guard(&self) -> Result<Option<Guard>> {
        debug!(
            account = %self.storage_account,
            container = %self.container,
            subscription_id = %self.subscription_id,
            resource_group = %self.resource_group_name,
            "Azure cas_guard: auth shim not wired"
        );
        Err(azure_auth_not_wired("cas_guard"))
    }
}

#[async_trait]
impl RestoreBackend for AzureLifecycleProvider {
    async fn restore(
        &self,
        path: &ObjectPath,
        tier: RestoreTier,
        _duration: Duration,
    ) -> Result<RestoreHandle> {
        // The Azure rehydration path uses `azure_storage_blobs`'
        // `BlobClient::set_blob_tier` with a `RehydratePriority` header.
        // That client needs an authenticated `StorageCredentials` which
        // is blocked on the same auth-shim integration as the management
        // policy path above. Mapping the intended tier is kept here so
        // when the shim lands only the client construction need change.
        let priority = match tier {
            RestoreTier::High => "High",
            _ => "Standard",
        };
        debug!(
            account = %self.storage_account,
            container = %self.container,
            key = %path,
            priority = %priority,
            "Azure restore: auth shim not wired"
        );
        Err(azure_auth_not_wired("restore"))
    }

    async fn state(&self, path: &ObjectPath) -> Result<RestoreState> {
        // Same story as `restore` — this needs an authenticated
        // `BlobClient::get_properties` call to read `AccessTier` and
        // `AccessTierChangeTime`. Until the auth shim lands we fail
        // loud rather than claim `NotRequested` on every archived blob
        // (which would trick the hydrate pipeline into skipping the
        // restore wait entirely).
        debug!(
            account = %self.storage_account,
            container = %self.container,
            key = %path,
            "Azure restore state: auth shim not wired"
        );
        Err(azure_auth_not_wired("restore state"))
    }

    fn supported_tiers(&self, class: &StorageClass) -> &'static [RestoreTier] {
        match class {
            // Azure Archive supports High (<1 h) and Standard (up to
            // 15 h). No Bulk or Expedited.
            StorageClass::AzureArchive => AZURE_ARCHIVE_TIERS,
            _ => NO_TIERS,
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────

/// Return the current time as an RFC 3339 string.
///
/// Currently unused — `put` errors before constructing an outcome.
/// Retained so the eventual real PUT path reuses it without
/// re-introducing the helper.
#[allow(dead_code, reason = "reused when put() is wired against the auth shim")]
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", duration.as_secs())
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

    fn cool_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::AzureCool,
        }
    }

    fn archive_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::AzureArchive,
        }
    }

    // ── Snapshot: basic Cool transition ──────────────────────────────

    #[test]
    fn snapshot_basic_cool_transition() {
        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-xorbs".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let json = render_json(&plan);
        insta::assert_snapshot!("azure_basic_cool_transition", json);
    }

    // ── Snapshot: multiple transitions (Cool + Archive) ─────────────

    #[test]
    fn snapshot_multiple_transitions() {
        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-xorbs".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30), archive_transition(180)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let json = render_json(&plan);
        insta::assert_snapshot!("azure_multiple_transitions", json);
    }

    // ── JSON validity ───────────────────────────────────────────────

    #[test]
    fn output_is_valid_json_with_expected_fields() {
        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-xorbs".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30)],
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
        let rules = parsed
            .get("rules")
            .expect("should have rules key")
            .as_array()
            .expect("rules should be an array");
        assert_eq!(rules.len(), 1);

        // Verify rule structure.
        let rule = &rules[0];
        assert_eq!(rule.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(rule.get("type").and_then(|v| v.as_str()), Some("Lifecycle"));
        assert!(rule.get("name").and_then(|v| v.as_str()).is_some());

        // Verify definition.
        let definition = rule.get("definition").expect("should have definition");
        let actions = definition.get("actions").expect("should have actions");
        let base_blob = actions.get("baseBlob").expect("should have baseBlob");
        let tier_to_cool = base_blob.get("tierToCool").expect("should have tierToCool");
        assert_eq!(
            tier_to_cool
                .get("daysAfterModificationGreaterThan")
                .and_then(|v| v.as_u64()),
            Some(30)
        );

        // Verify filters.
        let filters = definition.get("filters").expect("should have filters");
        let blob_types = filters
            .get("blobTypes")
            .and_then(|v| v.as_array())
            .expect("blobTypes should be an array");
        assert_eq!(blob_types.len(), 1);
        assert_eq!(blob_types[0].as_str(), Some("blockBlob"));

        let prefix_match = filters
            .get("prefixMatch")
            .and_then(|v| v.as_array())
            .expect("prefixMatch should be an array");
        assert_eq!(prefix_match.len(), 1);
        assert_eq!(prefix_match[0].as_str(), Some(".crab/xorbs/"));
    }

    // ── Rendered format is Json ─────────────────────────────────────

    #[test]
    fn rendered_format_is_json() {
        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30)],
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
            provider: Provider::Azure,
            rules: vec![
                TierRule {
                    id: "crab-z-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![cool_transition(30)],
                    noncurrent_expiration_days: None,
                    min_object_size_bytes: None,
                },
                TierRule {
                    id: "crab-a-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![archive_transition(180)],
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

    // ── Azure tier action key mapping ───────────────────────────────

    #[test]
    fn azure_tier_action_key_mapping() {
        assert_eq!(azure_tier_action_key(StorageClass::AzureCool), "tierToCool");
        assert_eq!(azure_tier_action_key(StorageClass::AzureCold), "tierToCold");
        assert_eq!(
            azure_tier_action_key(StorageClass::AzureArchive),
            "tierToArchive"
        );
    }

    #[test]
    fn azure_tier_action_key_non_azure_falls_back_to_cool() {
        assert_eq!(
            azure_tier_action_key(StorageClass::S3Standard),
            "tierToCool"
        );
        assert_eq!(
            azure_tier_action_key(StorageClass::GcsNearline),
            "tierToCool"
        );
        assert_eq!(azure_tier_action_key(StorageClass::Unknown), "tierToCool");
    }

    // ── AzureLifecycleProvider: kind ────────────────────────────────

    #[test]
    fn provider_kind_is_azure() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        assert_eq!(provider.kind(), Provider::Azure);
    }

    // ── AzureLifecycleProvider: accessors ───────────────────────────

    #[test]
    fn provider_accessors() {
        let provider =
            AzureLifecycleProvider::new_for_tests("myaccount".into(), "mycontainer".into());
        assert_eq!(provider.storage_account(), "myaccount");
        assert_eq!(provider.container(), "mycontainer");
    }

    // ── AzureLifecycleProvider: render delegates ────────────────────

    #[test]
    fn provider_render_delegates_to_module_render() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());

        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30)],
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

    // ── AzureLifecycleProvider: get surfaces auth-not-wired error ───
    //
    // Previously this test asserted `get` returned `Ok(None)` because
    // the stub pretended no policy existed. That behavior was a foot-
    // gun: the `tier::apply` CAS loop would then perform an
    // unconditional PUT. The new implementation fails loud with a
    // structured `Internal` error naming the missing adapter, so
    // deployments surface the configuration gap immediately.
    #[tokio::test]
    async fn provider_get_surfaces_auth_not_wired() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider.get().await.expect_err("get must fail loud");
        match err {
            CrabError::Internal(msg) => {
                assert!(msg.contains("Azure"), "message names Azure: {msg}");
                assert!(
                    msg.contains("auth::CredentialProvider"),
                    "message names the missing shim: {msg}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    // ── AzureLifecycleProvider: put surfaces auth-not-wired error ───
    #[tokio::test]
    async fn provider_put_surfaces_auth_not_wired() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());

        let plan = TierPlan {
            provider: Provider::Azure,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![cool_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        let err = provider
            .put(&rendered, None)
            .await
            .expect_err("put must fail loud");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // ── AzureLifecycleProvider: cas_guard surfaces auth-not-wired ───
    #[tokio::test]
    async fn provider_cas_guard_surfaces_auth_not_wired() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider
            .cas_guard()
            .await
            .expect_err("cas_guard must fail loud");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // ── RestoreBackend: supported_tiers matrix ──────────────────────

    #[test]
    fn supported_tiers_azure_archive_has_standard_and_high() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let tiers = provider.supported_tiers(&StorageClass::AzureArchive);
        assert_eq!(tiers.len(), 2);
        assert!(tiers.contains(&RestoreTier::Standard));
        assert!(tiers.contains(&RestoreTier::High));
        // No Bulk or Expedited for Azure.
        assert!(!tiers.contains(&RestoreTier::Bulk));
        assert!(!tiers.contains(&RestoreTier::Expedited));
    }

    #[test]
    fn supported_tiers_non_archive_azure_classes_empty() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        assert!(provider.supported_tiers(&StorageClass::AzureHot).is_empty());
        assert!(
            provider
                .supported_tiers(&StorageClass::AzureCool)
                .is_empty()
        );
        assert!(
            provider
                .supported_tiers(&StorageClass::AzureCold)
                .is_empty()
        );
    }

    // ── RestoreBackend: restore surfaces auth-not-wired error ───────
    #[tokio::test]
    async fn restore_surfaces_auth_not_wired() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider
            .restore(
                &"some/blob/path".to_string(),
                RestoreTier::High,
                Duration::from_secs(86_400 * 7),
            )
            .await
            .expect_err("restore must fail loud");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // ── RestoreBackend: state surfaces auth-not-wired error ─────────
    #[tokio::test]
    async fn restore_state_surfaces_auth_not_wired() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider
            .state(&"some/blob/path".to_string())
            .await
            .expect_err("state must fail loud");
        assert!(matches!(err, CrabError::Internal(_)));
    }
}
