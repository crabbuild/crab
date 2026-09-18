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

use std::sync::Arc;
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
#[expect(
    clippy::struct_field_names,
    reason = "Azure's management schema requires the tierTo* field names"
)]
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

/// Azure Blob lifecycle provider backed by the Azure management and blob
/// REST APIs through the Azure SDK credential contract.
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
/// The runtime constructor obtains a credential from `azure_identity`.
/// The infallible constructor remains useful for rendering and tests; any
/// remote operation on such a value fails closed with a configuration error.
pub struct AzureLifecycleProvider {
    storage_account: String,
    container: String,
    subscription_id: String,
    resource_group_name: String,
    credential: Option<Arc<dyn azure_core::auth::TokenCredential>>,
    http: reqwest::Client,
    management_endpoint: String,
    storage_endpoint: String,
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
            storage_endpoint: format!("https://{storage_account}.blob.core.windows.net"),
            storage_account,
            container,
            subscription_id,
            resource_group_name,
            credential: None,
            http: reqwest::Client::new(),
            management_endpoint: "https://management.azure.com".to_owned(),
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
        let credential =
            azure_identity::create_credential().map_err(|error| CrabError::Configuration {
                key: "azure.credentials".to_owned(),
                origin: format!("Azure authentication failed: {error}"),
            })?;
        let mut provider = Self::with_credential(
            storage_account,
            container,
            subscription_id,
            resource_group_name,
            credential,
        );
        if let Ok(endpoint) = std::env::var("AZURE_STORAGE_ENDPOINT")
            && !endpoint.trim().is_empty()
        {
            provider.storage_endpoint = endpoint;
        }
        if let Ok(endpoint) = std::env::var("AZURE_MANAGEMENT_ENDPOINT")
            && !endpoint.trim().is_empty()
        {
            provider.management_endpoint = endpoint;
        }
        Ok(provider)
    }

    /// Build an authenticated provider with an already configured Azure
    /// credential. This is the injection point for Crab's auth resolver and
    /// for provider integration tests.
    pub fn with_credential(
        storage_account: String,
        container: String,
        subscription_id: String,
        resource_group_name: String,
        credential: Arc<dyn azure_core::auth::TokenCredential>,
    ) -> Self {
        Self {
            storage_endpoint: format!("https://{storage_account}.blob.core.windows.net"),
            storage_account,
            container,
            subscription_id,
            resource_group_name,
            credential: Some(credential),
            http: reqwest::Client::new(),
            management_endpoint: "https://management.azure.com".to_owned(),
        }
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
            storage_endpoint: format!("https://{storage_account}.blob.core.windows.net"),
            storage_account,
            container,
            subscription_id: "00000000-0000-0000-0000-000000000000".into(),
            resource_group_name: "test-rg".into(),
            credential: None,
            http: reqwest::Client::new(),
            management_endpoint: "https://management.azure.com".to_owned(),
        }
    }
}

/// Return a structured error for an unauthenticated provider.
fn azure_missing_credential(op: &str) -> CrabError {
    CrabError::Internal(format!(
        "Azure {op}: no Azure TokenCredential is configured; construct the \
         provider with AzureLifecycleProvider::from_env or \
         AzureLifecycleProvider::with_credential"
    ))
}

impl AzureLifecycleProvider {
    fn credential(&self, operation: &str) -> Result<Arc<dyn azure_core::auth::TokenCredential>> {
        self.credential
            .clone()
            .ok_or_else(|| azure_missing_credential(operation))
    }

    fn management_url(&self) -> String {
        format!(
            "{}/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Storage/storageAccounts/{}/managementPolicies/default?api-version=2023-05-01",
            self.management_endpoint.trim_end_matches('/'),
            urlencoding::encode(&self.subscription_id),
            urlencoding::encode(&self.resource_group_name),
            urlencoding::encode(&self.storage_account),
        )
    }

    fn blob_url(&self, path: &ObjectPath) -> String {
        let mut url = self.storage_endpoint.trim_end_matches('/').to_owned();
        url.push('/');
        url.push_str(&urlencoding::encode(&self.container));
        for segment in path.trim_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            url.push('/');
            url.push_str(&urlencoding::encode(segment));
        }
        url
    }

    async fn management_token(&self, operation: &str) -> Result<String> {
        let credential = self.credential(operation)?;
        let token = credential
            .get_token(&["https://management.azure.com/.default"])
            .await
            .map_err(|error| {
                CrabError::Internal(format!("Azure {operation} authentication failed: {error}"))
            })?;
        Ok(format!("Bearer {}", token.token.secret()))
    }

    async fn storage_token(&self, operation: &str) -> Result<String> {
        let credential = self.credential(operation)?;
        let token = credential
            .get_token(&["https://storage.azure.com/.default"])
            .await
            .map_err(|error| {
                CrabError::Internal(format!("Azure {operation} authentication failed: {error}"))
            })?;
        Ok(format!("Bearer {}", token.token.secret()))
    }
}

fn azure_rest_status_error(operation: &str, status: reqwest::StatusCode, body: &[u8]) -> CrabError {
    let detail = String::from_utf8_lossy(body);
    CrabError::Internal(format!(
        "Azure {operation} failed with HTTP {status}: {detail}"
    ))
}

fn is_azure_not_found(error: &azure_core::Error) -> bool {
    matches!(
        error.kind(),
        azure_core::error::ErrorKind::HttpResponse { status, .. }
            if *status == azure_core::StatusCode::NotFound
    )
}

fn azure_blob_error(operation: &str, path: &ObjectPath, error: azure_core::Error) -> CrabError {
    if is_azure_not_found(&error) {
        CrabError::NotFound { path: path.clone() }
    } else {
        CrabError::Internal(format!("Azure {operation} for {path} failed: {error}"))
    }
}

fn azure_timestamp(value: Option<&str>, path: &ObjectPath) -> Result<String> {
    let Some(value) = value else {
        return Ok(String::new());
    };
    let parsed = azure_core::date::parse_rfc1123(value)
        .or_else(|_| azure_core::date::parse_rfc3339(value))
        .map_err(|error| CrabError::CorruptObject {
            path: path.clone(),
            reason: format!("Azure access-tier-change-time is invalid: {error}"),
        })?;
    Ok(azure_core::date::to_rfc3339(&parsed))
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
        let response = self
            .http
            .get(self.management_url())
            .header(
                reqwest::header::AUTHORIZATION,
                self.management_token("get lifecycle").await?,
            )
            .send()
            .await
            .map_err(|error| {
                CrabError::Internal(format!("Azure get lifecycle request failed: {error}"))
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("Azure get lifecycle response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(azure_rest_status_error("get lifecycle", status, &body));
        }

        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
                path: format!("az://{}/lifecycle", self.storage_account),
                reason: format!("Azure lifecycle response is not valid JSON: {error}"),
            })?;
        let rules = value
            .get("properties")
            .and_then(|properties| properties.get("policy"))
            .and_then(|policy| policy.get("rules"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| CrabError::CorruptObject {
                path: format!("az://{}/lifecycle", self.storage_account),
                reason: "Azure lifecycle response has no properties.policy.rules array".to_owned(),
            })?;
        if rules.is_empty() {
            return Ok(None);
        }
        let mut rule_ids = Vec::with_capacity(rules.len());
        for rule in rules {
            let name = rule
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| CrabError::CorruptObject {
                    path: format!("az://{}/lifecycle", self.storage_account),
                    reason: "Azure lifecycle rule has no non-empty name".to_owned(),
                })?;
            rule_ids.push(name.to_owned());
        }
        let body =
            serde_json::to_vec_pretty(&serde_json::json!({ "rules": rules })).map_err(|error| {
                CrabError::Internal(format!(
                    "Azure lifecycle response serialize failed: {error}"
                ))
            })?;
        Ok(Some(RenderedLifecycle {
            format: Format::Json,
            body,
            rule_ids,
        }))
    }

    async fn put(&self, doc: &RenderedLifecycle, guard: Option<Guard>) -> Result<PutOutcome> {
        if doc.format != Format::Json {
            return Err(CrabError::IncompatibleFormat {
                required: "Azure lifecycle JSON".to_owned(),
                found: format!("{:?}", doc.format),
            });
        }
        let value: serde_json::Value =
            serde_json::from_slice(&doc.body).map_err(|error| CrabError::Configuration {
                key: "tier.azure.lifecycle".to_owned(),
                origin: format!("rendered lifecycle is not valid JSON: {error}"),
            })?;
        let rules = value
            .get("rules")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| CrabError::Configuration {
                key: "tier.azure.lifecycle".to_owned(),
                origin: "rendered lifecycle is missing the rules array".to_owned(),
            })?;
        let payload = serde_json::json!({
            "properties": {
                "policy": {
                    "rules": rules,
                }
            }
        });
        let expected_etag = match guard {
            None => None,
            Some(Guard::Etag(etag)) => Some(etag),
            Some(Guard::Generation(_) | Guard::None) => {
                return Err(CrabError::Configuration {
                    key: "tier.azure.lifecycle.guard".to_owned(),
                    origin: "Azure lifecycle writes require an ETag guard".to_owned(),
                });
            }
        };
        let mut request = self
            .http
            .put(self.management_url())
            .header(
                reqwest::header::AUTHORIZATION,
                self.management_token("put lifecycle").await?,
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload);
        if let Some(etag) = expected_etag.as_deref() {
            request = request.header(reqwest::header::IF_MATCH, etag);
        } else {
            request = request.header(reqwest::header::IF_NONE_MATCH, "*");
        }
        let response = request.send().await.map_err(|error| {
            CrabError::Internal(format!("Azure put lifecycle request failed: {error}"))
        })?;
        let status = response.status();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .map(|value| {
                value.to_str().map(str::to_owned).map_err(|error| {
                    CrabError::Internal(format!(
                        "Azure put lifecycle returned invalid ETag: {error}"
                    ))
                })
            })
            .transpose()?;
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("Azure put lifecycle response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(CrabError::CasConflict {
                path: format!("az://{}/lifecycle", self.storage_account),
                expected_etag,
            });
        }
        if !status.is_success() {
            return Err(azure_rest_status_error("put lifecycle", status, &body));
        }
        let etag = etag.ok_or_else(|| CrabError::CorruptObject {
            path: format!("az://{}/lifecycle", self.storage_account),
            reason: "Azure lifecycle PUT response has no ETag".to_owned(),
        })?;
        debug!(
            account = %self.storage_account,
            rules = ?doc.rule_ids,
            "Azure lifecycle applied"
        );
        Ok(PutOutcome {
            new_guard: Guard::Etag(etag),
            applied_at: now_rfc3339()?,
        })
    }

    async fn delete(&self, guard: Option<Guard>) -> Result<PutOutcome> {
        let etag = match guard {
            Some(Guard::Etag(etag)) => etag,
            None => {
                return Err(CrabError::TierProviderUnsupported {
                    provider: "Azure lifecycle deletion requires an ETag guard".to_owned(),
                });
            }
            Some(Guard::Generation(_) | Guard::None) => {
                return Err(CrabError::Configuration {
                    key: "tier.azure.lifecycle.guard".to_owned(),
                    origin: "Azure lifecycle deletion requires an ETag guard".to_owned(),
                });
            }
        };
        let response = self
            .http
            .delete(self.management_url())
            .header(
                reqwest::header::AUTHORIZATION,
                self.management_token("delete lifecycle").await?,
            )
            .header(reqwest::header::IF_MATCH, etag.clone())
            .send()
            .await
            .map_err(|error| {
                CrabError::Internal(format!("Azure delete lifecycle request failed: {error}"))
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("Azure delete lifecycle response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(CrabError::CasConflict {
                path: format!("az://{}/lifecycle", self.storage_account),
                expected_etag: Some(etag),
            });
        }
        if status != reqwest::StatusCode::NOT_FOUND && !status.is_success() {
            return Err(azure_rest_status_error("delete lifecycle", status, &body));
        }
        Ok(PutOutcome {
            new_guard: Guard::None,
            applied_at: now_rfc3339()?,
        })
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
                path: format!("az://{}/lifecycle", self.storage_account),
                reason: format!("current lifecycle is not valid JSON: {error}"),
            })?;
        let intended: serde_json::Value =
            serde_json::from_slice(&intended.body).map_err(|error| CrabError::Configuration {
                key: "tier.azure.lifecycle".to_owned(),
                origin: format!("intended lifecycle is not valid JSON: {error}"),
            })?;
        Ok(current == intended)
    }

    async fn cas_guard(&self) -> Result<Option<Guard>> {
        let response = self
            .http
            .get(self.management_url())
            .header(
                reqwest::header::AUTHORIZATION,
                self.management_token("cas_guard").await?,
            )
            .send()
            .await
            .map_err(|error| {
                CrabError::Internal(format!("Azure cas_guard request failed: {error}"))
            })?;
        let status = response.status();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .map(|value| {
                value.to_str().map(str::to_owned).map_err(|error| {
                    CrabError::Internal(format!("Azure cas_guard returned invalid ETag: {error}"))
                })
            })
            .transpose()?;
        let body = response.bytes().await.map_err(|error| {
            CrabError::Internal(format!("Azure cas_guard response failed: {error}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(azure_rest_status_error("cas_guard", status, &body));
        }
        let etag = etag.ok_or_else(|| CrabError::CorruptObject {
            path: format!("az://{}/lifecycle", self.storage_account),
            reason: "Azure lifecycle GET response has no ETag".to_owned(),
        })?;
        Ok(Some(Guard::Etag(etag)))
    }
}

#[async_trait]
impl RestoreBackend for AzureLifecycleProvider {
    async fn restore(
        &self,
        path: &ObjectPath,
        tier: RestoreTier,
        duration: Duration,
    ) -> Result<RestoreHandle> {
        // Azure rehydration is a Set Blob Tier request with an explicit
        // priority. The SDK obtains and refreshes the storage bearer token
        // through the same credential used by the management API.
        let priority = match tier {
            RestoreTier::High => azure_storage_blobs::prelude::RehydratePriority::High,
            RestoreTier::Standard => azure_storage_blobs::prelude::RehydratePriority::Standard,
            RestoreTier::Bulk | RestoreTier::Expedited => {
                return Err(CrabError::TierProviderUnsupported {
                    provider: format!("Azure restore tier {tier:?}"),
                });
            }
        };
        let credential = self.credential("restore")?;
        let credentials = azure_storage::StorageCredentials::token_credential(credential);
        let client = azure_storage_blobs::prelude::ClientBuilder::with_location(
            azure_storage::CloudLocation::Custom {
                account: self.storage_account.clone(),
                uri: self.storage_endpoint.clone(),
            },
            credentials,
        )
        .blob_client(self.container.clone(), path.clone());
        client
            .set_blob_tier(azure_storage_blobs::prelude::AccessTier::Hot)
            .rehydrate_priority(priority)
            .await
            .map_err(|error| azure_blob_error("restore", path, error))?;
        debug!(
            account = %self.storage_account,
            container = %self.container,
            key = %path,
            priority = ?tier,
            duration_secs = duration.as_secs(),
            "Azure restore request submitted"
        );
        Ok(RestoreHandle {
            id: format!("azure-restore-{path}"),
        })
    }

    async fn state(&self, path: &ObjectPath) -> Result<RestoreState> {
        // Use a raw HEAD here because the pinned Blob SDK does not expose
        // rehydrate-priority or access-tier-change-time response headers.
        // Treat malformed tier metadata as corruption rather than skipping
        // a required restore.
        let response = self
            .http
            .head(self.blob_url(path))
            .header(
                reqwest::header::AUTHORIZATION,
                self.storage_token("restore state").await?,
            )
            .header("x-ms-version", "2023-11-03")
            .send()
            .await
            .map_err(|error| {
                CrabError::Internal(format!(
                    "Azure restore state request failed for {path}: {error}"
                ))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CrabError::NotFound { path: path.clone() });
        }
        if !status.is_success() {
            let body = response.bytes().await.map_err(|error| {
                CrabError::Internal(format!(
                    "Azure restore state response failed for {path}: {error}"
                ))
            })?;
            return Err(azure_rest_status_error("restore state", status, &body));
        }

        let access_tier = response
            .headers()
            .get("x-ms-access-tier")
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|error| CrabError::CorruptObject {
                        path: path.clone(),
                        reason: format!("Azure access tier header is invalid: {error}"),
                    })
            })
            .transpose()?;
        let Some(access_tier) = access_tier else {
            // The service can omit this header for an account's implicit
            // default tier; those blobs are readable without rehydration.
            return Ok(RestoreState::Ready);
        };
        if access_tier != "Archive" {
            if !matches!(access_tier.as_str(), "Hot" | "Cool" | "Cold") {
                return Err(CrabError::CorruptObject {
                    path: path.clone(),
                    reason: format!("Azure returned unknown access tier {access_tier}"),
                });
            }
            return Ok(RestoreState::Ready);
        }

        let priority = response
            .headers()
            .get("x-ms-rehydrate-priority")
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|error| CrabError::CorruptObject {
                        path: path.clone(),
                        reason: format!("Azure rehydrate-priority header is invalid: {error}"),
                    })
            })
            .transpose()?;
        if priority.is_none() {
            return Ok(RestoreState::NotRequested);
        }
        if !matches!(priority.as_deref(), Some("High" | "Standard")) {
            return Err(CrabError::CorruptObject {
                path: path.clone(),
                reason: format!("Azure returned unknown rehydrate priority {priority:?}"),
            });
        }
        let change_time = response
            .headers()
            .get("x-ms-access-tier-change-time")
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|error| CrabError::CorruptObject {
                        path: path.clone(),
                        reason: format!("Azure access-tier-change-time header is invalid: {error}"),
                    })
            })
            .transpose()?;
        let started_at = azure_timestamp(change_time.as_deref(), path)?;
        Ok(RestoreState::InProgress {
            started_at,
            expected_ready_at: String::new(),
        })
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
fn now_rfc3339() -> Result<String> {
    crab_types::time::now_rfc3339_millis()
        .map_err(|error| CrabError::Internal(format!("Azure lifecycle timestamp failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::provider::{Provider, TierPlan, TierRule, Transition};
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct TestCredential;

    #[async_trait::async_trait]
    impl azure_core::auth::TokenCredential for TestCredential {
        async fn get_token(
            &self,
            _scopes: &[&str],
        ) -> azure_core::Result<azure_core::auth::AccessToken> {
            let expires_on =
                azure_core::date::parse_rfc3339("2099-01-01T00:00:00Z").map_err(|error| {
                    azure_core::Error::with_message(
                        azure_core::error::ErrorKind::DataConversion,
                        || format!("test token timestamp: {error}"),
                    )
                })?;
            Ok(azure_core::auth::AccessToken::new("test-token", expires_on))
        }

        async fn clear_cache(&self) -> azure_core::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct ArmRequests(Arc<Mutex<Vec<(String, String, String, String, Vec<u8>)>>>);

    async fn arm_handler(
        State(requests): State<ArmRequests>,
        request: Request<Body>,
    ) -> (StatusCode, [(String, String); 1], Body) {
        let method = request.method().to_string();
        let uri = request.uri().to_string();
        let authorization = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let if_match = request
            .headers()
            .get(reqwest::header::IF_MATCH)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = to_bytes(request.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap_or_default()
            .to_vec();
        requests
            .0
            .lock()
            .unwrap()
            .push((method.clone(), uri, authorization, if_match, body));

        match method.as_str() {
            "GET" => (
                StatusCode::OK,
                [("etag".to_owned(), "\"v1\"".to_owned())],
                Body::from(
                    r#"{"properties":{"policy":{"rules":[{"enabled":true,"name":"user-cleanup","type":"Lifecycle","definition":{}}]}}}"#,
                ),
            ),
            "PUT" => (
                StatusCode::OK,
                [("etag".to_owned(), "\"v2\"".to_owned())],
                Body::from("{}"),
            ),
            "DELETE" => (
                StatusCode::NO_CONTENT,
                [("etag".to_owned(), "\"v3\"".to_owned())],
                Body::empty(),
            ),
            _ => (
                StatusCode::METHOD_NOT_ALLOWED,
                [("etag".to_owned(), "\"v1\"".to_owned())],
                Body::empty(),
            ),
        }
    }

    async fn test_arm_server() -> (String, ArmRequests, tokio::task::JoinHandle<()>) {
        let requests = ArmRequests::default();
        let app = Router::new()
            .fallback(arm_handler)
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), requests, task)
    }

    fn authenticated_test_provider(endpoint: &str) -> AzureLifecycleProvider {
        let mut provider = AzureLifecycleProvider::with_credential(
            "account/name".into(),
            "container".into(),
            "subscription/id".into(),
            "resource group".into(),
            Arc::new(TestCredential),
        );
        provider.management_endpoint = endpoint.to_owned();
        provider
    }

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

    #[tokio::test]
    async fn authenticated_lifecycle_reads_arm_policy_and_etag() {
        let (endpoint, requests, server) = test_arm_server().await;
        let provider = authenticated_test_provider(&endpoint);

        let current = provider.get().await.unwrap().unwrap();
        assert_eq!(current.rule_ids, vec!["user-cleanup"]);
        assert_eq!(
            provider.cas_guard().await.unwrap(),
            Some(Guard::Etag("\"v1\"".into()))
        );

        let recorded = requests.0.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].0 == "GET" && recorded[0].2 == "Bearer test-token");
        assert!(recorded[0].1.contains("managementPolicies/default"));
        assert!(recorded[0].1.contains("subscription%2Fid"));
        drop(recorded);
        server.abort();
    }

    #[tokio::test]
    async fn authenticated_lifecycle_put_uses_if_match_and_arm_payload() {
        let (endpoint, requests, server) = test_arm_server().await;
        let provider = authenticated_test_provider(&endpoint);
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
        let rendered = provider.render(&plan).unwrap();

        let outcome = provider
            .put(&rendered, Some(Guard::Etag("\"v1\"".into())))
            .await
            .unwrap();
        assert_eq!(outcome.new_guard, Guard::Etag("\"v2\"".into()));

        let recorded = requests.0.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let payload: serde_json::Value = serde_json::from_slice(&recorded[0].4).unwrap();
        assert_eq!(payload["properties"]["policy"]["rules"][0]["enabled"], true);
        assert_eq!(recorded[0].2, "Bearer test-token");
        assert_eq!(recorded[0].3, "\"v1\"");
        drop(recorded);
        server.abort();
    }

    // ── AzureLifecycleProvider: get requires a credential ───────────
    //
    // Previously this test asserted `get` returned `Ok(None)` because
    // the stub pretended no policy existed. That behavior was a foot-
    // gun: the `tier::apply` CAS loop would then perform an
    // unconditional PUT. The new implementation fails loud with a
    // structured `Internal` error naming the missing credential, so
    // deployments surface the configuration gap immediately.
    #[tokio::test]
    async fn provider_get_requires_credential() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider.get().await.expect_err("get must fail loud");
        match err {
            CrabError::Internal(msg) => {
                assert!(msg.contains("Azure"), "message names Azure: {msg}");
                assert!(
                    msg.contains("TokenCredential"),
                    "message names the missing credential: {msg}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    // ── AzureLifecycleProvider: put requires a credential ───────────
    #[tokio::test]
    async fn provider_put_requires_credential() {
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

    // ── AzureLifecycleProvider: cas_guard requires a credential ─────
    #[tokio::test]
    async fn provider_cas_guard_requires_credential() {
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

    // ── RestoreBackend: restore requires a credential ───────────────
    #[tokio::test]
    async fn restore_requires_credential() {
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

    // ── RestoreBackend: state requires a credential ─────────────────
    #[tokio::test]
    async fn restore_state_requires_credential() {
        let provider =
            AzureLifecycleProvider::new_for_tests("testaccount".into(), "testcontainer".into());
        let err = provider
            .state(&"some/blob/path".to_string())
            .await
            .expect_err("state must fail loud");
        assert!(matches!(err, CrabError::Internal(_)));
    }
}
