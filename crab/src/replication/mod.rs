//! Repository replication configuration, planning, and read readiness.
//!
//! V1 keeps writes pinned to the primary remote. Replicas are read targets
//! only, selected after their manifest generation and referenced immutable
//! objects are known to be present.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
#[cfg(feature = "replication-azure-control-plane")]
use azure_mgmt_storage::package_2023_05::models as azure_models;
use object_store::path::Path as ObjectPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crab_read::{
    ReadReplicaCandidate, ReadReplicaFallback, ReadReplicaProbeResult, ReadStoreChoice,
    ReadStoreTarget, check_read_replica_readiness, select_read_store_choice,
};
pub use crab_read::{ReadRoutingPolicy, ReadSource, ReadinessCheckOptions};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::git::url::{Cloud, CrabUrl, ObjectUrl, UrlForm};
use crate::metadata::manifest::{
    Manifest, materialize_active_active_manifest_projection as materialize_manifest_projection,
    read_manifest,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_coordination::active_active as coordination_active_active;
use crab_coordination::write_coordinator::{
    CommitRequest, CoordinatedRefUpdate, CoordinatorCheckState, CoordinatorControlPlaneStatus,
    CoordinatorFenceOutcome, CoordinatorHealth, CoordinatorRepairSnapshot,
    ManagedCoordinatorProvider, WriteCoordinator, validate_coordinator_write_admission,
};
use crab_metadata::ref_registry::{ActiveActiveCoordinatorRegistration, RefRegistry};
pub use crab_types::replication::{
    ReplicaConfig, ReplicationConfig, ReplicationCoordinatorConfig,
    ReplicationCoordinatorConsistency, ReplicationCoordinatorKind, ReplicationMode,
    ReplicationProviderKind, ReplicationRpo, WriterConfig,
};
#[cfg(test)]
use crab_xet::xorb::format::MerkleHash;

const READINESS_CACHE_VERSION: u32 = 2;
const READINESS_CACHE_INVALIDATION_VERSION: u32 = 1;
const READ_EVENT_VERSION: u32 = 1;
const READ_EVENT_LOG_MAX_BYTES: u64 = 1_048_576;
const READ_ROUTING_POLICY_ENV: &str = "CRAB_REPLICA_READ_POLICY";
const READINESS_CACHE_TTL_MS_ENV: &str = "CRAB_REPLICA_READINESS_CACHE_TTL_MS";
const READINESS_NO_CACHE_ENV: &str = "CRAB_REPLICA_READINESS_NO_CACHE";

/// Provider setup plan returned by `crab replica add --dry-run` and by
/// non-dry-run commands before they write Crab config.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationSetupPlan {
    pub provider: ReplicationProviderKind,
    pub primary: String,
    pub replica: String,
    pub region: String,
    pub rpo: ReplicationRpo,
    pub backfill: bool,
    pub actions: Vec<ReplicationAction>,
}

/// One cloud-side setup action Crab expects for a provider.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationAction {
    pub description: String,
    pub required: bool,
    pub automated: bool,
}

/// Cloud control-plane resources Crab owns for one replication setup.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationOwnership {
    pub owner: String,
    pub replica_name: String,
    pub primary: String,
    pub replica: String,
}

/// One provider management API operation needed to apply replication.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ControlPlaneRequest {
    pub provider: ReplicationProviderKind,
    pub action: String,
    pub target: String,
    pub request: serde_json::Value,
    pub reversible: bool,
    pub managed_resource_id: String,
}

/// Provider apply/export plan for `crab replica`.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicationControlPlanePlan {
    pub setup: ReplicationSetupPlan,
    pub ownership: ReplicationOwnership,
    pub requests: Vec<ControlPlaneRequest>,
}

/// Cloud control-plane apply/remove result.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ControlPlaneApplyStatus {
    pub provider: ReplicationProviderKind,
    pub applied: bool,
    pub checked_drift: bool,
    pub actions: Vec<String>,
    pub message: String,
}

/// State of one provider control-plane check.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ControlPlaneCheckState {
    Verified,
    Missing,
    Drifted,
    Unknown,
    Unsupported,
}

impl ControlPlaneCheckState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Missing => "missing",
            Self::Drifted => "drifted",
            Self::Unknown => "unknown",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One provider control-plane check for a replica.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ControlPlaneCheck {
    pub provider: ReplicationProviderKind,
    pub code: String,
    pub state: ControlPlaneCheckState,
    pub action: String,
    pub target: String,
    pub managed_resource_id: String,
    pub message: String,
    pub remediation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<u8>,
}

/// Provider control-plane status for one configured replica.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ControlPlaneStatus {
    pub provider: ReplicationProviderKind,
    pub replica_name: String,
    pub primary: String,
    pub replica: String,
    pub backend_available: bool,
    pub checked_drift: bool,
    pub checks: Vec<ControlPlaneCheck>,
}

/// Provider management backend for Crab-owned replication resources.
#[async_trait]
pub trait ReplicationControlPlaneBackend: Send + Sync {
    fn provider(&self) -> ReplicationProviderKind;
    async fn apply(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus>;
    async fn status(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneStatus>;
    async fn remove(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3ReplicationRoleSpec {
    pub role_name: String,
    pub policy_name: String,
    pub replica_name: String,
    pub source_bucket: String,
    pub destination_bucket: String,
    pub prefix_scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3ReplicationRoleState {
    pub role_arn: String,
    pub crab_managed: bool,
    pub trust_policy_matches: bool,
    pub policy_matches: bool,
    pub source_bucket: String,
    pub destination_bucket: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3ReplicationRuleSpec {
    pub rule_id: String,
    pub replica_name: String,
    pub source_bucket: String,
    pub destination_bucket: String,
    pub destination_region: String,
    pub role_arn: String,
    pub prefix_scope: Vec<String>,
    pub rtc_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3ReplicationRuleState {
    pub crab_managed: bool,
    pub enabled: bool,
    pub destination_bucket: String,
    pub destination_region: String,
    pub role_arn: String,
    pub rtc_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3BatchReplicationSpec {
    pub job_id: String,
    pub replica_name: String,
    pub source_bucket: String,
    pub destination_bucket: String,
    pub role_arn: String,
    pub prefix_scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3BatchReplicationState {
    pub job_id: String,
    pub crab_managed: bool,
    pub destination_bucket: String,
    pub status: String,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct S3PolicyValidationSpec {
    pub action: String,
    pub replica_name: String,
    pub source_bucket: String,
    pub destination_bucket: String,
    pub role_name: String,
    pub policy_name: String,
}

#[async_trait]
pub(crate) trait S3ReplicationControlPlaneClient: Send + Sync {
    async fn bucket_versioning_enabled(&self, bucket: &str) -> Result<bool>;
    async fn enable_bucket_versioning(&self, bucket: &str) -> Result<()>;
    async fn replication_role(
        &self,
        spec: &S3ReplicationRoleSpec,
    ) -> Result<Option<S3ReplicationRoleState>>;
    async fn create_replication_role(
        &self,
        spec: &S3ReplicationRoleSpec,
    ) -> Result<S3ReplicationRoleState>;
    async fn delete_replication_role(&self, spec: &S3ReplicationRoleSpec) -> Result<()>;
    async fn replication_rule(
        &self,
        spec: &S3ReplicationRuleSpec,
    ) -> Result<Option<S3ReplicationRuleState>>;
    async fn put_replication_rule(&self, spec: &S3ReplicationRuleSpec) -> Result<()>;
    async fn remove_replication_rule(&self, source_bucket: &str, rule_id: &str) -> Result<()>;
    async fn batch_replication_job(
        &self,
        spec: &S3BatchReplicationSpec,
    ) -> Result<Option<S3BatchReplicationState>>;
    async fn create_batch_replication_job(&self, spec: &S3BatchReplicationSpec) -> Result<()>;
    async fn validate_policy(
        &self,
        spec: &S3PolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcsBucketReplicationState {
    pub bucket: String,
    pub metageneration: i64,
    pub location_type: String,
    pub rpo: Option<String>,
    pub public_access_prevention_enforced: bool,
    pub requester_pays: bool,
    pub has_cmek: bool,
    pub has_retention_policy: bool,
    pub has_delete_lifecycle_rule: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcsStorageTransferBackfillState {
    pub job_id: String,
    pub crab_managed: bool,
    pub destination_bucket: String,
    pub status: String,
    pub complete: bool,
    pub operation_name: Option<String>,
    pub objects_found: Option<u64>,
    pub objects_copied: Option<u64>,
    pub objects_skipped: Option<u64>,
    pub objects_failed: Option<u64>,
    pub bytes_found: Option<u64>,
    pub bytes_copied: Option<u64>,
    pub bytes_skipped: Option<u64>,
    pub bytes_failed: Option<u64>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcsPolicyValidationSpec {
    pub action: String,
    pub source_bucket: String,
    pub destination_bucket: String,
}

#[async_trait]
pub(crate) trait GcsReplicationControlPlaneClient: Send + Sync {
    async fn bucket_state(&self, bucket: &str) -> Result<Option<GcsBucketReplicationState>>;
    async fn set_bucket_rpo(
        &self,
        bucket: &str,
        rpo: &str,
        if_metageneration_match: i64,
    ) -> Result<()>;
    async fn backfill_job(
        &self,
        spec: &GcsStorageTransferBackfillSpec,
    ) -> Result<Option<GcsStorageTransferBackfillState>>;
    async fn create_backfill_job(&self, spec: &GcsStorageTransferBackfillSpec) -> Result<()>;
    async fn validate_policy(
        &self,
        spec: &GcsPolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcsStorageTransferBackfillSpec {
    pub job_id: String,
    pub source_bucket: String,
    pub destination_bucket: String,
    pub prefix_scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzureBlobServiceState {
    pub account: String,
    pub change_feed_enabled: bool,
    pub versioning_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzureObjectReplicationPolicySpec {
    pub policy_id: String,
    pub replica_name: String,
    pub source_account: String,
    pub source_container: String,
    pub destination_account: String,
    pub destination_container: String,
    pub destination_region: String,
    pub prefix_scope: Vec<String>,
    pub priority: bool,
    pub existing_blob_replication: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzureObjectReplicationPolicyState {
    pub policy_id: String,
    pub crab_managed: bool,
    pub enabled: bool,
    pub source_account: String,
    pub source_container: String,
    pub destination_account: String,
    pub destination_container: String,
    pub destination_region: String,
    pub prefix_scope: Vec<String>,
    pub priority: bool,
    pub existing_blob_replication: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzureExistingBlobBackfillSpec {
    pub job_id: String,
    pub source_account: String,
    pub source_container: String,
    pub destination_account: String,
    pub destination_container: String,
    pub prefix_scope: Vec<String>,
    pub destination_prefix_scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzureExistingBlobBackfillState {
    pub job_id: String,
    pub crab_managed: bool,
    pub destination_account: String,
    pub destination_container: String,
    pub status: String,
    pub complete: bool,
    pub objects_checked: u64,
    pub missing_objects: u64,
    pub first_missing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AzurePolicyValidationSpec {
    pub action: String,
    pub source_account: String,
    pub source_container: String,
    pub destination_account: String,
    pub destination_container: String,
    pub prefix_scope: Vec<String>,
    pub destination_prefix_scope: Vec<String>,
}

#[async_trait]
pub(crate) trait AzureReplicationControlPlaneClient: Send + Sync {
    async fn blob_service_state(&self, account: &str) -> Result<Option<AzureBlobServiceState>>;
    async fn set_change_feed(&self, account: &str, enabled: bool) -> Result<()>;
    async fn set_blob_versioning(&self, account: &str, enabled: bool) -> Result<()>;
    async fn object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<Option<AzureObjectReplicationPolicyState>>;
    async fn put_object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()>;
    async fn remove_object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()>;
    async fn existing_blob_backfill(
        &self,
        spec: &AzureExistingBlobBackfillSpec,
    ) -> Result<Option<AzureExistingBlobBackfillState>>;
    async fn validate_policy(
        &self,
        spec: &AzurePolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState>;
}

pub(crate) struct GcsReplicationControlPlaneBackend<C> {
    client: C,
}

impl<C> GcsReplicationControlPlaneBackend<C> {
    pub(crate) fn new(client: C) -> Self {
        Self { client }
    }
}

pub(crate) struct AzureReplicationControlPlaneBackend<C> {
    client: C,
}

impl<C> AzureReplicationControlPlaneBackend<C> {
    pub(crate) fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C> ReplicationControlPlaneBackend for AzureReplicationControlPlaneBackend<C>
where
    C: AzureReplicationControlPlaneClient,
{
    fn provider(&self) -> ReplicationProviderKind {
        ReplicationProviderKind::Azure
    }

    async fn apply(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        let mut actions = Vec::new();
        for request in &plan.requests {
            match base_control_plane_action(&request.action) {
                "enable-change-feed" => {
                    let account = azure_account_from_url(&request.target)?;
                    let state = self.azure_blob_service_state(&account).await?;
                    if !state.change_feed_enabled {
                        self.client.set_change_feed(&account, true).await?;
                    }
                    actions.push(request.action.clone());
                }
                "enable-blob-versioning" => {
                    let account = azure_account_from_url(&request.target)?;
                    let state = self.azure_blob_service_state(&account).await?;
                    if !state.versioning_enabled {
                        self.client.set_blob_versioning(&account, true).await?;
                    }
                    actions.push(request.action.clone());
                }
                "put-object-replication-policy" => {
                    let spec = azure_object_replication_policy_spec(plan)?;
                    match self.client.object_replication_policy(&spec).await? {
                        Some(state) if azure_object_policy_state_matches(&spec, &state) => {}
                        Some(state)
                            if state.crab_managed
                                && state.destination_account == spec.destination_account =>
                        {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.azure.policy".into(),
                                origin: format!(
                                    "Azure object replication policy {} is Crab-managed but drifted for replica {}",
                                    state.policy_id, plan.ownership.replica_name
                                ),
                            });
                        }
                        Some(state) => {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.azure.policy".into(),
                                origin: format!(
                                    "Azure object replication policy {} exists but is not Crab-managed for this destination",
                                    state.policy_id
                                ),
                            });
                        }
                        None => self.client.put_object_replication_policy(&spec).await?,
                    }
                    actions.push(request.action.clone());
                }
                "track-existing-blob-backfill" => {
                    let spec = azure_existing_blob_backfill_spec(plan)?;
                    match self.client.existing_blob_backfill(&spec).await? {
                        Some(state) if azure_backfill_state_matches(&spec, &state) => {}
                        Some(state)
                            if state.crab_managed
                                && state.destination_account == spec.destination_account
                                && state.destination_container == spec.destination_container => {}
                        Some(state) => {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.azure.backfill".into(),
                                origin: format!(
                                    "Azure existing-blob replication {} exists but is not Crab-managed for this destination",
                                    state.job_id
                                ),
                            });
                        }
                        None => {}
                    }
                    actions.push(request.action.clone());
                }
                action if action.starts_with("validate-") => {
                    let spec = azure_policy_validation_spec(plan, action)?;
                    let state = self.client.validate_policy(&spec).await?;
                    ensure_azure_verified(request, state)?;
                    actions.push(request.action.clone());
                }
                action => {
                    return Err(CrabError::Configuration {
                        key: "replication.control_plane.azure".into(),
                        origin: format!("unsupported Azure control-plane action {action}"),
                    });
                }
            }
        }

        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::Azure,
            applied: true,
            checked_drift: true,
            actions,
            message: format!(
                "applied Crab-managed Azure replication settings for replica {}",
                plan.ownership.replica_name
            ),
        })
    }

    async fn status(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneStatus> {
        let mut checks = Vec::new();
        for request in &plan.requests {
            let check = match base_control_plane_action(&request.action) {
                "enable-change-feed" => self.azure_change_feed_check(request).await?,
                "enable-blob-versioning" => self.azure_versioning_check(request).await?,
                "put-object-replication-policy" => {
                    self.azure_object_policy_check(plan, request).await?
                }
                "track-existing-blob-backfill" => self.azure_backfill_check(plan, request).await?,
                action if action.starts_with("validate-") => {
                    self.azure_policy_validation_check(plan, request, action)
                        .await?
                }
                action => control_plane_check(
                    request,
                    ControlPlaneCheckState::Unsupported,
                    format!("unsupported Azure control-plane action {action}"),
                    "upgrade Crab or remove the unsupported provider action from this plan",
                ),
            };
            checks.push(check);
        }

        Ok(ControlPlaneStatus {
            provider: ReplicationProviderKind::Azure,
            replica_name: plan.ownership.replica_name.clone(),
            primary: plan.ownership.primary.clone(),
            replica: plan.ownership.replica.clone(),
            backend_available: true,
            checked_drift: true,
            checks,
        })
    }

    async fn remove(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        let mut actions = Vec::new();
        for request in &plan.requests {
            match base_control_plane_action(&request.action) {
                "put-object-replication-policy" => {
                    let spec = azure_object_replication_policy_spec(plan)?;
                    self.client.remove_object_replication_policy(&spec).await?;
                    actions.push(request.action.clone());
                }
                action => {
                    return Err(CrabError::Configuration {
                        key: "replication.control_plane.azure".into(),
                        origin: format!("unsupported Azure control-plane remove action {action}"),
                    });
                }
            }
        }

        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::Azure,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!(
                "removed Crab-managed Azure replication resources for replica {}",
                plan.ownership.replica_name
            ),
        })
    }
}

#[async_trait]
impl<C> ReplicationControlPlaneBackend for GcsReplicationControlPlaneBackend<C>
where
    C: GcsReplicationControlPlaneClient,
{
    fn provider(&self) -> ReplicationProviderKind {
        ReplicationProviderKind::Gcs
    }

    async fn apply(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        let mut actions = Vec::new();
        for request in &plan.requests {
            match base_control_plane_action(&request.action) {
                "validate-dual-region-replication" => {
                    let state = self.gcs_bucket_topology_state(request).await?;
                    ensure_gcs_verified(request, state)?;
                    actions.push(request.action.clone());
                }
                "patch-bucket-rpo" => {
                    let bucket = gcs_bucket_from_url(&request.target)?;
                    let state = self.gcs_bucket_state(&bucket).await?;
                    if state.rpo.as_deref() != Some("ASYNC_TURBO") {
                        self.client
                            .set_bucket_rpo(&bucket, "ASYNC_TURBO", state.metageneration)
                            .await?;
                    }
                    actions.push(request.action.clone());
                }
                "create-storage-transfer-backfill-job" => {
                    let spec = gcs_backfill_spec(plan)?;
                    match self.client.backfill_job(&spec).await? {
                        Some(state) if gcs_backfill_state_matches(&spec, &state) => {}
                        Some(state)
                            if state.crab_managed
                                && state.destination_bucket == spec.destination_bucket =>
                        {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.gcs.backfill".into(),
                                origin: format!(
                                    "GCS Storage Transfer backfill job {} is {} and has not completed",
                                    state.job_id, state.status
                                ),
                            });
                        }
                        Some(state) => {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.gcs.backfill".into(),
                                origin: format!(
                                    "GCS Storage Transfer backfill job {} exists but is not Crab-managed for this destination",
                                    state.job_id
                                ),
                            });
                        }
                        None => self.client.create_backfill_job(&spec).await?,
                    }
                    actions.push(request.action.clone());
                }
                action if action.starts_with("validate-") => {
                    let spec = gcs_policy_validation_spec(plan, action)?;
                    let state = self.client.validate_policy(&spec).await?;
                    ensure_gcs_verified(request, state)?;
                    actions.push(request.action.clone());
                }
                action => {
                    return Err(CrabError::Configuration {
                        key: "replication.control_plane.gcs".into(),
                        origin: format!("unsupported GCS control-plane action {action}"),
                    });
                }
            }
        }

        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::Gcs,
            applied: true,
            checked_drift: true,
            actions,
            message: format!(
                "applied Crab-managed GCS replication settings for replica {}",
                plan.ownership.replica_name
            ),
        })
    }

    async fn status(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneStatus> {
        let mut checks = Vec::new();
        for request in &plan.requests {
            let check = match base_control_plane_action(&request.action) {
                "validate-dual-region-replication" => {
                    self.gcs_bucket_topology_check(request).await?
                }
                "patch-bucket-rpo" => self.gcs_rpo_check(request).await?,
                "create-storage-transfer-backfill-job" => {
                    self.gcs_backfill_check(plan, request).await?
                }
                action if action.starts_with("validate-") => {
                    self.gcs_policy_validation_check(plan, request, action)
                        .await?
                }
                action => control_plane_check(
                    request,
                    ControlPlaneCheckState::Unsupported,
                    format!("unsupported GCS control-plane action {action}"),
                    "upgrade Crab or remove the unsupported provider action from this plan",
                ),
            };
            checks.push(check);
        }

        Ok(ControlPlaneStatus {
            provider: ReplicationProviderKind::Gcs,
            replica_name: plan.ownership.replica_name.clone(),
            primary: plan.ownership.primary.clone(),
            replica: plan.ownership.replica.clone(),
            backend_available: true,
            checked_drift: true,
            checks,
        })
    }

    async fn remove(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        if !plan.requests.is_empty() {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.gcs".into(),
                origin:
                    "GCS remove plan contains reversible resources, but no Crab-owned GCS replication resources are removable"
                        .into(),
            });
        }
        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::Gcs,
            applied: false,
            checked_drift: true,
            actions: Vec::new(),
            message: format!(
                "no Crab-owned GCS replication resources to remove for replica {}",
                plan.ownership.replica_name
            ),
        })
    }
}

pub(crate) struct S3ReplicationControlPlaneBackend<C> {
    client: C,
}

impl<C> S3ReplicationControlPlaneBackend<C> {
    pub(crate) fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C> ReplicationControlPlaneBackend for S3ReplicationControlPlaneBackend<C>
where
    C: S3ReplicationControlPlaneClient,
{
    fn provider(&self) -> ReplicationProviderKind {
        ReplicationProviderKind::S3
    }

    async fn apply(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        let role_spec = s3_replication_role_spec(plan)?;
        let mut actions = Vec::new();
        for request in &plan.requests {
            match base_control_plane_action(&request.action) {
                "put-bucket-versioning" => {
                    let bucket = s3_bucket_from_url(&request.target)?;
                    self.client.enable_bucket_versioning(&bucket).await?;
                    actions.push(request.action.clone());
                }
                "create-replication-role" => {
                    match self.client.replication_role(&role_spec).await? {
                        Some(state) if s3_role_state_matches(&role_spec, &state) => {}
                        Some(_) => {
                            return Err(CrabError::Configuration {
                                key: "replication.control_plane.s3.role".into(),
                                origin: format!(
                                    "S3 replication role {} exists but is not Crab-managed for replica {}",
                                    role_spec.role_name, plan.ownership.replica_name
                                ),
                            });
                        }
                        None => {
                            let _state = self.client.create_replication_role(&role_spec).await?;
                        }
                    }
                    actions.push(request.action.clone());
                }
                "put-replication-configuration" => {
                    let role_state =
                        self.client
                            .replication_role(&role_spec)
                            .await?
                            .ok_or_else(|| CrabError::Configuration {
                                key: "replication.control_plane.s3.role".into(),
                                origin: format!(
                                    "S3 replication role {} must exist before installing the replication rule",
                                    role_spec.role_name
                                ),
                            })?;
                    let rule_spec = s3_replication_rule_spec(plan, &role_state.role_arn)?;
                    self.client.put_replication_rule(&rule_spec).await?;
                    actions.push(request.action.clone());
                }
                "create-batch-replication-job" => {
                    let role_state =
                        self.client
                            .replication_role(&role_spec)
                            .await?
                            .ok_or_else(|| CrabError::Configuration {
                                key: "replication.control_plane.s3.role".into(),
                                origin: format!(
                                    "S3 replication role {} must exist before starting Batch Replication",
                                    role_spec.role_name
                                ),
                            })?;
                    let batch_spec = s3_batch_replication_spec(plan, &role_state.role_arn)?;
                    if self
                        .client
                        .batch_replication_job(&batch_spec)
                        .await?
                        .is_none()
                    {
                        self.client
                            .create_batch_replication_job(&batch_spec)
                            .await?;
                    }
                    actions.push(request.action.clone());
                }
                action if action.starts_with("validate-") => {
                    let spec = s3_policy_validation_spec(plan, action)?;
                    let state = self.client.validate_policy(&spec).await?;
                    if state != ControlPlaneCheckState::Verified {
                        return Err(CrabError::Configuration {
                            key: "replication.control_plane.s3.policy".into(),
                            origin: format!(
                                "S3 policy validation {action} is {}; refusing to apply replication",
                                state.as_str()
                            ),
                        });
                    }
                    actions.push(request.action.clone());
                }
                action => {
                    return Err(CrabError::Configuration {
                        key: "replication.control_plane.s3".into(),
                        origin: format!("unsupported S3 control-plane action {action}"),
                    });
                }
            }
        }

        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::S3,
            applied: true,
            checked_drift: true,
            actions,
            message: format!(
                "applied Crab-managed S3 replication resources for replica {}",
                plan.ownership.replica_name
            ),
        })
    }

    async fn status(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneStatus> {
        let role_spec = s3_replication_role_spec(plan)?;
        let mut checks = Vec::new();
        for request in &plan.requests {
            let check = match base_control_plane_action(&request.action) {
                "put-bucket-versioning" => self.s3_bucket_versioning_check(request).await?,
                "create-replication-role" => {
                    self.s3_replication_role_check(request, &role_spec).await?
                }
                "put-replication-configuration" => {
                    self.s3_replication_rule_check(plan, request, &role_spec)
                        .await?
                }
                "create-batch-replication-job" => {
                    self.s3_batch_replication_check(plan, request, &role_spec)
                        .await?
                }
                action if action.starts_with("validate-") => {
                    self.s3_policy_validation_check(plan, request, action)
                        .await?
                }
                action => control_plane_check(
                    request,
                    ControlPlaneCheckState::Unsupported,
                    format!("unsupported S3 control-plane action {action}"),
                    "upgrade Crab or remove the unsupported provider action from this plan",
                ),
            };
            checks.push(check);
        }

        Ok(ControlPlaneStatus {
            provider: ReplicationProviderKind::S3,
            replica_name: plan.ownership.replica_name.clone(),
            primary: plan.ownership.primary.clone(),
            replica: plan.ownership.replica.clone(),
            backend_available: true,
            checked_drift: true,
            checks,
        })
    }

    async fn remove(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneApplyStatus> {
        let role_spec = s3_replication_role_spec(plan)?;
        let mut actions = Vec::new();
        for request in &plan.requests {
            match base_control_plane_action(&request.action) {
                "put-replication-configuration" => {
                    let rule_id = s3_replication_rule_request(plan)?
                        .managed_resource_id
                        .clone();
                    self.client
                        .remove_replication_rule(&role_spec.source_bucket, &rule_id)
                        .await?;
                    actions.push(request.action.clone());
                }
                "create-replication-role" => {
                    self.client.delete_replication_role(&role_spec).await?;
                    actions.push(request.action.clone());
                }
                action => {
                    return Err(CrabError::Configuration {
                        key: "replication.control_plane.s3".into(),
                        origin: format!("unsupported S3 control-plane remove action {action}"),
                    });
                }
            }
        }

        Ok(ControlPlaneApplyStatus {
            provider: ReplicationProviderKind::S3,
            applied: true,
            checked_drift: true,
            actions,
            message: format!(
                "removed Crab-managed S3 replication resources for replica {}",
                plan.ownership.replica_name
            ),
        })
    }
}

impl<C> S3ReplicationControlPlaneBackend<C>
where
    C: S3ReplicationControlPlaneClient,
{
    async fn s3_bucket_versioning_check(
        &self,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let bucket = s3_bucket_from_url(&request.target)?;
        let enabled = self.client.bucket_versioning_enabled(&bucket).await?;
        let state = if enabled {
            ControlPlaneCheckState::Verified
        } else {
            ControlPlaneCheckState::Missing
        };
        let message = if enabled {
            format!("S3 bucket {bucket} has versioning enabled")
        } else {
            format!("S3 bucket {bucket} needs versioning enabled for replication")
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "run crab replica add --apply with S3 admin credentials",
        ))
    }

    async fn s3_replication_role_check(
        &self,
        request: &ControlPlaneRequest,
        spec: &S3ReplicationRoleSpec,
    ) -> Result<ControlPlaneCheck> {
        let state = match self.client.replication_role(spec).await? {
            Some(role) if s3_role_state_matches(spec, &role) => ControlPlaneCheckState::Verified,
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("S3 replication role {} is Crab-managed", spec.role_name)
            }
            ControlPlaneCheckState::Missing => {
                format!("S3 replication role {} does not exist", spec.role_name)
            }
            ControlPlaneCheckState::Drifted => format!(
                "S3 replication role {} exists but does not match Crab ownership or policy",
                spec.role_name
            ),
            _ => format!(
                "S3 replication role {} could not be verified",
                spec.role_name
            ),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "repair or remove the conflicting IAM role, then rerun crab replica add --apply",
        ))
    }

    async fn s3_replication_rule_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
        role_spec: &S3ReplicationRoleSpec,
    ) -> Result<ControlPlaneCheck> {
        let role = self.client.replication_role(role_spec).await?;
        let Some(role) = role else {
            return Ok(control_plane_check(
                request,
                ControlPlaneCheckState::Missing,
                format!(
                    "S3 replication rule {} needs role {} before it can be installed",
                    request.managed_resource_id, role_spec.role_name
                ),
                "run crab replica add --apply with S3 admin credentials",
            ));
        };
        let spec = s3_replication_rule_spec(plan, &role.role_arn)?;
        let state = match self.client.replication_rule(&spec).await? {
            Some(rule) if s3_rule_state_matches(&spec, &rule) => ControlPlaneCheckState::Verified,
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("S3 replication rule {} is installed", spec.rule_id)
            }
            ControlPlaneCheckState::Missing => {
                format!("S3 replication rule {} does not exist", spec.rule_id)
            }
            ControlPlaneCheckState::Drifted => format!(
                "S3 replication rule {} exists but does not match Crab ownership or destination",
                spec.rule_id
            ),
            _ => format!("S3 replication rule {} could not be verified", spec.rule_id),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "repair the conflicting replication rule, then rerun crab replica add --apply",
        ))
    }

    async fn s3_batch_replication_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
        role_spec: &S3ReplicationRoleSpec,
    ) -> Result<ControlPlaneCheck> {
        let role = self.client.replication_role(role_spec).await?;
        let Some(role) = role else {
            return Ok(control_plane_check(
                request,
                ControlPlaneCheckState::Missing,
                format!(
                    "S3 Batch Replication job {} needs role {} before it can start",
                    request.managed_resource_id, role_spec.role_name
                ),
                "run crab replica add --apply with S3 admin credentials",
            ));
        };
        let spec = s3_batch_replication_spec(plan, &role.role_arn)?;
        let job = self.client.batch_replication_job(&spec).await?;
        let state = match job.as_ref() {
            Some(job) if s3_batch_state_matches(&spec, job) => ControlPlaneCheckState::Verified,
            Some(job) if job.crab_managed && job.destination_bucket == spec.destination_bucket => {
                ControlPlaneCheckState::Missing
            }
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => job.as_ref().map_or_else(
                || format!("S3 Batch Replication job {} is complete", spec.job_id),
                |job| format!("S3 Batch Replication job {} is complete", job.job_id),
            ),
            ControlPlaneCheckState::Missing => match job.as_ref() {
                Some(job) => format!(
                    "S3 Batch Replication job {} is {} and has not completed",
                    job.job_id, job.status
                ),
                None => format!(
                    "S3 Batch Replication job {} has not been created",
                    spec.job_id
                ),
            },
            ControlPlaneCheckState::Drifted => format!(
                "S3 Batch Replication job {} exists but is not Crab-managed for this destination",
                spec.job_id
            ),
            _ => format!(
                "S3 Batch Replication job {} could not be verified",
                spec.job_id
            ),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "repair the conflicting batch replication job, then rerun crab replica add --apply",
        ))
    }

    async fn s3_policy_validation_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
        action: &str,
    ) -> Result<ControlPlaneCheck> {
        let spec = s3_policy_validation_spec(plan, action)?;
        let state = self.client.validate_policy(&spec).await?;
        let message = if state == ControlPlaneCheckState::Verified {
            format!("S3 policy validation {action} passed")
        } else {
            format!("S3 policy validation {action} is {}", state.as_str())
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "fix the provider policy finding, then rerun crab replica doctor --deep",
        ))
    }
}

impl<C> GcsReplicationControlPlaneBackend<C>
where
    C: GcsReplicationControlPlaneClient,
{
    async fn gcs_bucket_state(&self, bucket: &str) -> Result<GcsBucketReplicationState> {
        self.client
            .bucket_state(bucket)
            .await?
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.gcs.bucket".into(),
                origin: format!("GCS bucket {bucket} does not exist or is not readable"),
            })
    }

    async fn gcs_bucket_topology_state(
        &self,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheckState> {
        let bucket = gcs_bucket_from_url(&request.target)?;
        let Some(state) = self.client.bucket_state(&bucket).await? else {
            return Ok(ControlPlaneCheckState::Missing);
        };
        Ok(if gcs_bucket_has_replication_topology(&state) {
            ControlPlaneCheckState::Verified
        } else {
            ControlPlaneCheckState::Drifted
        })
    }

    async fn gcs_bucket_topology_check(
        &self,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let bucket = gcs_bucket_from_url(&request.target)?;
        let state = self.gcs_bucket_topology_state(request).await?;
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("GCS bucket {bucket} has a replication-capable topology")
            }
            ControlPlaneCheckState::Missing => {
                format!("GCS bucket {bucket} does not exist or is not readable")
            }
            ControlPlaneCheckState::Drifted => {
                format!("GCS bucket {bucket} is not dual-region or multi-region")
            }
            _ => format!("GCS bucket {bucket} topology could not be verified"),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "use a dual-region or multi-region GCS bucket, then rerun crab replica add --apply",
        ))
    }

    async fn gcs_rpo_check(&self, request: &ControlPlaneRequest) -> Result<ControlPlaneCheck> {
        let bucket = gcs_bucket_from_url(&request.target)?;
        let bucket_state = self.client.bucket_state(&bucket).await?;
        let state = match bucket_state.as_ref() {
            Some(state) if state.rpo.as_deref() == Some("ASYNC_TURBO") => {
                ControlPlaneCheckState::Verified
            }
            Some(state) if !gcs_bucket_is_dual_region(state) => ControlPlaneCheckState::Unsupported,
            None | Some(_) => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("GCS bucket {bucket} has ASYNC_TURBO RPO")
            }
            ControlPlaneCheckState::Missing => {
                format!("GCS bucket {bucket} needs ASYNC_TURBO RPO")
            }
            ControlPlaneCheckState::Unsupported => {
                format!(
                    "GCS bucket {bucket} must be dual-region before Turbo Replication can be enabled"
                )
            }
            _ => format!("GCS bucket {bucket} RPO could not be verified"),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "run crab replica add --apply with GCS admin credentials",
        ))
    }

    async fn gcs_backfill_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let spec = gcs_backfill_spec(plan)?;
        let job = self.client.backfill_job(&spec).await?;
        let state = match job.as_ref() {
            Some(job) if gcs_backfill_state_matches(&spec, job) => ControlPlaneCheckState::Verified,
            Some(job) if job.crab_managed && job.destination_bucket == spec.destination_bucket => {
                ControlPlaneCheckState::Missing
            }
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => job.as_ref().map_or_else(
                || {
                    format!(
                        "GCS Storage Transfer backfill job {} is complete",
                        spec.job_id
                    )
                },
                |job| {
                    let mut message = format!(
                        "GCS Storage Transfer backfill job {} is complete",
                        job.job_id
                    );
                    if let Some(detail) = gcs_backfill_progress_detail(job) {
                        message.push_str("; ");
                        message.push_str(&detail);
                    }
                    message
                },
            ),
            ControlPlaneCheckState::Missing => match job.as_ref() {
                Some(job) => {
                    let mut message = format!(
                        "GCS Storage Transfer backfill job {} is {} and has not completed",
                        job.job_id, job.status
                    );
                    if let Some(operation) = job.operation_name.as_deref() {
                        message.push_str("; latest operation ");
                        message.push_str(operation);
                    }
                    if let Some(detail) = gcs_backfill_progress_detail(job) {
                        message.push_str("; ");
                        message.push_str(&detail);
                    }
                    if let Some(error) = job.error_message.as_deref() {
                        message.push_str("; provider error: ");
                        message.push_str(error);
                    }
                    message
                }
                None => format!(
                    "GCS Storage Transfer backfill job {} has not been created",
                    spec.job_id
                ),
            },
            ControlPlaneCheckState::Drifted => format!(
                "GCS Storage Transfer backfill job {} exists but is not Crab-managed for this destination",
                spec.job_id
            ),
            _ => format!(
                "GCS Storage Transfer backfill job {} could not be verified",
                spec.job_id
            ),
        };
        let mut check = control_plane_check(
            request,
            state,
            message,
            gcs_backfill_remediation(state, job.as_ref()).as_str(),
        );
        check.progress_percent = job.as_ref().and_then(gcs_backfill_progress_percent);
        Ok(check)
    }

    async fn gcs_policy_validation_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
        action: &str,
    ) -> Result<ControlPlaneCheck> {
        let spec = gcs_policy_validation_spec(plan, action)?;
        let state = self.client.validate_policy(&spec).await?;
        let message = if state == ControlPlaneCheckState::Verified {
            format!("GCS policy validation {action} passed")
        } else {
            format!("GCS policy validation {action} is {}", state.as_str())
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            gcs_policy_validation_remediation(action),
        ))
    }
}

impl<C> AzureReplicationControlPlaneBackend<C>
where
    C: AzureReplicationControlPlaneClient,
{
    async fn azure_blob_service_state(&self, account: &str) -> Result<AzureBlobServiceState> {
        self.client
            .blob_service_state(account)
            .await?
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.azure.account".into(),
                origin: format!(
                    "Azure storage account {account} does not exist or is not readable"
                ),
            })
    }

    async fn azure_change_feed_check(
        &self,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let account = azure_account_from_url(&request.target)?;
        let service = self.client.blob_service_state(&account).await?;
        let state = match service.as_ref() {
            Some(service) if service.change_feed_enabled => ControlPlaneCheckState::Verified,
            None | Some(_) => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("Azure storage account {account} has blob change feed enabled")
            }
            ControlPlaneCheckState::Missing => {
                format!("Azure storage account {account} needs blob change feed enabled")
            }
            _ => format!("Azure blob change feed for {account} could not be verified"),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "run crab replica add --apply with Azure Storage admin credentials",
        ))
    }

    async fn azure_versioning_check(
        &self,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let account = azure_account_from_url(&request.target)?;
        let service = self.client.blob_service_state(&account).await?;
        let state = match service.as_ref() {
            Some(service) if service.versioning_enabled => ControlPlaneCheckState::Verified,
            None | Some(_) => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!("Azure storage account {account} has blob versioning enabled")
            }
            ControlPlaneCheckState::Missing => {
                format!("Azure storage account {account} needs blob versioning enabled")
            }
            _ => format!("Azure blob versioning for {account} could not be verified"),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "run crab replica add --apply with Azure Storage admin credentials",
        ))
    }

    async fn azure_object_policy_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let spec = azure_object_replication_policy_spec(plan)?;
        let policy = self.client.object_replication_policy(&spec).await?;
        let state = match policy.as_ref() {
            Some(policy) if azure_object_policy_state_matches(&spec, policy) => {
                ControlPlaneCheckState::Verified
            }
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => {
                format!(
                    "Azure object replication policy {} is installed",
                    spec.policy_id
                )
            }
            ControlPlaneCheckState::Missing => {
                format!(
                    "Azure object replication policy {} is missing",
                    spec.policy_id
                )
            }
            ControlPlaneCheckState::Drifted => format!(
                "Azure object replication policy {} exists but does not match Crab's plan",
                spec.policy_id
            ),
            _ => format!(
                "Azure object replication policy {} could not be verified",
                spec.policy_id
            ),
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "repair the conflicting Azure object replication policy, then rerun crab replica add --apply",
        ))
    }

    async fn azure_backfill_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
    ) -> Result<ControlPlaneCheck> {
        let spec = azure_existing_blob_backfill_spec(plan)?;
        let backfill = self.client.existing_blob_backfill(&spec).await?;
        let state = match backfill.as_ref() {
            Some(backfill) if azure_backfill_state_matches(&spec, backfill) => {
                ControlPlaneCheckState::Verified
            }
            Some(backfill)
                if backfill.crab_managed
                    && backfill.destination_account == spec.destination_account
                    && backfill.destination_container == spec.destination_container =>
            {
                ControlPlaneCheckState::Missing
            }
            Some(_) => ControlPlaneCheckState::Drifted,
            None => ControlPlaneCheckState::Missing,
        };
        let message = match state {
            ControlPlaneCheckState::Verified => backfill.as_ref().map_or_else(
                || {
                    format!(
                        "Azure existing-blob replication {} is complete",
                        spec.job_id
                    )
                },
                |backfill| {
                    format!(
                        "Azure existing-blob replication {} verified {} existing objects",
                        backfill.job_id, backfill.objects_checked
                    )
                },
            ),
            ControlPlaneCheckState::Missing => match backfill.as_ref() {
                Some(backfill) => {
                    let mut message = format!(
                        "Azure existing-blob replication {} is {} after checking {} objects; {} destination objects are still missing",
                        backfill.job_id,
                        backfill.status,
                        backfill.objects_checked,
                        backfill.missing_objects
                    );
                    if let Some(first_missing) = backfill.first_missing.as_deref() {
                        message.push_str("; first missing destination object ");
                        message.push_str(first_missing);
                    }
                    message
                }
                None => format!(
                    "Azure existing-blob replication {} has not reported completion",
                    spec.job_id
                ),
            },
            ControlPlaneCheckState::Drifted => format!(
                "Azure existing-blob replication {} exists but is not Crab-managed for this destination",
                spec.job_id
            ),
            _ => format!(
                "Azure existing-blob replication {} could not be verified",
                spec.job_id
            ),
        };
        let mut check = control_plane_check(
            request,
            state,
            message,
            azure_backfill_remediation(state, backfill.as_ref()).as_str(),
        );
        check.progress_percent = backfill.as_ref().map(azure_backfill_progress_percent);
        Ok(check)
    }

    async fn azure_policy_validation_check(
        &self,
        plan: &ReplicationControlPlanePlan,
        request: &ControlPlaneRequest,
        action: &str,
    ) -> Result<ControlPlaneCheck> {
        let spec = azure_policy_validation_spec(plan, action)?;
        let state = self.client.validate_policy(&spec).await?;
        let message = if state == ControlPlaneCheckState::Verified {
            format!("Azure policy validation {action} passed")
        } else {
            format!("Azure policy validation {action} is {}", state.as_str())
        };
        Ok(control_plane_check(
            request,
            state,
            message,
            "fix the provider policy finding, then rerun crab replica doctor --deep",
        ))
    }
}

fn ensure_gcs_verified(request: &ControlPlaneRequest, state: ControlPlaneCheckState) -> Result<()> {
    if state == ControlPlaneCheckState::Verified {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replication.control_plane.gcs".into(),
        origin: format!(
            "GCS control-plane validation {} is {}; refusing to apply replication",
            request.action,
            state.as_str()
        ),
    })
}

fn ensure_azure_verified(
    request: &ControlPlaneRequest,
    state: ControlPlaneCheckState,
) -> Result<()> {
    if state == ControlPlaneCheckState::Verified {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replication.control_plane.azure".into(),
        origin: format!(
            "Azure control-plane validation {} is {}; refusing to apply replication",
            request.action,
            state.as_str()
        ),
    })
}

/// Optional IaC export format for provider review workflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ControlPlaneExportFormat {
    Terraform,
    CloudFormation,
    Bicep,
}

impl ControlPlaneExportFormat {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terraform => "terraform",
            Self::CloudFormation => "cloudformation",
            Self::Bicep => "bicep",
        }
    }
}

/// Build the provider-neutral setup plan. Some cloud admin operations
/// remain explicit operator actions until the corresponding management
/// SDK/auth surface exists in Crab.
#[must_use]
pub fn setup_plan(
    provider: ReplicationProviderKind,
    primary: &str,
    replica: &str,
    region: &str,
    rpo: ReplicationRpo,
    backfill: bool,
) -> ReplicationSetupPlan {
    let actions = match provider {
        ReplicationProviderKind::S3 => s3_plan(rpo, backfill),
        ReplicationProviderKind::Gcs => gcs_plan(rpo, backfill),
        ReplicationProviderKind::Azure => azure_plan(rpo, backfill),
    };
    ReplicationSetupPlan {
        provider,
        primary: primary.to_owned(),
        replica: replica.to_owned(),
        region: region.to_owned(),
        rpo,
        backfill,
        actions,
    }
}

/// Build the cloud management operation plan for one replica.
#[must_use]
pub fn control_plane_plan(
    replica_name: &str,
    provider: ReplicationProviderKind,
    primary: &str,
    replica: &str,
    region: &str,
    rpo: ReplicationRpo,
    backfill: bool,
) -> ReplicationControlPlanePlan {
    let setup = setup_plan(provider, primary, replica, region, rpo, backfill);
    let ownership = ReplicationOwnership {
        owner: "crab".to_owned(),
        replica_name: replica_name.to_owned(),
        primary: primary.to_owned(),
        replica: replica.to_owned(),
    };
    let requests = match provider {
        ReplicationProviderKind::S3 => {
            s3_control_plane_requests(replica_name, primary, replica, region, rpo, backfill)
        }
        ReplicationProviderKind::Gcs => {
            gcs_control_plane_requests(replica_name, primary, replica, region, rpo, backfill)
        }
        ReplicationProviderKind::Azure => {
            azure_control_plane_requests(replica_name, primary, replica, region, rpo, backfill)
        }
    };
    ReplicationControlPlanePlan {
        setup,
        ownership,
        requests,
    }
}

/// Build the provider-side removal plan for a Crab-managed replica.
#[must_use]
pub fn control_plane_remove_plan(
    replica: &ReplicaConfig,
    primary: &str,
) -> ReplicationControlPlanePlan {
    let mut plan = control_plane_plan(
        &replica.name,
        replica.provider,
        primary,
        &replica.url,
        &replica.region,
        replica.rpo,
        false,
    );
    let mut requests: Vec<_> = plan
        .requests
        .into_iter()
        .filter(|request| request.reversible)
        .collect();
    // Remove dependent policies before the roles or provider toggles they
    // reference so a partial remove cannot orphan a live replication rule.
    requests.reverse();
    plan.requests = requests
        .into_iter()
        .map(|mut request| {
            request.action = format!("remove:{}", request.action);
            request
        })
        .collect();
    plan
}

/// Apply provider control-plane operations.
///
/// The CLI owns this workflow, but live provider backends are intentionally
/// explicit. Until a backend is wired and tested, apply fails closed rather
/// than pretending that cloud replication exists.
pub async fn apply_control_plane_plan(
    plan: &ReplicationControlPlanePlan,
) -> Result<ControlPlaneApplyStatus> {
    if plan.setup.provider == ReplicationProviderKind::S3 {
        #[cfg(feature = "replication-s3-control-plane")]
        {
            let backend = S3ReplicationControlPlaneBackend::new(
                AwsS3ReplicationControlPlaneClient::for_region(&plan.setup.region).await,
            );
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-s3-control-plane"))]
        {
            let backend = S3ReplicationControlPlaneBackend::new(UnavailableS3ControlPlaneClient);
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Gcs {
        #[cfg(feature = "replication-gcs-control-plane")]
        {
            let backend = GcsReplicationControlPlaneBackend::new(
                GoogleGcsReplicationControlPlaneClient::new()
                    .await
                    .map_err(|err| {
                        control_plane_backend_initialization_failed(
                            plan.setup.provider,
                            "apply",
                            &plan.requests,
                            err,
                        )
                    })?,
            );
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-gcs-control-plane"))]
        {
            let backend = GcsReplicationControlPlaneBackend::new(UnavailableGcsControlPlaneClient);
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Azure {
        #[cfg(feature = "replication-azure-control-plane")]
        {
            let backend = AzureReplicationControlPlaneBackend::new(
                AzureStorageReplicationControlPlaneClient::new().map_err(|err| {
                    control_plane_backend_initialization_failed(
                        plan.setup.provider,
                        "apply",
                        &plan.requests,
                        err,
                    )
                })?,
            );
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-azure-control-plane"))]
        {
            let backend =
                AzureReplicationControlPlaneBackend::new(UnavailableAzureControlPlaneClient);
            return apply_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    Err(control_plane_backend_unavailable(
        plan.setup.provider,
        "apply",
        &plan.requests,
    ))
}

/// Apply provider control-plane operations through a live backend.
pub async fn apply_control_plane_plan_with_backend(
    plan: &ReplicationControlPlanePlan,
    backend: &dyn ReplicationControlPlaneBackend,
) -> Result<ControlPlaneApplyStatus> {
    validate_control_plane_backend(plan, backend)?;
    let status = backend.status(plan).await?;
    validate_control_plane_status_matches_plan(plan, &status)?;
    validate_apply_status(&status)?;
    backend.apply(plan).await
}

/// Remove provider control-plane resources created by Crab.
pub async fn remove_control_plane_plan(
    plan: &ReplicationControlPlanePlan,
) -> Result<ControlPlaneApplyStatus> {
    if plan.setup.provider == ReplicationProviderKind::S3 {
        #[cfg(feature = "replication-s3-control-plane")]
        {
            let backend = S3ReplicationControlPlaneBackend::new(
                AwsS3ReplicationControlPlaneClient::for_region(&plan.setup.region).await,
            );
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-s3-control-plane"))]
        {
            let backend = S3ReplicationControlPlaneBackend::new(UnavailableS3ControlPlaneClient);
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Gcs {
        #[cfg(feature = "replication-gcs-control-plane")]
        {
            let backend = GcsReplicationControlPlaneBackend::new(
                GoogleGcsReplicationControlPlaneClient::new()
                    .await
                    .map_err(|err| {
                        control_plane_backend_initialization_failed(
                            plan.setup.provider,
                            "remove",
                            &plan.requests,
                            err,
                        )
                    })?,
            );
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-gcs-control-plane"))]
        {
            let backend = GcsReplicationControlPlaneBackend::new(UnavailableGcsControlPlaneClient);
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Azure {
        #[cfg(feature = "replication-azure-control-plane")]
        {
            let backend = AzureReplicationControlPlaneBackend::new(
                AzureStorageReplicationControlPlaneClient::new().map_err(|err| {
                    control_plane_backend_initialization_failed(
                        plan.setup.provider,
                        "remove",
                        &plan.requests,
                        err,
                    )
                })?,
            );
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
        #[cfg(not(feature = "replication-azure-control-plane"))]
        {
            let backend =
                AzureReplicationControlPlaneBackend::new(UnavailableAzureControlPlaneClient);
            return remove_control_plane_plan_with_backend(plan, &backend).await;
        }
    }
    Err(control_plane_backend_unavailable(
        plan.setup.provider,
        "remove",
        &plan.requests,
    ))
}

/// Remove provider resources after proving Crab ownership and drift state.
pub async fn remove_control_plane_plan_with_backend(
    plan: &ReplicationControlPlanePlan,
    backend: &dyn ReplicationControlPlaneBackend,
) -> Result<ControlPlaneApplyStatus> {
    validate_control_plane_backend(plan, backend)?;
    let status = backend.status(plan).await?;
    validate_control_plane_status_matches_plan(plan, &status)?;
    validate_remove_status(&status)?;
    backend.remove(plan).await
}

/// Inspect planned provider resources through the control-plane status contract.
///
/// Live cloud adapters populate these checks with verified, missing, or drifted
/// state. Until an adapter is wired, Crab reports every required check as
/// unknown so doctor/status can fail closed without changing the JSON shape.
#[must_use]
pub fn inspect_control_plane_plan(plan: &ReplicationControlPlanePlan) -> ControlPlaneStatus {
    let checks = plan
        .requests
        .iter()
        .map(|request| {
            control_plane_check(
                request,
                ControlPlaneCheckState::Unknown,
                format!(
                    "{} control-plane status backend is not wired for {}",
                    request.provider, request.action
                ),
                "run crab replica export for audit, or rerun status after the provider status backend is available",
            )
        })
        .collect();

    ControlPlaneStatus {
        provider: plan.setup.provider,
        replica_name: plan.ownership.replica_name.clone(),
        primary: plan.ownership.primary.clone(),
        replica: plan.ownership.replica.clone(),
        backend_available: false,
        checked_drift: false,
        checks,
    }
}

/// Inspect provider resources through the default Crab CLI control plane.
pub async fn inspect_control_plane_plan_default(
    plan: &ReplicationControlPlanePlan,
) -> ControlPlaneStatus {
    if plan.setup.provider == ReplicationProviderKind::S3 {
        #[cfg(feature = "replication-s3-control-plane")]
        {
            let backend = S3ReplicationControlPlaneBackend::new(
                AwsS3ReplicationControlPlaneClient::for_region(&plan.setup.region).await,
            );
            return inspect_control_plane_plan_with_backend(plan, &backend)
                .await
                .unwrap_or_else(|err| inspect_control_plane_plan_error(plan, err));
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Gcs {
        #[cfg(feature = "replication-gcs-control-plane")]
        {
            let backend = match GoogleGcsReplicationControlPlaneClient::new().await {
                Ok(backend) => GcsReplicationControlPlaneBackend::new(backend),
                Err(err) => return inspect_control_plane_plan_error(plan, err),
            };
            return inspect_control_plane_plan_with_backend(plan, &backend)
                .await
                .unwrap_or_else(|err| inspect_control_plane_plan_error(plan, err));
        }
    }
    if plan.setup.provider == ReplicationProviderKind::Azure {
        #[cfg(feature = "replication-azure-control-plane")]
        {
            let backend = match AzureStorageReplicationControlPlaneClient::new() {
                Ok(backend) => AzureReplicationControlPlaneBackend::new(backend),
                Err(err) => return inspect_control_plane_plan_error(plan, err),
            };
            return inspect_control_plane_plan_with_backend(plan, &backend)
                .await
                .unwrap_or_else(|err| inspect_control_plane_plan_error(plan, err));
        }
        #[cfg(not(feature = "replication-azure-control-plane"))]
        {
            let backend =
                AzureReplicationControlPlaneBackend::new(UnavailableAzureControlPlaneClient);
            return inspect_control_plane_plan_with_backend(plan, &backend)
                .await
                .unwrap_or_else(|err| inspect_control_plane_plan_error(plan, err));
        }
    }
    inspect_control_plane_plan(plan)
}

fn inspect_control_plane_plan_error(
    plan: &ReplicationControlPlanePlan,
    err: CrabError,
) -> ControlPlaneStatus {
    let checks = plan
        .requests
        .iter()
        .map(|request| {
            control_plane_check(
                request,
                ControlPlaneCheckState::Unknown,
                format!(
                    "{} control-plane status failed for {}: {err}",
                    request.provider, request.action
                ),
                "fix provider credentials or permissions, then rerun crab replica doctor --deep",
            )
        })
        .collect();
    ControlPlaneStatus {
        provider: plan.setup.provider,
        replica_name: plan.ownership.replica_name.clone(),
        primary: plan.ownership.primary.clone(),
        replica: plan.ownership.replica.clone(),
        backend_available: false,
        checked_drift: false,
        checks,
    }
}

/// Inspect provider resources through a live backend.
pub async fn inspect_control_plane_plan_with_backend(
    plan: &ReplicationControlPlanePlan,
    backend: &dyn ReplicationControlPlaneBackend,
) -> Result<ControlPlaneStatus> {
    validate_control_plane_backend(plan, backend)?;
    backend.status(plan).await
}

/// Render an audit artifact for teams that review cloud changes as IaC.
pub fn export_control_plane_plan(
    plan: &ReplicationControlPlanePlan,
    format: ControlPlaneExportFormat,
) -> Result<String> {
    let body =
        serde_json::to_string_pretty(plan).map_err(|e| CrabError::Internal(e.to_string()))?;
    let rendered = match format {
        ControlPlaneExportFormat::Terraform => format!(
            "# Crab replication control-plane export\n# Review and translate these provider operations into Terraform resources.\nlocals {{\n  crab_replication_plan = <<JSON\n{body}\nJSON\n}}\n"
        ),
        ControlPlaneExportFormat::CloudFormation => format!(
            "AWSTemplateFormatVersion: '2010-09-09'\nDescription: Crab replication control-plane export\nMetadata:\n  CrabReplicationPlan: |\n{}\n",
            indent_block(&body, 4)
        ),
        ControlPlaneExportFormat::Bicep => format!(
            "// Crab replication control-plane export\n// Review and translate these provider operations into Bicep resources.\nvar crabReplicationPlan = json('''\n{body}\n''')\n"
        ),
    };
    Ok(rendered)
}

fn validate_control_plane_backend(
    plan: &ReplicationControlPlanePlan,
    backend: &dyn ReplicationControlPlaneBackend,
) -> Result<()> {
    if backend.provider() == plan.setup.provider {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replication.control_plane".into(),
        origin: format!(
            "provider backend {} cannot manage {} replication plan",
            backend.provider(),
            plan.setup.provider
        ),
    })
}

fn validate_control_plane_status_matches_plan(
    plan: &ReplicationControlPlanePlan,
    status: &ControlPlaneStatus,
) -> Result<()> {
    if status.provider != plan.setup.provider
        || status.replica_name != plan.ownership.replica_name
        || status.primary != plan.ownership.primary
        || status.replica != plan.ownership.replica
    {
        return Err(CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "provider control-plane status for replica {} does not match planned replica {}",
                status.replica_name, plan.ownership.replica_name
            ),
        });
    }
    Ok(())
}

fn validate_remove_status(status: &ControlPlaneStatus) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "{} replica {} cannot be removed until control-plane drift is verified",
                status.provider, status.replica_name
            ),
        });
    }
    if let Some(check) = status
        .checks
        .iter()
        .find(|check| check.state != ControlPlaneCheckState::Verified)
    {
        return Err(CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "{} is {}; refusing to remove resources that are missing, drifted, unsupported, or unverified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

fn validate_apply_status(status: &ControlPlaneStatus) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "{} replica {} cannot be applied until control-plane drift is verified",
                status.provider, status.replica_name
            ),
        });
    }
    if let Some(check) = status
        .checks
        .iter()
        .find(|check| !control_plane_check_allows_apply(check))
    {
        return Err(CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "{} is {}; refusing to mutate resources that are missing safety proof, drifted, unsupported, or unverified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

fn control_plane_check_allows_apply(check: &ControlPlaneCheck) -> bool {
    match check.state {
        ControlPlaneCheckState::Verified => true,
        ControlPlaneCheckState::Missing => {
            !base_control_plane_action(&check.action).starts_with("validate-")
        }
        ControlPlaneCheckState::Drifted
        | ControlPlaneCheckState::Unknown
        | ControlPlaneCheckState::Unsupported => false,
    }
}

fn control_plane_backend_unavailable(
    provider: ReplicationProviderKind,
    operation: &str,
    requests: &[ControlPlaneRequest],
) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane".into(),
        origin: format!(
            "{provider} control-plane {operation} backend is not wired; {} Crab-owned operation(s) were planned and no cloud resources were changed",
            requests.len()
        ),
    }
}

fn control_plane_backend_initialization_failed(
    provider: ReplicationProviderKind,
    operation: &str,
    requests: &[ControlPlaneRequest],
    err: CrabError,
) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane".into(),
        origin: format!(
            "{provider} control-plane {operation} backend is not wired or configured; {} Crab-owned operation(s) were planned and no cloud resources were changed: {err}",
            requests.len()
        ),
    }
}

#[cfg(not(feature = "replication-s3-control-plane"))]
struct UnavailableS3ControlPlaneClient;

#[cfg(not(feature = "replication-s3-control-plane"))]
#[async_trait]
impl S3ReplicationControlPlaneClient for UnavailableS3ControlPlaneClient {
    async fn bucket_versioning_enabled(&self, _bucket: &str) -> Result<bool> {
        Ok(false)
    }

    async fn enable_bucket_versioning(&self, _bucket: &str) -> Result<()> {
        Err(s3_control_plane_sdk_unavailable("enable bucket versioning"))
    }

    async fn replication_role(
        &self,
        _spec: &S3ReplicationRoleSpec,
    ) -> Result<Option<S3ReplicationRoleState>> {
        Ok(None)
    }

    async fn create_replication_role(
        &self,
        _spec: &S3ReplicationRoleSpec,
    ) -> Result<S3ReplicationRoleState> {
        Err(s3_control_plane_sdk_unavailable(
            "create IAM replication role",
        ))
    }

    async fn delete_replication_role(&self, _spec: &S3ReplicationRoleSpec) -> Result<()> {
        Err(s3_control_plane_sdk_unavailable(
            "delete IAM replication role",
        ))
    }

    async fn replication_rule(
        &self,
        _spec: &S3ReplicationRuleSpec,
    ) -> Result<Option<S3ReplicationRuleState>> {
        Ok(None)
    }

    async fn put_replication_rule(&self, _spec: &S3ReplicationRuleSpec) -> Result<()> {
        Err(s3_control_plane_sdk_unavailable(
            "put S3 replication configuration",
        ))
    }

    async fn remove_replication_rule(&self, _source_bucket: &str, _rule_id: &str) -> Result<()> {
        Err(s3_control_plane_sdk_unavailable(
            "remove S3 replication configuration",
        ))
    }

    async fn batch_replication_job(
        &self,
        _spec: &S3BatchReplicationSpec,
    ) -> Result<Option<S3BatchReplicationState>> {
        Ok(None)
    }

    async fn create_batch_replication_job(&self, _spec: &S3BatchReplicationSpec) -> Result<()> {
        Err(s3_control_plane_sdk_unavailable(
            "create S3 Batch Replication job",
        ))
    }

    async fn validate_policy(
        &self,
        _spec: &S3PolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        Ok(ControlPlaneCheckState::Unsupported)
    }
}

#[cfg(not(feature = "replication-s3-control-plane"))]
fn s3_control_plane_sdk_unavailable(operation: &str) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3".into(),
        origin: format!(
            "S3 control-plane SDK adapter is not wired for {operation}; no cloud resources were changed"
        ),
    }
}

#[cfg(not(feature = "replication-gcs-control-plane"))]
struct UnavailableGcsControlPlaneClient;

#[cfg(not(feature = "replication-gcs-control-plane"))]
#[async_trait]
impl GcsReplicationControlPlaneClient for UnavailableGcsControlPlaneClient {
    async fn bucket_state(&self, _bucket: &str) -> Result<Option<GcsBucketReplicationState>> {
        Ok(None)
    }

    async fn set_bucket_rpo(
        &self,
        _bucket: &str,
        _rpo: &str,
        _if_metageneration_match: i64,
    ) -> Result<()> {
        Err(gcs_control_plane_sdk_unavailable("set bucket RPO"))
    }

    async fn backfill_job(
        &self,
        _spec: &GcsStorageTransferBackfillSpec,
    ) -> Result<Option<GcsStorageTransferBackfillState>> {
        Ok(None)
    }

    async fn create_backfill_job(&self, _spec: &GcsStorageTransferBackfillSpec) -> Result<()> {
        Err(gcs_control_plane_sdk_unavailable(
            "create Storage Transfer backfill job",
        ))
    }

    async fn validate_policy(
        &self,
        _spec: &GcsPolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        Ok(ControlPlaneCheckState::Unsupported)
    }
}

#[cfg(not(feature = "replication-gcs-control-plane"))]
fn gcs_control_plane_sdk_unavailable(operation: &str) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.gcs".into(),
        origin: format!(
            "GCS control-plane SDK adapter is not wired for {operation}; no cloud resources were changed"
        ),
    }
}

#[cfg(not(feature = "replication-azure-control-plane"))]
struct UnavailableAzureControlPlaneClient;

#[cfg(not(feature = "replication-azure-control-plane"))]
#[async_trait]
impl AzureReplicationControlPlaneClient for UnavailableAzureControlPlaneClient {
    async fn blob_service_state(&self, _account: &str) -> Result<Option<AzureBlobServiceState>> {
        Err(azure_control_plane_sdk_unavailable(
            "read Azure Blob service properties",
        ))
    }

    async fn set_change_feed(&self, _account: &str, _enabled: bool) -> Result<()> {
        Err(azure_control_plane_sdk_unavailable(
            "set Azure Blob change feed",
        ))
    }

    async fn set_blob_versioning(&self, _account: &str, _enabled: bool) -> Result<()> {
        Err(azure_control_plane_sdk_unavailable(
            "set Azure Blob versioning",
        ))
    }

    async fn object_replication_policy(
        &self,
        _spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<Option<AzureObjectReplicationPolicyState>> {
        Err(azure_control_plane_sdk_unavailable(
            "read Azure Object Replication policy",
        ))
    }

    async fn put_object_replication_policy(
        &self,
        _spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()> {
        Err(azure_control_plane_sdk_unavailable(
            "put Azure Object Replication policy",
        ))
    }

    async fn remove_object_replication_policy(
        &self,
        _spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()> {
        Err(azure_control_plane_sdk_unavailable(
            "remove Azure Object Replication policy",
        ))
    }

    async fn existing_blob_backfill(
        &self,
        _spec: &AzureExistingBlobBackfillSpec,
    ) -> Result<Option<AzureExistingBlobBackfillState>> {
        Err(azure_control_plane_sdk_unavailable(
            "read Azure existing-blob replication progress",
        ))
    }

    async fn validate_policy(
        &self,
        _spec: &AzurePolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        Err(azure_control_plane_sdk_unavailable(
            "validate Azure replication policy",
        ))
    }
}

#[cfg(not(feature = "replication-azure-control-plane"))]
fn azure_control_plane_sdk_unavailable(operation: &str) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.azure".into(),
        origin: format!(
            "Azure control-plane SDK adapter is not wired for {operation}; no cloud resources were changed"
        ),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
#[derive(Clone)]
pub(crate) struct AzureStorageReplicationControlPlaneClient {
    client: azure_mgmt_storage::Client,
    subscription_id: String,
    resource_group_name: String,
}

#[cfg(feature = "replication-azure-control-plane")]
#[derive(Debug, Clone)]
struct AzureStorageAccountReplicationState {
    key_source: Option<String>,
    allow_blob_public_access: Option<bool>,
    allow_cross_tenant_replication: Option<bool>,
    is_hns_enabled: bool,
}

#[cfg(feature = "replication-azure-control-plane")]
#[derive(Debug, Clone)]
struct AzureContainerPolicyState {
    public_access_enabled: bool,
    has_legal_hold: bool,
    has_immutability_policy: bool,
    has_default_encryption_scope: bool,
}

#[cfg(feature = "replication-azure-control-plane")]
impl AzureStorageReplicationControlPlaneClient {
    pub(crate) fn new() -> Result<Self> {
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
        let credential = azure_identity::create_credential().map_err(azure_auth_error)?;
        let client = azure_mgmt_storage::Client::builder(credential)
            .build()
            .map_err(|err| azure_sdk_error("build ARM client", err))?;
        Ok(Self {
            client,
            subscription_id,
            resource_group_name,
        })
    }

    async fn read_blob_service_properties(
        &self,
        account: &str,
    ) -> Result<Option<azure_models::BlobServiceProperties>> {
        match self
            .client
            .blob_services_client()
            .get_service_properties(
                &self.resource_group_name,
                account,
                &self.subscription_id,
                "default",
            )
            .await
        {
            Ok(properties) => Ok(Some(properties)),
            Err(err) if is_azure_not_found(&err) => Ok(None),
            Err(err) => Err(azure_sdk_error("read Blob service properties", err)),
        }
    }

    async fn update_blob_service_properties(
        &self,
        account: &str,
        update: impl FnOnce(&mut azure_models::blob_service_properties::Properties),
    ) -> Result<()> {
        let mut service = self
            .read_blob_service_properties(account)
            .await?
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.azure.account".into(),
                origin: format!(
                    "Azure storage account {account} does not exist or is not readable"
                ),
            })?;
        let properties = service
            .properties
            .get_or_insert_with(azure_models::blob_service_properties::Properties::new);
        update(properties);
        self.client
            .blob_services_client()
            .set_service_properties(
                &self.resource_group_name,
                account,
                &self.subscription_id,
                "default",
                service,
            )
            .await
            .map_err(|err| azure_sdk_error("update Blob service properties", err))?;
        Ok(())
    }

    async fn storage_account_state(
        &self,
        account: &str,
    ) -> Result<Option<AzureStorageAccountReplicationState>> {
        match self
            .client
            .storage_accounts_client()
            .get_properties(&self.resource_group_name, account, &self.subscription_id)
            .await
        {
            Ok(account) => Ok(Some(azure_storage_account_state_from_sdk(account))),
            Err(err) if is_azure_not_found(&err) => Ok(None),
            Err(err) => Err(azure_sdk_error("read storage account properties", err)),
        }
    }

    async fn container_policy_state(
        &self,
        account: &str,
        container: &str,
    ) -> Result<Option<AzureContainerPolicyState>> {
        match self
            .client
            .blob_containers_client()
            .get(
                &self.resource_group_name,
                account,
                container,
                &self.subscription_id,
            )
            .await
        {
            Ok(container) => Ok(Some(azure_container_policy_state_from_sdk(container))),
            Err(err) if is_azure_not_found(&err) => Ok(None),
            Err(err) => Err(azure_sdk_error("read Blob container properties", err)),
        }
    }

    async fn object_replication_policy_models(
        &self,
        account: &str,
    ) -> Result<Vec<azure_models::ObjectReplicationPolicy>> {
        let response = self
            .client
            .object_replication_policies_client()
            .list(&self.resource_group_name, account, &self.subscription_id)
            .send()
            .await;
        match response {
            Ok(response) => {
                let policies = response
                    .into_body()
                    .await
                    .map_err(|err| azure_sdk_error("decode Object Replication policies", err))?;
                Ok(policies.value)
            }
            Err(err) if is_azure_not_found(&err) => Ok(Vec::new()),
            Err(err) => Err(azure_sdk_error("list Object Replication policies", err)),
        }
    }

    async fn matching_object_replication_policy_model(
        &self,
        account: &str,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<Option<azure_models::ObjectReplicationPolicy>> {
        Ok(self
            .object_replication_policy_models(account)
            .await?
            .into_iter()
            .find(|policy| azure_object_policy_model_matches(spec, policy)))
    }

    async fn management_policy(
        &self,
        account: &str,
    ) -> Result<Option<azure_models::ManagementPolicy>> {
        match self
            .client
            .management_policies_client()
            .get(
                &self.resource_group_name,
                account,
                &self.subscription_id,
                "default",
            )
            .await
        {
            Ok(policy) => Ok(Some(policy)),
            Err(err) if is_azure_not_found(&err) => Ok(None),
            Err(err) => Err(azure_sdk_error("read lifecycle management policy", err)),
        }
    }

    async fn pair_policy_inputs(
        &self,
        spec: &AzurePolicyValidationSpec,
    ) -> Result<Option<AzurePairPolicyInputs>> {
        let source_account = self.storage_account_state(&spec.source_account).await?;
        let destination_account = self
            .storage_account_state(&spec.destination_account)
            .await?;
        let source_container = self
            .container_policy_state(&spec.source_account, &spec.source_container)
            .await?;
        let destination_container = self
            .container_policy_state(&spec.destination_account, &spec.destination_container)
            .await?;
        let (
            Some(source_account),
            Some(destination_account),
            Some(source_container),
            Some(destination_container),
        ) = (
            source_account,
            destination_account,
            source_container,
            destination_container,
        )
        else {
            return Ok(None);
        };
        Ok(Some(AzurePairPolicyInputs {
            source_account,
            destination_account,
            source_container,
            destination_container,
        }))
    }
}

#[cfg(feature = "replication-azure-control-plane")]
struct AzurePairPolicyInputs {
    source_account: AzureStorageAccountReplicationState,
    destination_account: AzureStorageAccountReplicationState,
    source_container: AzureContainerPolicyState,
    destination_container: AzureContainerPolicyState,
}

#[cfg(feature = "replication-azure-control-plane")]
#[async_trait]
impl AzureReplicationControlPlaneClient for AzureStorageReplicationControlPlaneClient {
    async fn blob_service_state(&self, account: &str) -> Result<Option<AzureBlobServiceState>> {
        Ok(self
            .read_blob_service_properties(account)
            .await?
            .map(|properties| azure_blob_service_state_from_sdk(account, properties)))
    }

    async fn set_change_feed(&self, account: &str, enabled: bool) -> Result<()> {
        self.update_blob_service_properties(account, |properties| {
            let change_feed = properties
                .change_feed
                .get_or_insert_with(azure_models::ChangeFeed::new);
            change_feed.enabled = Some(enabled);
        })
        .await
    }

    async fn set_blob_versioning(&self, account: &str, enabled: bool) -> Result<()> {
        self.update_blob_service_properties(account, |properties| {
            properties.is_versioning_enabled = Some(enabled);
        })
        .await
    }

    async fn object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<Option<AzureObjectReplicationPolicyState>> {
        let mut drifted = None;
        for policy in self
            .object_replication_policy_models(&spec.source_account)
            .await?
        {
            let Some(mut state) = azure_object_policy_state_from_sdk(spec, &policy) else {
                continue;
            };
            state.crab_managed = azure_object_policy_fields_match(spec, &state);
            if state.crab_managed {
                return Ok(Some(state));
            }
            if state.source_account == spec.source_account
                && state.destination_account == spec.destination_account
                && state.source_container == spec.source_container
            {
                drifted = Some(state);
            }
        }
        Ok(drifted)
    }

    async fn put_object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()> {
        if let Some(state) = self.object_replication_policy(spec).await? {
            if azure_object_policy_state_matches(spec, &state) {
                return Ok(());
            }
            return Err(CrabError::Configuration {
                key: "replication.control_plane.azure.policy".into(),
                origin: format!(
                    "Azure Object Replication policy {} exists but does not match Crab's plan",
                    state.policy_id
                ),
            });
        }
        let destination_policy = match self
            .matching_object_replication_policy_model(&spec.destination_account, spec)
            .await?
        {
            Some(policy) => policy,
            None => self
                .client
                .object_replication_policies_client()
                .create_or_update(
                    &self.resource_group_name,
                    &spec.destination_account,
                    &self.subscription_id,
                    "default",
                    azure_object_replication_policy_model(spec),
                )
                .await
                .map_err(|err| {
                    azure_sdk_error("create destination Object Replication policy", err)
                })?,
        };
        let policy_id = azure_object_policy_id(&destination_policy)?;
        if !azure_object_policy_has_rule_ids(&destination_policy) {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.azure.policy".into(),
                origin: format!(
                    "Azure Object Replication policy {policy_id} did not return generated rule IDs"
                ),
            });
        }
        self.client
            .object_replication_policies_client()
            .create_or_update(
                &self.resource_group_name,
                &spec.source_account,
                &self.subscription_id,
                &policy_id,
                destination_policy,
            )
            .await
            .map_err(|err| azure_sdk_error("create source Object Replication policy", err))?;
        Ok(())
    }

    async fn remove_object_replication_policy(
        &self,
        spec: &AzureObjectReplicationPolicySpec,
    ) -> Result<()> {
        for account in [&spec.source_account, &spec.destination_account] {
            let Some(policy) = self
                .matching_object_replication_policy_model(account, spec)
                .await?
            else {
                continue;
            };
            let policy_id = azure_object_policy_id(&policy)?;
            self.client
                .object_replication_policies_client()
                .delete(
                    &self.resource_group_name,
                    account,
                    &self.subscription_id,
                    policy_id,
                )
                .send()
                .await
                .map_err(|err| azure_sdk_error("delete Object Replication policy", err))?;
        }
        Ok(())
    }

    async fn existing_blob_backfill(
        &self,
        spec: &AzureExistingBlobBackfillSpec,
    ) -> Result<Option<AzureExistingBlobBackfillState>> {
        let source = build_static_env_target_store(
            crab_storage::StaticEnvStoreTarget::azure_account_container(
                spec.source_account.as_str(),
                spec.source_container.as_str(),
            ),
        )?;
        let destination = build_static_env_target_store(
            crab_storage::StaticEnvStoreTarget::azure_account_container(
                spec.destination_account.as_str(),
                spec.destination_container.as_str(),
            ),
        )?;
        let verification = verify_existing_object_backfill(&source, &destination, spec).await?;
        let complete = verification.missing_objects == 0;
        Ok(Some(AzureExistingBlobBackfillState {
            job_id: spec.job_id.clone(),
            crab_managed: true,
            destination_account: spec.destination_account.clone(),
            destination_container: spec.destination_container.clone(),
            status: if complete {
                "verified-object-set".to_owned()
            } else {
                verification.first_missing.as_ref().map_or_else(
                    || "missing-objects".to_owned(),
                    |key| format!("missing-object:{key}"),
                )
            },
            complete,
            objects_checked: verification.objects_checked,
            missing_objects: verification.missing_objects,
            first_missing: verification.first_missing,
        }))
    }

    async fn validate_policy(
        &self,
        spec: &AzurePolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        let Some(inputs) = self.pair_policy_inputs(spec).await? else {
            return Ok(ControlPlaneCheckState::Missing);
        };
        match spec.action.as_str() {
            "validate-replication-permissions" => {
                self.object_replication_policy_models(&spec.source_account)
                    .await?;
                self.object_replication_policy_models(&spec.destination_account)
                    .await?;
                Ok(ControlPlaneCheckState::Verified)
            }
            "validate-encryption-compatibility" => Ok(azure_encryption_check_state(&inputs)),
            "validate-lifecycle-retention-policy" => {
                let source_policy = self.management_policy(&spec.source_account).await?;
                let destination_policy = self.management_policy(&spec.destination_account).await?;
                Ok(azure_lifecycle_policy_check_state(
                    &spec.prefix_scope,
                    &spec.source_container,
                    source_policy.as_ref(),
                    &spec.destination_prefix_scope,
                    &spec.destination_container,
                    destination_policy.as_ref(),
                ))
            }
            "validate-immutability-policy" => Ok(azure_immutability_check_state(&inputs)),
            "validate-public-access-policy" => Ok(azure_public_access_check_state(&inputs)),
            "validate-cross-tenant-replication" => Ok(azure_cross_tenant_check_state(&inputs)),
            _ => Ok(ControlPlaneCheckState::Unsupported),
        }
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_blob_service_state_from_sdk(
    account: &str,
    service: azure_models::BlobServiceProperties,
) -> AzureBlobServiceState {
    let properties = service.properties.as_ref();
    AzureBlobServiceState {
        account: account.to_owned(),
        change_feed_enabled: properties
            .and_then(|properties| properties.change_feed.as_ref())
            .and_then(|change_feed| change_feed.enabled)
            .unwrap_or(false),
        versioning_enabled: properties
            .and_then(|properties| properties.is_versioning_enabled)
            .unwrap_or(false),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_storage_account_state_from_sdk(
    account: azure_models::StorageAccount,
) -> AzureStorageAccountReplicationState {
    let properties = account.properties.as_ref();
    AzureStorageAccountReplicationState {
        key_source: properties
            .and_then(|properties| properties.encryption.as_ref())
            .and_then(|encryption| encryption.key_source.as_ref())
            .map(azure_key_source_name),
        allow_blob_public_access: properties
            .and_then(|properties| properties.allow_blob_public_access),
        allow_cross_tenant_replication: properties
            .and_then(|properties| properties.allow_cross_tenant_replication),
        is_hns_enabled: properties
            .and_then(|properties| properties.is_hns_enabled)
            .unwrap_or(false),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_container_policy_state_from_sdk(
    container: azure_models::BlobContainer,
) -> AzureContainerPolicyState {
    let properties = container.properties.as_ref();
    AzureContainerPolicyState {
        public_access_enabled: properties
            .and_then(|properties| properties.public_access.as_ref())
            .is_some_and(|public_access| {
                !matches!(
                    public_access,
                    azure_models::container_properties::PublicAccess::None
                )
            }),
        has_legal_hold: properties
            .and_then(|properties| properties.has_legal_hold)
            .unwrap_or(false),
        has_immutability_policy: properties.is_some_and(|properties| {
            properties.has_immutability_policy.unwrap_or(false)
                || properties.immutability_policy.is_some()
                || properties.immutable_storage_with_versioning.is_some()
        }),
        has_default_encryption_scope: properties.is_some_and(|properties| {
            properties.default_encryption_scope.is_some()
                || properties.deny_encryption_scope_override.unwrap_or(false)
        }),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_key_source_name(key_source: &azure_models::encryption::KeySource) -> String {
    match key_source {
        azure_models::encryption::KeySource::MicrosoftStorage => "Microsoft.Storage".to_owned(),
        azure_models::encryption::KeySource::MicrosoftKeyvault => "Microsoft.Keyvault".to_owned(),
        azure_models::encryption::KeySource::UnknownValue(value) => value.clone(),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_object_replication_policy_model(
    spec: &AzureObjectReplicationPolicySpec,
) -> azure_models::ObjectReplicationPolicy {
    let mut rule = azure_models::ObjectReplicationPolicyRule::new(
        spec.source_container.clone(),
        spec.destination_container.clone(),
    );
    if !spec.prefix_scope.is_empty() {
        let mut filter = azure_models::ObjectReplicationPolicyFilter::new();
        filter.prefix_match.clone_from(&spec.prefix_scope);
        rule.filters = Some(filter);
    }
    let mut properties = azure_models::ObjectReplicationPolicyProperties::new(
        spec.source_account.clone(),
        spec.destination_account.clone(),
    );
    properties.rules.push(rule);
    let mut policy = azure_models::ObjectReplicationPolicy::new();
    policy.properties = Some(properties);
    policy
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_object_policy_state_from_sdk(
    spec: &AzureObjectReplicationPolicySpec,
    policy: &azure_models::ObjectReplicationPolicy,
) -> Option<AzureObjectReplicationPolicyState> {
    let properties = policy.properties.as_ref()?;
    let rule = properties.rules.first()?;
    Some(AzureObjectReplicationPolicyState {
        policy_id: properties
            .policy_id
            .clone()
            .or_else(|| policy.resource.name.clone())
            .unwrap_or_default(),
        crab_managed: false,
        enabled: true,
        source_account: properties.source_account.clone(),
        source_container: rule.source_container.clone(),
        destination_account: properties.destination_account.clone(),
        destination_container: rule.destination_container.clone(),
        destination_region: spec.destination_region.clone(),
        prefix_scope: rule
            .filters
            .as_ref()
            .map(|filters| filters.prefix_match.clone())
            .unwrap_or_default(),
        priority: spec.priority,
        existing_blob_replication: spec.existing_blob_replication,
    })
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_object_policy_model_matches(
    spec: &AzureObjectReplicationPolicySpec,
    policy: &azure_models::ObjectReplicationPolicy,
) -> bool {
    azure_object_policy_state_from_sdk(spec, policy)
        .as_ref()
        .is_some_and(|state| azure_object_policy_fields_match(spec, state))
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_object_policy_id(policy: &azure_models::ObjectReplicationPolicy) -> Result<String> {
    policy
        .properties
        .as_ref()
        .and_then(|properties| properties.policy_id.clone())
        .or_else(|| policy.resource.name.clone())
        .filter(|policy_id| !policy_id.is_empty())
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.control_plane.azure.policy".into(),
            origin: "Azure Object Replication policy did not return a policy ID".into(),
        })
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_object_policy_has_rule_ids(policy: &azure_models::ObjectReplicationPolicy) -> bool {
    policy.properties.as_ref().is_some_and(|properties| {
        !properties.rules.is_empty()
            && properties
                .rules
                .iter()
                .all(|rule| rule.rule_id.as_deref().is_some_and(|id| !id.is_empty()))
    })
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_encryption_check_state(inputs: &AzurePairPolicyInputs) -> ControlPlaneCheckState {
    if inputs.source_account.is_hns_enabled || inputs.destination_account.is_hns_enabled {
        return ControlPlaneCheckState::Unsupported;
    }
    let account_key_sources = [
        inputs.source_account.key_source.as_deref(),
        inputs.destination_account.key_source.as_deref(),
    ];
    if account_key_sources
        .iter()
        .all(|source| source.is_none_or(|source| source == "Microsoft.Storage"))
        && !inputs.source_container.has_default_encryption_scope
        && !inputs.destination_container.has_default_encryption_scope
    {
        return ControlPlaneCheckState::Verified;
    }
    ControlPlaneCheckState::Unsupported
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_immutability_check_state(inputs: &AzurePairPolicyInputs) -> ControlPlaneCheckState {
    if inputs.source_container.has_legal_hold
        || inputs.destination_container.has_legal_hold
        || inputs.source_container.has_immutability_policy
        || inputs.destination_container.has_immutability_policy
    {
        return ControlPlaneCheckState::Drifted;
    }
    ControlPlaneCheckState::Verified
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_public_access_check_state(inputs: &AzurePairPolicyInputs) -> ControlPlaneCheckState {
    if inputs.source_account.allow_blob_public_access == Some(true)
        || inputs.destination_account.allow_blob_public_access == Some(true)
        || inputs.source_container.public_access_enabled
        || inputs.destination_container.public_access_enabled
    {
        return ControlPlaneCheckState::Drifted;
    }
    ControlPlaneCheckState::Verified
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_cross_tenant_check_state(inputs: &AzurePairPolicyInputs) -> ControlPlaneCheckState {
    if inputs.source_account.allow_cross_tenant_replication == Some(true)
        || inputs.destination_account.allow_cross_tenant_replication == Some(true)
    {
        return ControlPlaneCheckState::Drifted;
    }
    ControlPlaneCheckState::Verified
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_lifecycle_policy_check_state(
    source_prefix_scope: &[String],
    source_container: &str,
    source_policy: Option<&azure_models::ManagementPolicy>,
    destination_prefix_scope: &[String],
    destination_container: &str,
    destination_policy: Option<&azure_models::ManagementPolicy>,
) -> ControlPlaneCheckState {
    if azure_lifecycle_policy_has_destructive_rule(
        source_prefix_scope,
        source_container,
        source_policy,
    ) || azure_lifecycle_policy_has_destructive_rule(
        destination_prefix_scope,
        destination_container,
        destination_policy,
    ) {
        return ControlPlaneCheckState::Drifted;
    }
    ControlPlaneCheckState::Verified
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_lifecycle_policy_has_destructive_rule(
    prefix_scope: &[String],
    container: &str,
    policy: Option<&azure_models::ManagementPolicy>,
) -> bool {
    let Some(policy) = policy else {
        return false;
    };
    let Some(properties) = policy.properties.as_ref() else {
        return false;
    };
    let protected_prefixes = azure_lifecycle_protected_prefixes(prefix_scope, container);
    properties.policy.rules.iter().any(|rule| {
        rule.enabled.unwrap_or(true)
            && azure_management_rule_deletes_objects(rule)
            && azure_management_rule_covers_prefixes(rule, &protected_prefixes)
    })
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_lifecycle_protected_prefixes(prefix_scope: &[String], container: &str) -> Vec<String> {
    if prefix_scope.is_empty() {
        return vec![format!("{}/", container.trim_matches('/'))];
    }
    prefix_scope
        .iter()
        .map(|prefix| {
            format!(
                "{}/{}",
                container.trim_matches('/'),
                prefix.trim_start_matches('/')
            )
        })
        .collect()
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_management_rule_deletes_objects(rule: &azure_models::ManagementPolicyRule) -> bool {
    let actions = &rule.definition.actions;
    actions
        .base_blob
        .as_ref()
        .is_some_and(|base_blob| base_blob.delete.is_some())
        || actions
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.delete.is_some())
        || actions
            .version
            .as_ref()
            .is_some_and(|version| version.delete.is_some())
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_management_rule_covers_prefixes(
    rule: &azure_models::ManagementPolicyRule,
    protected_prefixes: &[String],
) -> bool {
    let Some(filters) = rule.definition.filters.as_ref() else {
        return true;
    };
    if filters.prefix_match.is_empty() {
        return true;
    }
    filters.prefix_match.iter().any(|rule_prefix| {
        protected_prefixes.iter().any(|protected| {
            protected.starts_with(rule_prefix) || rule_prefix.starts_with(protected)
        })
    })
}

#[cfg(feature = "replication-azure-control-plane")]
fn is_azure_not_found(err: &azure_core::error::Error) -> bool {
    matches!(
        err.kind(),
        azure_core::error::ErrorKind::HttpResponse { status, .. }
            if *status == azure_core::StatusCode::NotFound
    )
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.azure".into(),
        origin: format!("Azure Storage {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-azure-control-plane")]
fn azure_auth_error(err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.azure.auth".into(),
        origin: format!("Azure authentication failed: {err}"),
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
#[derive(Clone)]
pub(crate) struct GoogleGcsReplicationControlPlaneClient {
    client: google_cloud_storage::client::Client,
    transfer: GcsStorageTransferRestClient,
}

#[cfg(feature = "replication-gcs-control-plane")]
impl GoogleGcsReplicationControlPlaneClient {
    pub(crate) async fn new() -> Result<Self> {
        let config = google_cloud_storage::client::ClientConfig::default()
            .with_auth()
            .await
            .map_err(gcs_auth_error)?;
        let project_id = config.project_id.clone().ok_or_else(|| CrabError::Configuration {
            key: "replication.control_plane.gcs.project".into(),
            origin:
                "Google Cloud Storage authentication did not resolve a project ID for Storage Transfer"
                    .into(),
        })?;
        let token_source = config
            .token_source_provider
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.gcs.auth".into(),
                origin: "Google Cloud Storage authentication did not provide a token source".into(),
            })?
            .token_source();
        Ok(Self {
            transfer: GcsStorageTransferRestClient::new(
                reqwest::Client::new(),
                token_source,
                project_id,
            ),
            client: google_cloud_storage::client::Client::new(config),
        })
    }

    async fn gcs_bucket(
        &self,
        bucket: &str,
    ) -> Result<Option<google_cloud_storage::http::buckets::Bucket>> {
        use google_cloud_storage::http::buckets::get::GetBucketRequest;

        match self
            .client
            .get_bucket(&GetBucketRequest {
                bucket: bucket.to_owned(),
                ..Default::default()
            })
            .await
        {
            Ok(bucket) => Ok(Some(bucket)),
            Err(err) if is_gcs_not_found(&err) => Ok(None),
            Err(err) => Err(gcs_sdk_error("read bucket metadata", err)),
        }
    }

    async fn gcs_permission_state(
        &self,
        bucket: &str,
        permissions: &[&str],
    ) -> Result<ControlPlaneCheckState> {
        use google_cloud_storage::http::buckets::test_iam_permissions::TestIamPermissionsRequest;

        if self.gcs_bucket(bucket).await?.is_none() {
            return Ok(ControlPlaneCheckState::Missing);
        }
        let requested = permissions
            .iter()
            .map(|permission| (*permission).to_owned())
            .collect::<Vec<_>>();
        let output = self
            .client
            .test_iam_permissions(&TestIamPermissionsRequest {
                resource: bucket.to_owned(),
                permissions: requested,
            })
            .await
            .map_err(|err| gcs_sdk_error("test IAM permissions", err))?;
        Ok(
            if permissions
                .iter()
                .all(|permission| output.permissions.iter().any(|actual| actual == permission))
            {
                ControlPlaneCheckState::Verified
            } else {
                ControlPlaneCheckState::Drifted
            },
        )
    }

    async fn gcs_bucket_policy_state(
        &self,
        bucket: &str,
        evaluate: impl FnOnce(&GcsBucketReplicationState) -> ControlPlaneCheckState,
    ) -> Result<ControlPlaneCheckState> {
        Ok(match self.bucket_state(bucket).await? {
            Some(state) => evaluate(&state),
            None => ControlPlaneCheckState::Missing,
        })
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
#[async_trait]
impl GcsReplicationControlPlaneClient for GoogleGcsReplicationControlPlaneClient {
    async fn bucket_state(&self, bucket: &str) -> Result<Option<GcsBucketReplicationState>> {
        Ok(self
            .gcs_bucket(bucket)
            .await?
            .map(gcs_bucket_state_from_sdk))
    }

    async fn set_bucket_rpo(
        &self,
        bucket: &str,
        rpo: &str,
        if_metageneration_match: i64,
    ) -> Result<()> {
        use google_cloud_storage::http::buckets::patch::{BucketPatchConfig, PatchBucketRequest};

        self.client
            .patch_bucket(&PatchBucketRequest {
                bucket: bucket.to_owned(),
                if_metageneration_match: Some(if_metageneration_match),
                metadata: Some(BucketPatchConfig {
                    rpo: Some(rpo.to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .map_err(|err| gcs_sdk_error("patch bucket RPO", err))?;
        Ok(())
    }

    async fn backfill_job(
        &self,
        spec: &GcsStorageTransferBackfillSpec,
    ) -> Result<Option<GcsStorageTransferBackfillState>> {
        self.transfer.backfill_job(spec).await
    }

    async fn create_backfill_job(&self, spec: &GcsStorageTransferBackfillSpec) -> Result<()> {
        self.transfer.create_backfill_job(spec).await
    }

    async fn validate_policy(
        &self,
        spec: &GcsPolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        match spec.action.as_str() {
            "validate-replication-permissions" => Ok(gcs_pair_policy_state(
                self.gcs_permission_state(
                    &spec.source_bucket,
                    &[
                        "storage.buckets.get",
                        "storage.buckets.update",
                        "storage.objects.get",
                        "storage.objects.list",
                    ],
                )
                .await?,
                self.gcs_permission_state(
                    &spec.destination_bucket,
                    &[
                        "storage.buckets.get",
                        "storage.buckets.update",
                        "storage.objects.create",
                        "storage.objects.get",
                        "storage.objects.list",
                    ],
                )
                .await?,
            )),
            "validate-encryption-compatibility" => Ok(gcs_pair_policy_state(
                self.gcs_bucket_policy_state(&spec.source_bucket, |bucket| {
                    if bucket.has_cmek {
                        ControlPlaneCheckState::Unsupported
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
                self.gcs_bucket_policy_state(&spec.destination_bucket, |bucket| {
                    if bucket.has_cmek {
                        ControlPlaneCheckState::Unsupported
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
            )),
            "validate-lifecycle-retention-policy" => Ok(gcs_pair_policy_state(
                self.gcs_bucket_policy_state(&spec.source_bucket, |bucket| {
                    if bucket.has_retention_policy || bucket.has_delete_lifecycle_rule {
                        ControlPlaneCheckState::Drifted
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
                self.gcs_bucket_policy_state(&spec.destination_bucket, |bucket| {
                    if bucket.has_retention_policy || bucket.has_delete_lifecycle_rule {
                        ControlPlaneCheckState::Drifted
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
            )),
            "validate-public-access-policy" => Ok(gcs_pair_policy_state(
                self.gcs_bucket_policy_state(&spec.source_bucket, |bucket| {
                    if bucket.public_access_prevention_enforced {
                        ControlPlaneCheckState::Verified
                    } else {
                        ControlPlaneCheckState::Drifted
                    }
                })
                .await?,
                self.gcs_bucket_policy_state(&spec.destination_bucket, |bucket| {
                    if bucket.public_access_prevention_enforced {
                        ControlPlaneCheckState::Verified
                    } else {
                        ControlPlaneCheckState::Drifted
                    }
                })
                .await?,
            )),
            "validate-requester-pays" => Ok(gcs_pair_policy_state(
                self.gcs_bucket_policy_state(&spec.source_bucket, |bucket| {
                    if bucket.requester_pays {
                        ControlPlaneCheckState::Drifted
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
                self.gcs_bucket_policy_state(&spec.destination_bucket, |bucket| {
                    if bucket.requester_pays {
                        ControlPlaneCheckState::Drifted
                    } else {
                        ControlPlaneCheckState::Verified
                    }
                })
                .await?,
            )),
            _ => Ok(ControlPlaneCheckState::Unsupported),
        }
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
#[derive(Clone)]
struct GcsStorageTransferRestClient {
    http: reqwest::Client,
    token_source: Arc<dyn google_cloud_token::TokenSource>,
    project_id: String,
    base_url: String,
}

#[cfg(feature = "replication-gcs-control-plane")]
impl GcsStorageTransferRestClient {
    fn new(
        http: reqwest::Client,
        token_source: Arc<dyn google_cloud_token::TokenSource>,
        project_id: String,
    ) -> Self {
        Self {
            http,
            token_source,
            project_id,
            base_url: "https://storagetransfer.googleapis.com/v1".to_owned(),
        }
    }

    async fn backfill_job(
        &self,
        spec: &GcsStorageTransferBackfillSpec,
    ) -> Result<Option<GcsStorageTransferBackfillState>> {
        let job_name = gcs_transfer_job_name(spec)?;
        let filter = serde_json::json!({
            "projectId": self.project_id,
            "jobNames": [job_name],
            "jobStatuses": ["ENABLED", "DISABLED", "DELETED"],
        });
        let response = self
            .get_json(
                "transferJobs",
                &[
                    ("filter", filter.to_string()),
                    ("pageSize", "256".to_owned()),
                ],
                "list Storage Transfer backfill jobs",
            )
            .await?;
        let Some(job) = response
            .get("transferJobs")
            .and_then(serde_json::Value::as_array)
            .and_then(|jobs| {
                jobs.iter()
                    .find(|job| gcs_json_str(job, &["name"]) == Some(job_name.as_str()))
            })
        else {
            return Ok(None);
        };
        let latest_operation = self.latest_operation(&job_name).await?;
        Ok(Some(gcs_transfer_state_from_job(
            spec,
            job,
            latest_operation.as_ref(),
        )?))
    }

    async fn create_backfill_job(&self, spec: &GcsStorageTransferBackfillSpec) -> Result<()> {
        let request = gcs_transfer_create_request(spec, &self.project_id)?;
        let response = self
            .post_json_raw(
                "transferJobs",
                &request,
                "create Storage Transfer backfill job",
            )
            .await?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.gcs.backfill".into(),
                origin: format!(
                    "GCS Storage Transfer job {} already exists but did not match Crab drift checks",
                    gcs_transfer_job_name(spec)?
                ),
            });
        }
        gcs_transfer_response_json(response, "create Storage Transfer backfill job").await?;
        self.post_json(
            &format!("{}:run", gcs_transfer_job_name(spec)?),
            &serde_json::json!({ "projectId": self.project_id }),
            "run Storage Transfer backfill job",
        )
        .await?;
        Ok(())
    }

    async fn latest_operation(&self, job_name: &str) -> Result<Option<serde_json::Value>> {
        let filter = serde_json::json!({
            "projectId": self.project_id,
            "jobNames": [job_name],
        });
        let response = self
            .get_json(
                "transferOperations",
                &[("filter", filter.to_string()), ("pageSize", "1".to_owned())],
                "list Storage Transfer operations",
            )
            .await?;
        Ok(response
            .get("operations")
            .and_then(serde_json::Value::as_array)
            .and_then(|operations| operations.first())
            .cloned())
    }

    async fn get_json(
        &self,
        path: &str,
        query: &[(&str, String)],
        operation: &str,
    ) -> Result<serde_json::Value> {
        let response = self
            .http
            .get(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .query(query)
            .send()
            .await
            .map_err(|err| gcs_transfer_request_error(operation, err))?;
        gcs_transfer_response_json(response, operation).await
    }

    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        operation: &str,
    ) -> Result<serde_json::Value> {
        let response = self.post_json_raw(path, body, operation).await?;
        gcs_transfer_response_json(response, operation).await
    }

    async fn post_json_raw(
        &self,
        path: &str,
        body: &serde_json::Value,
        operation: &str,
    ) -> Result<reqwest::Response> {
        self.http
            .post(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .json(body)
            .send()
            .await
            .map_err(|err| gcs_transfer_request_error(operation, err))
    }

    async fn bearer_token(&self) -> Result<String> {
        self.token_source
            .token()
            .await
            .map_err(|err| gcs_auth_error(format!("Storage Transfer token failed: {err}")))
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
async fn gcs_transfer_response_json(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| gcs_transfer_request_error(operation, err))?;
    if !status.is_success() {
        return Err(CrabError::Configuration {
            key: "replication.control_plane.gcs.backfill".into(),
            origin: format!(
                "GCS Storage Transfer {operation} failed with HTTP {status}: {}",
                gcs_error_excerpt(&body)
            ),
        });
    }
    serde_json::from_str(&body).map_err(|err| CrabError::Configuration {
        key: "replication.control_plane.gcs.backfill".into(),
        origin: format!("GCS Storage Transfer {operation} returned invalid JSON: {err}"),
    })
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_request_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.gcs.backfill".into(),
        origin: format!("GCS Storage Transfer {operation} request failed: {err}"),
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_error_excerpt(body: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 512;
    let mut excerpt = body.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if body.chars().count() > MAX_ERROR_BODY_CHARS {
        excerpt.push_str("...");
    }
    excerpt
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_job_name(spec: &GcsStorageTransferBackfillSpec) -> Result<String> {
    let job_id = spec.job_id.trim();
    let valid_body = !job_id.is_empty()
        && !job_id.starts_with("OPI")
        && job_id.len() + "transferJobs/".len() <= 128
        && job_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~'))
        && job_id
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_alphanumeric());
    if !valid_body {
        return Err(CrabError::Configuration {
            key: "replication.control_plane.gcs.backfill".into(),
            origin: format!(
                "GCS Storage Transfer job id {} is not a valid non-PosixFilesystem transfer job name",
                spec.job_id
            ),
        });
    }
    Ok(format!("transferJobs/{job_id}"))
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_job_description(spec: &GcsStorageTransferBackfillSpec) -> String {
    format!("Crab GCS Storage Transfer backfill {}", spec.job_id)
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_include_prefixes(spec: &GcsStorageTransferBackfillSpec) -> Result<Vec<String>> {
    let mut prefixes = Vec::new();
    for raw_prefix in &spec.prefix_scope {
        let prefix = raw_prefix.trim_start_matches('/').to_owned();
        if prefix.is_empty() {
            continue;
        }
        if prefix.contains('\r') || prefix.contains('\n') || prefix.len() > 1024 {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.gcs.backfill.prefix".into(),
                origin: format!(
                    "GCS Storage Transfer include prefix {prefix:?} violates provider prefix rules"
                ),
            });
        }
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    if prefixes.len() > 1000 {
        return Err(CrabError::Configuration {
            key: "replication.control_plane.gcs.backfill.prefix".into(),
            origin: "GCS Storage Transfer include prefix scope exceeds the 1000 prefix limit"
                .into(),
        });
    }
    for (idx, prefix) in prefixes.iter().enumerate() {
        if prefixes
            .iter()
            .enumerate()
            .any(|(other_idx, other)| idx != other_idx && other.starts_with(prefix))
        {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.gcs.backfill.prefix".into(),
                origin: format!(
                    "GCS Storage Transfer include prefix {prefix:?} is a prefix of another include prefix"
                ),
            });
        }
    }
    Ok(prefixes)
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_create_request(
    spec: &GcsStorageTransferBackfillSpec,
    project_id: &str,
) -> Result<serde_json::Value> {
    let prefixes = gcs_transfer_include_prefixes(spec)?;
    let mut transfer_spec = serde_json::json!({
        "gcsDataSource": { "bucketName": spec.source_bucket },
        "gcsDataSink": { "bucketName": spec.destination_bucket },
        "transferOptions": {
            "deleteObjectsUniqueInSink": false,
            "deleteObjectsFromSourceAfterTransfer": false,
            "overwriteWhen": "DIFFERENT",
        },
    });
    if !prefixes.is_empty()
        && let Some(transfer_spec) = transfer_spec.as_object_mut()
    {
        transfer_spec.insert(
            "objectConditions".to_owned(),
            serde_json::json!({ "includePrefixes": prefixes }),
        );
    }
    Ok(serde_json::json!({
        "name": gcs_transfer_job_name(spec)?,
        "description": gcs_transfer_job_description(spec),
        "projectId": project_id,
        "status": "ENABLED",
        "transferSpec": transfer_spec,
    }))
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_state_from_job(
    spec: &GcsStorageTransferBackfillSpec,
    job: &serde_json::Value,
    latest_operation: Option<&serde_json::Value>,
) -> Result<GcsStorageTransferBackfillState> {
    let status = latest_operation
        .and_then(gcs_transfer_operation_status)
        .or_else(|| gcs_json_str(job, &["status"]).map(str::to_owned))
        .unwrap_or_else(|| "UNKNOWN".to_owned());
    Ok(GcsStorageTransferBackfillState {
        job_id: spec.job_id.clone(),
        crab_managed: gcs_transfer_job_matches(spec, job)?,
        destination_bucket: gcs_json_str(job, &["transferSpec", "gcsDataSink", "bucketName"])
            .unwrap_or_default()
            .to_owned(),
        complete: status == "SUCCESS",
        operation_name: latest_operation
            .and_then(|operation| gcs_json_str(operation, &["name"]))
            .map(str::to_owned),
        objects_found: latest_operation.and_then(|operation| {
            gcs_json_u64(
                operation,
                &["metadata", "counters", "objectsFoundFromSource"],
            )
        }),
        objects_copied: latest_operation.and_then(|operation| {
            gcs_json_u64(operation, &["metadata", "counters", "objectsCopiedToSink"])
        }),
        objects_skipped: latest_operation.and_then(|operation| {
            gcs_json_u64(
                operation,
                &["metadata", "counters", "objectsFromSourceSkippedBySync"],
            )
        }),
        objects_failed: latest_operation.and_then(|operation| {
            gcs_json_u64(
                operation,
                &["metadata", "counters", "objectsFromSourceFailed"],
            )
        }),
        bytes_found: latest_operation.and_then(|operation| {
            gcs_json_u64(operation, &["metadata", "counters", "bytesFoundFromSource"])
        }),
        bytes_copied: latest_operation.and_then(|operation| {
            gcs_json_u64(operation, &["metadata", "counters", "bytesCopiedToSink"])
        }),
        bytes_skipped: latest_operation.and_then(|operation| {
            gcs_json_u64(
                operation,
                &["metadata", "counters", "bytesFromSourceSkippedBySync"],
            )
        }),
        bytes_failed: latest_operation.and_then(|operation| {
            gcs_json_u64(
                operation,
                &["metadata", "counters", "bytesFromSourceFailed"],
            )
        }),
        error_message: latest_operation
            .and_then(|operation| gcs_json_str(operation, &["error", "message"]))
            .map(str::to_owned),
        status,
    })
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_job_matches(
    spec: &GcsStorageTransferBackfillSpec,
    job: &serde_json::Value,
) -> Result<bool> {
    let expected_prefixes = gcs_transfer_include_prefixes(spec)?;
    let actual_prefixes = gcs_json_string_array(
        job,
        &["transferSpec", "objectConditions", "includePrefixes"],
    )
    .unwrap_or_default();
    let deletes_sink = gcs_json_bool(
        job,
        &[
            "transferSpec",
            "transferOptions",
            "deleteObjectsUniqueInSink",
        ],
    )
    .unwrap_or(false);
    let deletes_source = gcs_json_bool(
        job,
        &[
            "transferSpec",
            "transferOptions",
            "deleteObjectsFromSourceAfterTransfer",
        ],
    )
    .unwrap_or(false);
    Ok(
        gcs_json_str(job, &["name"]) == Some(gcs_transfer_job_name(spec)?.as_str())
            && gcs_json_str(job, &["description"])
                == Some(gcs_transfer_job_description(spec).as_str())
            && gcs_json_str(job, &["transferSpec", "gcsDataSource", "bucketName"])
                == Some(spec.source_bucket.as_str())
            && gcs_json_str(job, &["transferSpec", "gcsDataSink", "bucketName"])
                == Some(spec.destination_bucket.as_str())
            && actual_prefixes == expected_prefixes
            && !deletes_sink
            && !deletes_source,
    )
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_transfer_operation_status(operation: &serde_json::Value) -> Option<String> {
    if let Some(status) = gcs_json_str(operation, &["metadata", "status"]) {
        return Some(status.to_owned());
    }
    match (
        operation.get("done").and_then(serde_json::Value::as_bool),
        operation.get("error"),
        operation.get("response"),
    ) {
        (Some(false), _, _) => Some("IN_PROGRESS".to_owned()),
        (Some(true), Some(_), _) => Some("FAILED".to_owned()),
        (Some(true), _, Some(_)) => Some("SUCCESS".to_owned()),
        _ => None,
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_json_str<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = value;
    for field in path {
        cursor = cursor.get(*field)?;
    }
    cursor.as_str()
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_json_bool(value: &serde_json::Value, path: &[&str]) -> Option<bool> {
    let mut cursor = value;
    for field in path {
        cursor = cursor.get(*field)?;
    }
    cursor.as_bool()
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_json_u64(value: &serde_json::Value, path: &[&str]) -> Option<u64> {
    let mut cursor = value;
    for field in path {
        cursor = cursor.get(*field)?;
    }
    cursor
        .as_u64()
        .or_else(|| cursor.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_json_string_array(value: &serde_json::Value, path: &[&str]) -> Option<Vec<String>> {
    let mut cursor = value;
    for field in path {
        cursor = cursor.get(*field)?;
    }
    cursor
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_bucket_state_from_sdk(
    bucket: google_cloud_storage::http::buckets::Bucket,
) -> GcsBucketReplicationState {
    use google_cloud_storage::http::buckets::iam_configuration::PublicAccessPrevention;
    use google_cloud_storage::http::buckets::lifecycle::rule::ActionType;

    let public_access_prevention_enforced = bucket
        .iam_configuration
        .as_ref()
        .and_then(|iam| iam.public_access_prevention)
        .is_some_and(|prevention| prevention == PublicAccessPrevention::Enforced);
    let requester_pays = bucket
        .billing
        .as_ref()
        .is_some_and(|billing| billing.requester_pays);
    let has_cmek = bucket
        .encryption
        .as_ref()
        .is_some_and(|encryption| !encryption.default_kms_key_name.is_empty());
    let has_delete_lifecycle_rule = bucket.lifecycle.as_ref().is_some_and(|lifecycle| {
        lifecycle.rule.iter().any(|rule| {
            rule.action
                .as_ref()
                .is_some_and(|action| action.r#type == ActionType::Delete)
        })
    });
    GcsBucketReplicationState {
        bucket: bucket.name,
        metageneration: bucket.metageneration,
        location_type: bucket.location_type,
        rpo: bucket.rpo,
        public_access_prevention_enforced,
        requester_pays,
        has_cmek,
        has_retention_policy: bucket.retention_policy.is_some(),
        has_delete_lifecycle_rule,
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
fn is_gcs_not_found(err: &google_cloud_storage::http::Error) -> bool {
    matches!(
        err,
        google_cloud_storage::http::Error::Response(response) if response.code == 404
    )
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.gcs".into(),
        origin: format!("Google Cloud Storage {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-gcs-control-plane")]
fn gcs_auth_error(err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.gcs.auth".into(),
        origin: format!("Google Cloud Storage authentication failed: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
#[derive(Debug, Clone)]
pub(crate) struct AwsS3ReplicationControlPlaneClient {
    s3: aws_sdk_s3::Client,
    iam: aws_sdk_iam::Client,
    s3control: aws_sdk_s3control::Client,
    sts: aws_sdk_sts::Client,
}

#[cfg(feature = "replication-s3-control-plane")]
impl AwsS3ReplicationControlPlaneClient {
    #[must_use]
    pub(crate) fn new(
        s3: aws_sdk_s3::Client,
        iam: aws_sdk_iam::Client,
        s3control: aws_sdk_s3control::Client,
        sts: aws_sdk_sts::Client,
    ) -> Self {
        Self {
            s3,
            iam,
            s3control,
            sts,
        }
    }

    pub(crate) async fn for_region(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_owned()))
            .load()
            .await;
        Self::new(
            aws_sdk_s3::Client::new(&config),
            aws_sdk_iam::Client::new(&config),
            aws_sdk_s3control::Client::new(&config),
            aws_sdk_sts::Client::new(&config),
        )
    }

    async fn account_id(&self) -> Result<String> {
        let output = self
            .sts
            .get_caller_identity()
            .send()
            .await
            .map_err(|err| sts_sdk_error("read caller identity", err))?;
        output
            .account()
            .map(str::to_owned)
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.s3.sts".into(),
                origin: "AWS STS GetCallerIdentity did not return an account id".into(),
            })
    }

    async fn describe_batch_replication_job(
        &self,
        account_id: &str,
        job_id: &str,
        spec: &S3BatchReplicationSpec,
    ) -> Result<Option<S3BatchReplicationState>> {
        let output = self
            .s3control
            .describe_job()
            .account_id(account_id)
            .job_id(job_id)
            .send()
            .await
            .map_err(|err| s3control_sdk_error("describe Batch Replication job", err))?;
        let tags = self
            .s3control
            .get_job_tagging()
            .account_id(account_id)
            .job_id(job_id)
            .send()
            .await
            .map_err(|err| s3control_sdk_error("read Batch Replication job tags", err))?;
        Ok(output
            .job()
            .map(|job| s3_batch_state_from_job(spec, job, tags.tags())))
    }

    async fn replication_configuration(
        &self,
        bucket: &str,
    ) -> Result<Option<aws_sdk_s3::types::ReplicationConfiguration>> {
        let output = match self.s3.get_bucket_replication().bucket(bucket).send().await {
            Ok(output) => output,
            Err(err) => {
                if err.as_service_error().is_some_and(|err| {
                    err.meta().code().is_some_and(|code| {
                        matches!(
                            code,
                            "ReplicationConfigurationNotFoundError"
                                | "NoSuchReplicationConfiguration"
                                | "NoSuchBucketReplication"
                        )
                    })
                }) {
                    return Ok(None);
                }
                return Err(s3_sdk_error("read bucket replication", err));
            }
        };
        Ok(output.replication_configuration().cloned())
    }

    async fn encryption_policy_state(&self, bucket: &str) -> Result<ControlPlaneCheckState> {
        use aws_sdk_s3::types::ServerSideEncryption;

        let output = match self.s3.get_bucket_encryption().bucket(bucket).send().await {
            Ok(output) => output,
            Err(err) => {
                if err.as_service_error().is_some_and(|err| {
                    err.meta().code().is_some_and(|code| {
                        code == "ServerSideEncryptionConfigurationNotFoundError"
                    })
                }) {
                    return Ok(ControlPlaneCheckState::Verified);
                }
                return Err(s3_sdk_error("read bucket encryption", err));
            }
        };
        let Some(config) = output.server_side_encryption_configuration() else {
            return Ok(ControlPlaneCheckState::Verified);
        };
        for rule in config.rules() {
            let Some(default) = rule.apply_server_side_encryption_by_default() else {
                continue;
            };
            if default.sse_algorithm() != &ServerSideEncryption::Aes256 {
                return Ok(ControlPlaneCheckState::Unsupported);
            }
        }
        Ok(ControlPlaneCheckState::Verified)
    }

    async fn lifecycle_policy_state(&self, bucket: &str) -> Result<ControlPlaneCheckState> {
        let output = match self
            .s3
            .get_bucket_lifecycle_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                if err.as_service_error().is_some_and(|err| {
                    err.meta()
                        .code()
                        .is_some_and(|code| code == "NoSuchLifecycleConfiguration")
                }) {
                    return Ok(ControlPlaneCheckState::Verified);
                }
                return Err(s3_sdk_error("read bucket lifecycle", err));
            }
        };
        if output.rules().iter().any(|rule| {
            rule.expiration().is_some() || rule.noncurrent_version_expiration().is_some()
        }) {
            return Ok(ControlPlaneCheckState::Drifted);
        }
        Ok(ControlPlaneCheckState::Verified)
    }

    async fn object_lock_policy_state(&self, bucket: &str) -> Result<ControlPlaneCheckState> {
        let output = match self
            .s3
            .get_object_lock_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                if err.as_service_error().is_some_and(|err| {
                    err.meta()
                        .code()
                        .is_some_and(|code| code == "ObjectLockConfigurationNotFoundError")
                }) {
                    return Ok(ControlPlaneCheckState::Verified);
                }
                return Err(s3_sdk_error("read object lock configuration", err));
            }
        };
        let Some(config) = output.object_lock_configuration() else {
            return Ok(ControlPlaneCheckState::Verified);
        };
        if config.object_lock_enabled().is_some()
            || config
                .rule()
                .and_then(|rule| rule.default_retention())
                .is_some()
        {
            return Ok(ControlPlaneCheckState::Unsupported);
        }
        Ok(ControlPlaneCheckState::Verified)
    }

    async fn public_access_policy_state(&self, bucket: &str) -> Result<ControlPlaneCheckState> {
        let output = match self
            .s3
            .get_public_access_block()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) => {
                if err.as_service_error().is_some_and(|err| {
                    err.meta()
                        .code()
                        .is_some_and(|code| code == "NoSuchPublicAccessBlockConfiguration")
                }) {
                    return Ok(ControlPlaneCheckState::Drifted);
                }
                return Err(s3_sdk_error("read public access block", err));
            }
        };
        let Some(config) = output.public_access_block_configuration() else {
            return Ok(ControlPlaneCheckState::Drifted);
        };
        let hardened = config.block_public_acls().unwrap_or(false)
            && config.ignore_public_acls().unwrap_or(false)
            && config.block_public_policy().unwrap_or(false)
            && config.restrict_public_buckets().unwrap_or(false);
        Ok(if hardened {
            ControlPlaneCheckState::Verified
        } else {
            ControlPlaneCheckState::Drifted
        })
    }

    async fn requester_pays_policy_state(&self, bucket: &str) -> Result<ControlPlaneCheckState> {
        use aws_sdk_s3::types::Payer;

        let output = self
            .s3
            .get_bucket_request_payment()
            .bucket(bucket)
            .send()
            .await
            .map_err(|err| s3_sdk_error("read request payment configuration", err))?;
        Ok(match output.payer() {
            Some(Payer::Requester) => ControlPlaneCheckState::Drifted,
            _ => ControlPlaneCheckState::Verified,
        })
    }

    async fn bucket_owner_id(&self, bucket: &str) -> Result<Option<String>> {
        let output = self
            .s3
            .get_bucket_acl()
            .bucket(bucket)
            .send()
            .await
            .map_err(|err| s3_sdk_error("read bucket owner", err))?;
        Ok(output
            .owner()
            .and_then(|owner| owner.id())
            .map(str::to_owned))
    }
}

#[cfg(feature = "replication-s3-control-plane")]
#[async_trait]
impl S3ReplicationControlPlaneClient for AwsS3ReplicationControlPlaneClient {
    async fn bucket_versioning_enabled(&self, bucket: &str) -> Result<bool> {
        use aws_sdk_s3::types::BucketVersioningStatus;

        let output = self
            .s3
            .get_bucket_versioning()
            .bucket(bucket)
            .send()
            .await
            .map_err(|err| s3_sdk_error("read bucket versioning", err))?;
        Ok(output
            .status()
            .is_some_and(|status| status == &BucketVersioningStatus::Enabled))
    }

    async fn enable_bucket_versioning(&self, bucket: &str) -> Result<()> {
        use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};

        let config = VersioningConfiguration::builder()
            .status(BucketVersioningStatus::Enabled)
            .build();
        self.s3
            .put_bucket_versioning()
            .bucket(bucket)
            .versioning_configuration(config)
            .send()
            .await
            .map_err(|err| s3_sdk_error("enable bucket versioning", err))?;
        Ok(())
    }

    async fn replication_role(
        &self,
        spec: &S3ReplicationRoleSpec,
    ) -> Result<Option<S3ReplicationRoleState>> {
        let output = match self.iam.get_role().role_name(&spec.role_name).send().await {
            Ok(output) => output,
            Err(err) => {
                if matches!(
                    err.as_service_error(),
                    Some(err) if err.is_no_such_entity_exception()
                ) {
                    return Ok(None);
                }
                return Err(iam_sdk_error("read replication role", err));
            }
        };
        let Some(role) = output.role() else {
            return Ok(None);
        };
        let policy_matches = match self
            .iam
            .get_role_policy()
            .role_name(&spec.role_name)
            .policy_name(&spec.policy_name)
            .send()
            .await
        {
            Ok(policy) => s3_policy_document_matches(
                policy.policy_document(),
                &s3_replication_role_policy_document(spec)?,
            ),
            Err(err) => {
                if matches!(
                    err.as_service_error(),
                    Some(err) if err.is_no_such_entity_exception()
                ) {
                    false
                } else {
                    return Err(iam_sdk_error("read replication role policy", err));
                }
            }
        };
        Ok(Some(S3ReplicationRoleState {
            role_arn: role.arn().to_owned(),
            crab_managed: s3_role_tags_match(role.tags(), spec),
            trust_policy_matches: s3_policy_document_matches(
                role.assume_role_policy_document().unwrap_or_default(),
                &s3_replication_assume_role_policy_document()?,
            ),
            policy_matches,
            source_bucket: spec.source_bucket.clone(),
            destination_bucket: spec.destination_bucket.clone(),
        }))
    }

    async fn create_replication_role(
        &self,
        spec: &S3ReplicationRoleSpec,
    ) -> Result<S3ReplicationRoleState> {
        let mut create = self
            .iam
            .create_role()
            .path("/crab/")
            .role_name(&spec.role_name)
            .assume_role_policy_document(s3_replication_assume_role_policy_document()?);
        for tag in s3_iam_tags(spec)? {
            create = create.tags(tag);
        }
        create
            .send()
            .await
            .map_err(|err| iam_sdk_error("create replication role", err))?;
        self.iam
            .put_role_policy()
            .role_name(&spec.role_name)
            .policy_name(&spec.policy_name)
            .policy_document(s3_replication_role_policy_document(spec)?)
            .send()
            .await
            .map_err(|err| iam_sdk_error("put replication role policy", err))?;
        self.replication_role(spec)
            .await?
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.control_plane.s3.role".into(),
                origin: format!(
                    "created IAM role {} but it was not readable during verification",
                    spec.role_name
                ),
            })
    }

    async fn delete_replication_role(&self, spec: &S3ReplicationRoleSpec) -> Result<()> {
        match self
            .iam
            .delete_role_policy()
            .role_name(&spec.role_name)
            .policy_name(&spec.policy_name)
            .send()
            .await
        {
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.as_service_error(),
                    Some(err) if err.is_no_such_entity_exception()
                ) => {}
            Err(err) => return Err(iam_sdk_error("delete replication role policy", err)),
        }
        match self
            .iam
            .delete_role()
            .role_name(&spec.role_name)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err)
                if matches!(
                    err.as_service_error(),
                    Some(err) if err.is_no_such_entity_exception()
                ) =>
            {
                Ok(())
            }
            Err(err) => Err(iam_sdk_error("delete replication role", err)),
        }
    }

    async fn replication_rule(
        &self,
        spec: &S3ReplicationRuleSpec,
    ) -> Result<Option<S3ReplicationRuleState>> {
        use aws_sdk_s3::types::{MetricsStatus, ReplicationRuleStatus, ReplicationTimeStatus};

        let Some(config) = self.replication_configuration(&spec.source_bucket).await? else {
            return Ok(None);
        };
        let Some(rule) = config
            .rules()
            .iter()
            .find(|rule| rule.id() == Some(spec.rule_id.as_str()))
        else {
            return Ok(None);
        };
        let Some(destination) = rule.destination() else {
            return Ok(Some(S3ReplicationRuleState {
                crab_managed: false,
                enabled: false,
                destination_bucket: String::new(),
                destination_region: spec.destination_region.clone(),
                role_arn: config.role().to_owned(),
                rtc_enabled: false,
            }));
        };
        let replication_time_enabled = destination
            .replication_time()
            .is_some_and(|time| time.status() == &ReplicationTimeStatus::Enabled);
        let metrics_enabled = destination
            .metrics()
            .is_some_and(|metrics| metrics.status() == &MetricsStatus::Enabled);
        Ok(Some(S3ReplicationRuleState {
            crab_managed: spec.rule_id.starts_with("crab-replication-"),
            enabled: rule.status() == &ReplicationRuleStatus::Enabled,
            destination_bucket: s3_bucket_from_arn(destination.bucket()),
            destination_region: spec.destination_region.clone(),
            role_arn: config.role().to_owned(),
            rtc_enabled: replication_time_enabled && metrics_enabled,
        }))
    }

    async fn put_replication_rule(&self, spec: &S3ReplicationRuleSpec) -> Result<()> {
        let existing = self.replication_configuration(&spec.source_bucket).await?;
        if let Some(config) = existing.as_ref()
            && config
                .rules()
                .iter()
                .any(|rule| rule.id() != Some(spec.rule_id.as_str()))
        {
            return Err(CrabError::Configuration {
                key: "replication.control_plane.s3.replication".into(),
                origin: format!(
                    "S3 bucket {} already has replication rules outside {}; Crab will not overwrite existing provider replication configuration",
                    spec.source_bucket, spec.rule_id
                ),
            });
        }

        let rule = s3_sdk_replication_rule(spec)?;
        let config = aws_sdk_s3::types::ReplicationConfiguration::builder()
            .role(&spec.role_arn)
            .rules(rule)
            .build()
            .map_err(s3_build_error)?;
        self.s3
            .put_bucket_replication()
            .bucket(&spec.source_bucket)
            .replication_configuration(config)
            .send()
            .await
            .map_err(|err| s3_sdk_error("put replication configuration", err))?;
        Ok(())
    }

    async fn remove_replication_rule(&self, source_bucket: &str, rule_id: &str) -> Result<()> {
        let Some(config) = self.replication_configuration(source_bucket).await? else {
            return Ok(());
        };
        let mut found = false;
        let remaining = config
            .rules()
            .iter()
            .filter_map(|rule| {
                if rule.id() == Some(rule_id) {
                    found = true;
                    None
                } else {
                    Some(rule.clone())
                }
            })
            .collect::<Vec<_>>();
        if !found {
            return Ok(());
        }
        if remaining.is_empty() {
            self.s3
                .delete_bucket_replication()
                .bucket(source_bucket)
                .send()
                .await
                .map_err(|err| s3_sdk_error("delete replication configuration", err))?;
            return Ok(());
        }

        let mut builder =
            aws_sdk_s3::types::ReplicationConfiguration::builder().role(config.role().to_owned());
        for rule in remaining {
            builder = builder.rules(rule);
        }
        let next = builder.build().map_err(s3_build_error)?;
        self.s3
            .put_bucket_replication()
            .bucket(source_bucket)
            .replication_configuration(next)
            .send()
            .await
            .map_err(|err| s3_sdk_error("remove replication rule", err))?;
        Ok(())
    }

    async fn batch_replication_job(
        &self,
        spec: &S3BatchReplicationSpec,
    ) -> Result<Option<S3BatchReplicationState>> {
        let account_id = self.account_id().await?;
        let description = s3_batch_description(spec);
        let mut next_token = None;
        loop {
            let mut request = self
                .s3control
                .list_jobs()
                .account_id(account_id.as_str())
                .max_results(1000);
            if let Some(token) = next_token {
                request = request.next_token(token);
            }
            let output = request
                .send()
                .await
                .map_err(|err| s3control_sdk_error("list Batch Replication jobs", err))?;
            for job in output.jobs() {
                if job.description() != Some(description.as_str()) {
                    continue;
                }
                let Some(job_id) = job.job_id() else {
                    continue;
                };
                return self
                    .describe_batch_replication_job(&account_id, job_id, spec)
                    .await;
            }
            let Some(token) = output.next_token() else {
                return Ok(None);
            };
            next_token = Some(token.to_owned());
        }
    }

    async fn create_batch_replication_job(&self, spec: &S3BatchReplicationSpec) -> Result<()> {
        if self.batch_replication_job(spec).await?.is_some() {
            return Ok(());
        }

        let account_id = self.account_id().await?;
        let mut request = self
            .s3control
            .create_job()
            .account_id(account_id)
            .confirmation_required(false)
            .operation(s3_batch_operation())
            .report(s3_batch_report())
            .client_request_token(&spec.job_id)
            .manifest_generator(s3_batch_manifest_generator(spec)?)
            .description(s3_batch_description(spec))
            .priority(10)
            .role_arn(&spec.role_arn);
        for tag in s3_batch_tags(spec)? {
            request = request.tags(tag);
        }
        request
            .send()
            .await
            .map_err(|err| s3control_sdk_error("create Batch Replication job", err))?;
        Ok(())
    }

    async fn validate_policy(
        &self,
        spec: &S3PolicyValidationSpec,
    ) -> Result<ControlPlaneCheckState> {
        match spec.action.as_str() {
            "validate-replication-permissions" => {
                let role = S3ReplicationRoleSpec {
                    role_name: spec.role_name.clone(),
                    policy_name: spec.policy_name.clone(),
                    replica_name: spec.replica_name.clone(),
                    source_bucket: spec.source_bucket.clone(),
                    destination_bucket: spec.destination_bucket.clone(),
                    prefix_scope: vec!["{repo}/".to_owned(), ".crab/".to_owned()],
                };
                Ok(match self.replication_role(&role).await? {
                    Some(state) if s3_role_state_matches(&role, &state) => {
                        ControlPlaneCheckState::Verified
                    }
                    Some(_) => ControlPlaneCheckState::Drifted,
                    None => ControlPlaneCheckState::Missing,
                })
            }
            "validate-encryption-compatibility" => Ok(s3_pair_policy_state(
                self.encryption_policy_state(&spec.source_bucket).await?,
                self.encryption_policy_state(&spec.destination_bucket)
                    .await?,
            )),
            "validate-lifecycle-retention-policy" => Ok(s3_pair_policy_state(
                self.lifecycle_policy_state(&spec.source_bucket).await?,
                self.lifecycle_policy_state(&spec.destination_bucket)
                    .await?,
            )),
            "validate-immutability-policy" => Ok(s3_pair_policy_state(
                self.object_lock_policy_state(&spec.source_bucket).await?,
                self.object_lock_policy_state(&spec.destination_bucket)
                    .await?,
            )),
            "validate-public-access-policy" => Ok(s3_pair_policy_state(
                self.public_access_policy_state(&spec.source_bucket).await?,
                self.public_access_policy_state(&spec.destination_bucket)
                    .await?,
            )),
            "validate-requester-pays" => Ok(s3_pair_policy_state(
                self.requester_pays_policy_state(&spec.source_bucket)
                    .await?,
                self.requester_pays_policy_state(&spec.destination_bucket)
                    .await?,
            )),
            "validate-cross-account-ownership" => {
                let source_owner = self.bucket_owner_id(&spec.source_bucket).await?;
                let destination_owner = self.bucket_owner_id(&spec.destination_bucket).await?;
                Ok(
                    if source_owner.is_some() && source_owner == destination_owner {
                        ControlPlaneCheckState::Verified
                    } else {
                        ControlPlaneCheckState::Drifted
                    },
                )
            }
            _ => Ok(ControlPlaneCheckState::Unsupported),
        }
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_sdk_replication_rule(
    spec: &S3ReplicationRuleSpec,
) -> Result<aws_sdk_s3::types::ReplicationRule> {
    use aws_sdk_s3::types::{
        DeleteMarkerReplication, DeleteMarkerReplicationStatus, Destination, Metrics,
        MetricsStatus, ReplicationRule, ReplicationRuleFilter, ReplicationRuleStatus,
        ReplicationTime, ReplicationTimeStatus, ReplicationTimeValue,
    };

    let mut destination = Destination::builder().bucket(s3_bucket_arn(&spec.destination_bucket));
    if spec.rtc_enabled {
        let threshold = ReplicationTimeValue::builder().minutes(15).build();
        let metrics = Metrics::builder()
            .status(MetricsStatus::Enabled)
            .event_threshold(threshold.clone())
            .build()
            .map_err(s3_build_error)?;
        let replication_time = ReplicationTime::builder()
            .status(ReplicationTimeStatus::Enabled)
            .time(threshold)
            .build()
            .map_err(s3_build_error)?;
        destination = destination
            .metrics(metrics)
            .replication_time(replication_time);
    }

    ReplicationRule::builder()
        .id(&spec.rule_id)
        .priority(1)
        .filter(ReplicationRuleFilter::builder().prefix("").build())
        .status(ReplicationRuleStatus::Enabled)
        .destination(destination.build().map_err(s3_build_error)?)
        .delete_marker_replication(
            DeleteMarkerReplication::builder()
                .status(DeleteMarkerReplicationStatus::Disabled)
                .build(),
        )
        .build()
        .map_err(s3_build_error)
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_operation() -> aws_sdk_s3control::types::JobOperation {
    use aws_sdk_s3control::types::{JobOperation, S3ReplicateObjectOperation};

    JobOperation::builder()
        .s3_replicate_object(S3ReplicateObjectOperation::builder().build())
        .build()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_report() -> aws_sdk_s3control::types::JobReport {
    aws_sdk_s3control::types::JobReport::builder()
        .enabled(false)
        .build()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_manifest_generator(
    spec: &S3BatchReplicationSpec,
) -> Result<aws_sdk_s3control::types::JobManifestGenerator> {
    use aws_sdk_s3control::types::{
        JobManifestGenerator, JobManifestGeneratorFilter, KeyNameConstraint, ReplicationStatus,
        S3JobManifestGenerator,
    };

    let mut filter = JobManifestGeneratorFilter::builder()
        .eligible_for_replication(true)
        .object_replication_statuses(ReplicationStatus::None)
        .object_replication_statuses(ReplicationStatus::Failed);
    let prefixes = s3_batch_filter_prefixes(spec);
    if !prefixes.is_empty() {
        filter = filter.key_name_constraint(
            KeyNameConstraint::builder()
                .set_match_any_prefix(Some(prefixes))
                .build(),
        );
    }
    let generator = S3JobManifestGenerator::builder()
        .source_bucket(s3_bucket_arn(&spec.source_bucket))
        .filter(filter.build())
        .enable_manifest_output(false)
        .build()
        .map_err(s3control_build_error)?;
    Ok(JobManifestGenerator::S3JobManifestGenerator(generator))
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_filter_prefixes(spec: &S3BatchReplicationSpec) -> Vec<String> {
    spec.prefix_scope
        .iter()
        .filter(|prefix| !prefix.contains('{'))
        .map(|prefix| prefix.trim_matches('/').to_owned())
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| format!("{prefix}/"))
        .collect()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_tags(spec: &S3BatchReplicationSpec) -> Result<Vec<aws_sdk_s3control::types::S3Tag>> {
    [
        ("crab:managed", "true"),
        ("crab:resource", "replication-backfill"),
        ("crab:replica", spec.replica_name.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| {
        aws_sdk_s3control::types::S3Tag::builder()
            .key(key)
            .value(value)
            .build()
            .map_err(s3control_build_error)
    })
    .collect()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_description(spec: &S3BatchReplicationSpec) -> String {
    format!("Crab S3 Batch Replication {}", spec.job_id)
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_state_from_job(
    spec: &S3BatchReplicationSpec,
    job: &aws_sdk_s3control::types::JobDescriptor,
    tags: &[aws_sdk_s3control::types::S3Tag],
) -> S3BatchReplicationState {
    let status = job
        .status()
        .map_or_else(|| "Unknown".to_owned(), |status| status.as_str().to_owned());
    let complete = job
        .status()
        .is_some_and(|status| status == &aws_sdk_s3control::types::JobStatus::Complete);
    let operation_matches = job
        .operation()
        .is_some_and(|operation| operation.s3_replicate_object().is_some());
    let expected_source_bucket = s3_bucket_arn(&spec.source_bucket);
    let source_matches = job
        .manifest_generator()
        .and_then(|generator| generator.as_s3_job_manifest_generator().ok())
        .is_some_and(|generator| generator.source_bucket() == expected_source_bucket.as_str());
    let expected_description = s3_batch_description(spec);
    let crab_managed = job.description() == Some(expected_description.as_str())
        && job.role_arn() == Some(spec.role_arn.as_str())
        && operation_matches
        && source_matches
        && s3_batch_tags_match(spec, tags);

    S3BatchReplicationState {
        job_id: job
            .job_id()
            .map_or_else(|| spec.job_id.clone(), str::to_owned),
        crab_managed,
        destination_bucket: spec.destination_bucket.clone(),
        status,
        complete,
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_batch_tags_match(
    spec: &S3BatchReplicationSpec,
    tags: &[aws_sdk_s3control::types::S3Tag],
) -> bool {
    tags.iter()
        .any(|tag| tag.key() == "crab:managed" && tag.value() == "true")
        && tags
            .iter()
            .any(|tag| tag.key() == "crab:resource" && tag.value() == "replication-backfill")
        && tags
            .iter()
            .any(|tag| tag.key() == "crab:replica" && tag.value() == spec.replica_name.as_str())
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_pair_policy_state(
    source: ControlPlaneCheckState,
    destination: ControlPlaneCheckState,
) -> ControlPlaneCheckState {
    if source == ControlPlaneCheckState::Verified && destination == ControlPlaneCheckState::Verified
    {
        ControlPlaneCheckState::Verified
    } else if source == ControlPlaneCheckState::Drifted
        || destination == ControlPlaneCheckState::Drifted
    {
        ControlPlaneCheckState::Drifted
    } else {
        ControlPlaneCheckState::Unsupported
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_replication_assume_role_policy_document() -> Result<String> {
    serde_json::to_string(&serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": {
                "Service": [
                    "s3.amazonaws.com",
                    "batchoperations.s3.amazonaws.com"
                ]
            },
            "Action": "sts:AssumeRole"
        }]
    }))
    .map_err(|err| CrabError::Configuration {
        key: "replication.control_plane.s3.role".into(),
        origin: format!("failed to serialize S3 replication trust policy: {err}"),
    })
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_replication_role_policy_document(spec: &S3ReplicationRoleSpec) -> Result<String> {
    serde_json::to_string(&serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetReplicationConfiguration",
                    "s3:ListBucket",
                    "s3:PutInventoryConfiguration"
                ],
                "Resource": s3_bucket_arn(&spec.source_bucket)
            },
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetObjectVersion",
                    "s3:GetObjectVersionForReplication",
                    "s3:GetObjectVersionAcl",
                    "s3:GetObjectVersionTagging",
                    "s3:GetObjectRetention",
                    "s3:GetObjectLegalHold",
                    "s3:InitiateReplication"
                ],
                "Resource": s3_bucket_object_arn(&spec.source_bucket)
            },
            {
                "Effect": "Allow",
                "Action": [
                    "s3:ReplicateObject",
                    "s3:ReplicateDelete",
                    "s3:ReplicateTags"
                ],
                "Resource": s3_bucket_object_arn(&spec.destination_bucket)
            }
        ]
    }))
    .map_err(|err| CrabError::Configuration {
        key: "replication.control_plane.s3.role".into(),
        origin: format!("failed to serialize S3 replication role policy: {err}"),
    })
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_iam_tags(spec: &S3ReplicationRoleSpec) -> Result<Vec<aws_sdk_iam::types::Tag>> {
    [
        ("crab:managed", "true"),
        ("crab:resource", "replication"),
        ("crab:replica", spec.replica_name.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| {
        aws_sdk_iam::types::Tag::builder()
            .key(key)
            .value(value)
            .build()
            .map_err(iam_build_error)
    })
    .collect()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_role_tags_match(tags: &[aws_sdk_iam::types::Tag], spec: &S3ReplicationRoleSpec) -> bool {
    tags.iter()
        .any(|tag| tag.key() == "crab:managed" && tag.value() == "true")
        && tags
            .iter()
            .any(|tag| tag.key() == "crab:resource" && tag.value() == "replication")
        && tags
            .iter()
            .any(|tag| tag.key() == "crab:replica" && tag.value() == spec.replica_name)
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_policy_document_matches(actual: &str, expected: &str) -> bool {
    let actual = urlencoding::decode(actual)
        .map_or_else(|_| actual.to_owned(), std::borrow::Cow::into_owned);
    match (
        serde_json::from_str::<serde_json::Value>(&actual),
        serde_json::from_str::<serde_json::Value>(expected),
    ) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual == expected,
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_bucket_arn(bucket: &str) -> String {
    format!("arn:aws:s3:::{bucket}")
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_bucket_object_arn(bucket: &str) -> String {
    format!("arn:aws:s3:::{bucket}/*")
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_bucket_from_arn(arn: &str) -> String {
    arn.strip_prefix("arn:aws:s3:::").unwrap_or(arn).to_owned()
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3".into(),
        origin: format!("AWS S3 {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn iam_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3.iam".into(),
        origin: format!("AWS IAM {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3control_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3.batch".into(),
        origin: format!("AWS S3 Control {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn sts_sdk_error(operation: &str, err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3.sts".into(),
        origin: format!("AWS STS {operation} failed: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3_build_error(err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3".into(),
        origin: format!("failed to build S3 replication request: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn s3control_build_error(err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3.batch".into(),
        origin: format!("failed to build S3 Batch Replication request: {err}"),
    }
}

#[cfg(feature = "replication-s3-control-plane")]
fn iam_build_error(err: impl fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "replication.control_plane.s3.iam".into(),
        origin: format!("failed to build IAM replication request: {err}"),
    }
}

fn control_plane_check(
    request: &ControlPlaneRequest,
    state: ControlPlaneCheckState,
    message: String,
    remediation: &str,
) -> ControlPlaneCheck {
    ControlPlaneCheck {
        provider: request.provider,
        code: control_plane_check_code(request.provider, &request.action),
        state,
        action: request.action.clone(),
        target: request.target.clone(),
        managed_resource_id: request.managed_resource_id.clone(),
        message,
        remediation: remediation.to_owned(),
        progress_percent: None,
    }
}

fn control_plane_check_code(provider: ReplicationProviderKind, action: &str) -> String {
    let provider = provider.as_str();
    let action = base_control_plane_action(action);
    match action {
        "put-bucket-versioning" | "enable-blob-versioning" => {
            format!("provider.{provider}.versioning.unverified")
        }
        "create-replication-role" => format!("provider.{provider}.replication-role.unverified"),
        "put-replication-configuration" => {
            format!("provider.{provider}.replication-rule.unverified")
        }
        "create-batch-replication-job" => {
            format!("provider.{provider}.batch-replication.unverified")
        }
        "create-storage-transfer-backfill-job" | "track-existing-blob-backfill" => {
            format!("provider.{provider}.backfill.unverified")
        }
        "validate-dual-region-replication" => {
            format!("provider.{provider}.bucket-topology.unverified")
        }
        "patch-bucket-rpo" => format!("provider.{provider}.turbo-rpo.unverified"),
        "enable-change-feed" => format!("provider.{provider}.change-feed.unverified"),
        "put-object-replication-policy" => {
            format!("provider.{provider}.object-replication-policy.unverified")
        }
        "validate-replication-permissions" => {
            format!("provider.{provider}.replication-permissions.unverified")
        }
        "validate-encryption-compatibility" => {
            format!("provider.{provider}.encryption.unverified")
        }
        "validate-lifecycle-retention-policy" => {
            format!("provider.{provider}.lifecycle-retention.unverified")
        }
        "validate-immutability-policy" => {
            format!("provider.{provider}.immutability.unverified")
        }
        "validate-public-access-policy" => {
            format!("provider.{provider}.public-access.unverified")
        }
        "validate-requester-pays" => {
            format!("provider.{provider}.requester-pays.unverified")
        }
        "validate-cross-account-ownership" => {
            format!("provider.{provider}.cross-account-ownership.unverified")
        }
        "validate-cross-tenant-replication" => {
            format!("provider.{provider}.cross-tenant-replication.unverified")
        }
        other => format!(
            "provider.{provider}.{}.unverified",
            other.replace([':', '_'], "-")
        ),
    }
}

fn base_control_plane_action(action: &str) -> &str {
    action.strip_prefix("remove:").unwrap_or(action)
}

fn managed_id(replica_name: &str, suffix: &str) -> String {
    format!("crab-replication-{replica_name}-{suffix}")
}

fn ownership_json(replica_name: &str) -> serde_json::Value {
    serde_json::json!({
        "crab:managed": "true",
        "crab:resource": "replication",
        "crab:replica": replica_name,
    })
}

fn s3_replication_role_spec(plan: &ReplicationControlPlanePlan) -> Result<S3ReplicationRoleSpec> {
    let request = control_plane_request_by_action(plan, "create-replication-role")?;
    let role_name = request_string(request, "role_name")?.to_owned();
    let policy_name = request_string(request, "policy_name")?.to_owned();
    let destination = request_string(request, "destination")?;
    Ok(S3ReplicationRoleSpec {
        role_name,
        policy_name,
        replica_name: plan.ownership.replica_name.clone(),
        source_bucket: s3_bucket_from_url(&request.target)?,
        destination_bucket: s3_bucket_from_url(destination)?,
        prefix_scope: request_string_vec(request, "prefix_scope")?,
    })
}

fn s3_replication_rule_spec(
    plan: &ReplicationControlPlanePlan,
    role_arn: &str,
) -> Result<S3ReplicationRuleSpec> {
    let request = s3_replication_rule_request(plan)?;
    let destination = request_string(request, "destination")?;
    Ok(S3ReplicationRuleSpec {
        rule_id: request.managed_resource_id.clone(),
        replica_name: plan.ownership.replica_name.clone(),
        source_bucket: s3_bucket_from_url(&request.target)?,
        destination_bucket: s3_bucket_from_url(destination)?,
        destination_region: request_string(request, "destination_region")?.to_owned(),
        role_arn: role_arn.to_owned(),
        prefix_scope: request_string_vec(request, "prefix_scope")?,
        rtc_enabled: request_bool(request, "replication_time_control")?,
    })
}

fn s3_batch_replication_spec(
    plan: &ReplicationControlPlanePlan,
    role_arn: &str,
) -> Result<S3BatchReplicationSpec> {
    let request = control_plane_request_by_action(plan, "create-batch-replication-job")?;
    let destination = request_string(request, "destination")?;
    Ok(S3BatchReplicationSpec {
        job_id: request.managed_resource_id.clone(),
        replica_name: plan.ownership.replica_name.clone(),
        source_bucket: s3_bucket_from_url(&request.target)?,
        destination_bucket: s3_bucket_from_url(destination)?,
        role_arn: role_arn.to_owned(),
        prefix_scope: s3_resolved_prefix_scope(
            &request.target,
            request_string_vec(request, "prefix_scope")
                .unwrap_or_else(|_| vec!["{repo}/".to_owned(), ".crab/".to_owned()]),
        )?,
    })
}

fn s3_resolved_prefix_scope(primary: &str, prefixes: Vec<String>) -> Result<Vec<String>> {
    let repo_prefix = ObjectUrl::parse(primary)?.prefix;
    if repo_prefix.is_empty() {
        return Ok(Vec::new());
    }
    let repo_prefix = format!("{}/", repo_prefix.trim_matches('/'));
    let mut resolved = Vec::new();
    for prefix in prefixes {
        let prefix = if prefix == "{repo}/" {
            repo_prefix.clone()
        } else {
            prefix
        };
        let prefix = prefix.trim_start_matches('/').to_owned();
        if !prefix.is_empty() && !resolved.contains(&prefix) {
            resolved.push(prefix);
        }
    }
    Ok(resolved)
}

fn s3_policy_validation_spec(
    plan: &ReplicationControlPlanePlan,
    action: &str,
) -> Result<S3PolicyValidationSpec> {
    let role = s3_replication_role_spec(plan)?;
    Ok(S3PolicyValidationSpec {
        action: action.to_owned(),
        replica_name: plan.ownership.replica_name.clone(),
        source_bucket: s3_bucket_from_url(&plan.ownership.primary)?,
        destination_bucket: s3_bucket_from_url(&plan.ownership.replica)?,
        role_name: role.role_name,
        policy_name: role.policy_name,
    })
}

fn gcs_backfill_spec(plan: &ReplicationControlPlanePlan) -> Result<GcsStorageTransferBackfillSpec> {
    let request = control_plane_request_by_action(plan, "create-storage-transfer-backfill-job")?;
    let destination = request_string(request, "destination")?;
    Ok(GcsStorageTransferBackfillSpec {
        job_id: request.managed_resource_id.clone(),
        source_bucket: gcs_bucket_from_url(&request.target)?,
        destination_bucket: gcs_bucket_from_url(destination)?,
        prefix_scope: gcs_resolved_prefix_scope(
            &request.target,
            request_string_vec(request, "prefix_scope")
                .unwrap_or_else(|_| vec!["{repo}/".to_owned(), ".crab/".to_owned()]),
        )?,
    })
}

fn gcs_policy_validation_spec(
    plan: &ReplicationControlPlanePlan,
    action: &str,
) -> Result<GcsPolicyValidationSpec> {
    Ok(GcsPolicyValidationSpec {
        action: action.to_owned(),
        source_bucket: gcs_bucket_from_url(&plan.ownership.primary)?,
        destination_bucket: gcs_bucket_from_url(&plan.ownership.replica)?,
    })
}

fn azure_object_replication_policy_spec(
    plan: &ReplicationControlPlanePlan,
) -> Result<AzureObjectReplicationPolicySpec> {
    let request = control_plane_request_by_action(plan, "put-object-replication-policy")?;
    let destination = request_string(request, "destination")?;
    let source = azure_storage_target_from_url(&request.target)?;
    let destination = azure_storage_target_from_url(destination)?;
    let prefix_scope = azure_resolved_prefix_scope(
        &source.object_prefix,
        request_string_vec(request, "prefix_scope")
            .unwrap_or_else(|_| vec!["{repo}/".to_owned(), ".crab/".to_owned()]),
    );
    Ok(AzureObjectReplicationPolicySpec {
        policy_id: request.managed_resource_id.clone(),
        replica_name: plan.ownership.replica_name.clone(),
        source_account: source.account,
        source_container: source.container,
        destination_account: destination.account,
        destination_container: destination.container,
        destination_region: request_string(request, "destination_region")?.to_owned(),
        prefix_scope,
        priority: request_bool(request, "priority")?,
        existing_blob_replication: request_bool(request, "existing_blob_replication")?,
    })
}

fn azure_existing_blob_backfill_spec(
    plan: &ReplicationControlPlanePlan,
) -> Result<AzureExistingBlobBackfillSpec> {
    let request = control_plane_request_by_action(plan, "track-existing-blob-backfill")?;
    let destination = request_string(request, "destination")?;
    let source = azure_storage_target_from_url(&request.target)?;
    let destination = azure_storage_target_from_url(destination)?;
    let prefix_scope = azure_resolved_prefix_scope(
        &source.object_prefix,
        vec!["{repo}/".to_owned(), ".crab/".to_owned()],
    );
    let destination_prefix_scope = azure_resolved_prefix_scope(
        &destination.object_prefix,
        vec!["{repo}/".to_owned(), ".crab/".to_owned()],
    );
    Ok(AzureExistingBlobBackfillSpec {
        job_id: request.managed_resource_id.clone(),
        source_account: source.account,
        source_container: source.container,
        destination_account: destination.account,
        destination_container: destination.container,
        prefix_scope,
        destination_prefix_scope,
    })
}

fn azure_policy_validation_spec(
    plan: &ReplicationControlPlanePlan,
    action: &str,
) -> Result<AzurePolicyValidationSpec> {
    let source = azure_storage_target_from_url(&plan.ownership.primary)?;
    let destination = azure_storage_target_from_url(&plan.ownership.replica)?;
    let prefix_scope = azure_resolved_prefix_scope(
        &source.object_prefix,
        vec!["{repo}/".to_owned(), ".crab/".to_owned()],
    );
    let destination_prefix_scope = azure_resolved_prefix_scope(
        &destination.object_prefix,
        vec!["{repo}/".to_owned(), ".crab/".to_owned()],
    );
    Ok(AzurePolicyValidationSpec {
        action: action.to_owned(),
        source_account: source.account,
        source_container: source.container,
        destination_account: destination.account,
        destination_container: destination.container,
        prefix_scope,
        destination_prefix_scope,
    })
}

fn gcs_resolved_prefix_scope(primary: &str, prefixes: Vec<String>) -> Result<Vec<String>> {
    let repo_prefix = ObjectUrl::parse(primary)?.prefix;
    if repo_prefix.is_empty() {
        return Ok(Vec::new());
    }
    let repo_prefix = format!("{}/", repo_prefix.trim_matches('/'));
    let mut resolved = Vec::new();
    for prefix in prefixes {
        let prefix = if prefix == "{repo}/" {
            repo_prefix.clone()
        } else {
            prefix
        };
        let prefix = prefix.trim_start_matches('/').to_owned();
        if !prefix.is_empty() && !resolved.contains(&prefix) {
            resolved.push(prefix);
        }
    }
    Ok(resolved)
}

fn azure_resolved_prefix_scope(repo_prefix: &str, prefixes: Vec<String>) -> Vec<String> {
    if repo_prefix.is_empty() {
        return Vec::new();
    }
    let repo_prefix = format!("{}/", repo_prefix.trim_matches('/'));
    let mut resolved = Vec::new();
    for prefix in prefixes {
        let prefix = if prefix == "{repo}/" {
            repo_prefix.clone()
        } else {
            prefix
        };
        let prefix = prefix.trim_start_matches('/').to_owned();
        if !prefix.is_empty() && !resolved.contains(&prefix) {
            resolved.push(prefix);
        }
    }
    resolved
}

fn s3_replication_rule_request(plan: &ReplicationControlPlanePlan) -> Result<&ControlPlaneRequest> {
    control_plane_request_by_action(plan, "put-replication-configuration")
}

fn control_plane_request_by_action<'a>(
    plan: &'a ReplicationControlPlanePlan,
    action: &str,
) -> Result<&'a ControlPlaneRequest> {
    plan.requests
        .iter()
        .find(|request| base_control_plane_action(&request.action) == action)
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.control_plane".into(),
            origin: format!(
                "provider control-plane plan for replica {} is missing {action}",
                plan.ownership.replica_name
            ),
        })
}

fn request_string<'a>(request: &'a ControlPlaneRequest, field: &str) -> Result<&'a str> {
    request
        .request
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("replication.control_plane.{}", request.action),
            origin: format!(
                "{} request {} is missing string field {field}",
                request.provider, request.managed_resource_id
            ),
        })
}

fn request_bool(request: &ControlPlaneRequest, field: &str) -> Result<bool> {
    request
        .request
        .get(field)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("replication.control_plane.{}", request.action),
            origin: format!(
                "{} request {} is missing bool field {field}",
                request.provider, request.managed_resource_id
            ),
        })
}

fn request_string_vec(request: &ControlPlaneRequest, field: &str) -> Result<Vec<String>> {
    let values = request
        .request
        .get(field)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("replication.control_plane.{}", request.action),
            origin: format!(
                "{} request {} is missing array field {field}",
                request.provider, request.managed_resource_id
            ),
        })?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| CrabError::Configuration {
                    key: format!("replication.control_plane.{}", request.action),
                    origin: format!(
                        "{} request {} field {field} must contain only strings",
                        request.provider, request.managed_resource_id
                    ),
                })
        })
        .collect()
}

fn s3_bucket_from_url(url: &str) -> Result<String> {
    let parsed = ObjectUrl::parse(url)?;
    if parsed.cloud == Cloud::S3 {
        return Ok(parsed.bucket);
    }
    Err(CrabError::Configuration {
        key: "replication.control_plane.s3.url".into(),
        origin: format!("expected S3 or crab URL for S3 replication, got {url}"),
    })
}

fn gcs_bucket_from_url(url: &str) -> Result<String> {
    let parsed = ObjectUrl::parse(url)?;
    if parsed.cloud == Cloud::Gcs {
        return Ok(parsed.bucket);
    }
    Err(CrabError::Configuration {
        key: "replication.control_plane.gcs.url".into(),
        origin: format!("expected GCS or crab URL for GCS replication, got {url}"),
    })
}

fn azure_account_from_url(url: &str) -> Result<String> {
    Ok(azure_storage_target_from_url(url)?.account)
}

fn azure_storage_target_from_url(url: &str) -> Result<crab_git::AzureStorageTarget> {
    let parsed = ObjectUrl::parse(url)?;
    azure_storage_target_from_object_url(&parsed, url)
}

fn azure_storage_target_from_object_url(
    parsed: &ObjectUrl,
    original_url: &str,
) -> Result<crab_git::AzureStorageTarget> {
    crab_git::url::ObjectUrl::from(parsed.clone())
        .azure_storage_target()
        .map_err(|error| azure_storage_target_error(error, original_url))
}

fn azure_storage_target_error(error: crab_git::UrlError, original_url: &str) -> CrabError {
    match error {
        crab_git::UrlError::MissingAzureContainer { .. } => CrabError::Configuration {
            key: "replication.control_plane.azure.url".into(),
            origin: format!(
                "Azure replication URL {original_url} must use az://account/container/repo-prefix"
            ),
        },
        crab_git::UrlError::ExpectedAzureObjectUrl { .. } => CrabError::Configuration {
            key: "replication.control_plane.azure.url".into(),
            origin: format!("expected Azure URL for Azure replication, got {original_url}"),
        },
        other => CrabError::from(other),
    }
}

fn s3_role_state_matches(spec: &S3ReplicationRoleSpec, state: &S3ReplicationRoleState) -> bool {
    state.crab_managed
        && state.trust_policy_matches
        && state.policy_matches
        && !state.role_arn.is_empty()
        && state.source_bucket == spec.source_bucket
        && state.destination_bucket == spec.destination_bucket
}

fn s3_rule_state_matches(spec: &S3ReplicationRuleSpec, state: &S3ReplicationRuleState) -> bool {
    state.crab_managed
        && state.enabled
        && state.destination_bucket == spec.destination_bucket
        && state.destination_region == spec.destination_region
        && state.role_arn == spec.role_arn
        && state.rtc_enabled == spec.rtc_enabled
}

fn s3_batch_state_matches(spec: &S3BatchReplicationSpec, state: &S3BatchReplicationState) -> bool {
    state.crab_managed && state.destination_bucket == spec.destination_bucket && state.complete
}

fn gcs_bucket_has_replication_topology(state: &GcsBucketReplicationState) -> bool {
    matches!(
        state.location_type.to_ascii_uppercase().as_str(),
        "DUAL_REGION" | "MULTI_REGION"
    )
}

fn gcs_bucket_is_dual_region(state: &GcsBucketReplicationState) -> bool {
    state.location_type.eq_ignore_ascii_case("DUAL_REGION")
}

fn gcs_backfill_state_matches(
    spec: &GcsStorageTransferBackfillSpec,
    state: &GcsStorageTransferBackfillState,
) -> bool {
    state.crab_managed && state.destination_bucket == spec.destination_bucket && state.complete
}

fn gcs_backfill_progress_percent(state: &GcsStorageTransferBackfillState) -> Option<u8> {
    if state.complete {
        return Some(100);
    }
    let found = state.objects_found?;
    if found == 0 {
        return Some(0);
    }
    let copied_or_skipped = state
        .objects_copied
        .unwrap_or(0)
        .saturating_add(state.objects_skipped.unwrap_or(0));
    let percent = copied_or_skipped.saturating_mul(100) / found;
    Some(percent.min(100) as u8)
}

fn gcs_backfill_progress_detail(state: &GcsStorageTransferBackfillState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(found) = state.objects_found {
        let copied = state.objects_copied.unwrap_or(0);
        let skipped = state.objects_skipped.unwrap_or(0);
        let failed = state.objects_failed.unwrap_or(0);
        parts.push(format!(
            "objects copied {copied}/{found}, skipped {skipped}, failed {failed}"
        ));
    }
    if let Some(found) = state.bytes_found {
        let copied = state.bytes_copied.unwrap_or(0);
        let skipped = state.bytes_skipped.unwrap_or(0);
        let failed = state.bytes_failed.unwrap_or(0);
        parts.push(format!(
            "bytes copied {copied}/{found}, skipped {skipped}, failed {failed}"
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn gcs_backfill_remediation(
    state: ControlPlaneCheckState,
    job: Option<&GcsStorageTransferBackfillState>,
) -> String {
    if state == ControlPlaneCheckState::Drifted {
        return "repair the conflicting Storage Transfer job, then rerun crab replica add --apply"
            .to_owned();
    }
    if state == ControlPlaneCheckState::Verified {
        return "rerun crab replica verify --deep before enabling replica reads".to_owned();
    }
    if let Some(job) = job {
        let has_failures = job.objects_failed.unwrap_or(0) > 0
            || job.bytes_failed.unwrap_or(0) > 0
            || job.error_message.is_some();
        if has_failures {
            return "fix the Storage Transfer operation error and grant the service agent source object read/list plus destination object create/read/list permissions, then rerun crab replica add --apply and crab replica backfill status".to_owned();
        }
        return "wait for the Storage Transfer operation to finish; if it stalls, verify the service agent has source object read/list and destination object create/read/list permissions, then rerun crab replica backfill status".to_owned();
    }
    "run crab replica add --apply with GCS admin credentials to create and run the Storage Transfer backfill job; ensure the service agent has source object read/list and destination object create/read/list permissions".to_owned()
}

fn gcs_policy_validation_remediation(action: &str) -> &'static str {
    if action == "validate-replication-permissions" {
        return "grant the Crab operator identity Storage Transfer admin rights and grant the Storage Transfer service agent source object read/list plus destination object create/read/list permissions, then rerun crab replica doctor --deep";
    }
    "fix the provider policy finding, then rerun crab replica doctor --deep"
}

fn azure_object_policy_state_matches(
    spec: &AzureObjectReplicationPolicySpec,
    state: &AzureObjectReplicationPolicyState,
) -> bool {
    state.crab_managed && azure_object_policy_fields_match(spec, state)
}

fn azure_object_policy_fields_match(
    spec: &AzureObjectReplicationPolicySpec,
    state: &AzureObjectReplicationPolicyState,
) -> bool {
    state.enabled
        && state.source_account == spec.source_account
        && state.source_container == spec.source_container
        && state.destination_account == spec.destination_account
        && state.destination_container == spec.destination_container
        && state.destination_region == spec.destination_region
        && state.prefix_scope == spec.prefix_scope
        && state.priority == spec.priority
        && state.existing_blob_replication == spec.existing_blob_replication
}

fn azure_backfill_state_matches(
    spec: &AzureExistingBlobBackfillSpec,
    state: &AzureExistingBlobBackfillState,
) -> bool {
    state.crab_managed
        && state.destination_account == spec.destination_account
        && state.destination_container == spec.destination_container
        && state.complete
}

fn azure_backfill_progress_percent(state: &AzureExistingBlobBackfillState) -> u8 {
    if state.complete {
        return 100;
    }
    if state.objects_checked == 0 {
        return 0;
    }
    let replicated = state.objects_checked.saturating_sub(state.missing_objects);
    let percent = replicated.saturating_mul(100) / state.objects_checked;
    percent.min(100) as u8
}

fn azure_backfill_remediation(
    state: ControlPlaneCheckState,
    backfill: Option<&AzureExistingBlobBackfillState>,
) -> String {
    if state == ControlPlaneCheckState::Drifted {
        return "repair the conflicting Azure object replication policy, then rerun crab replica add --apply".to_owned();
    }
    if state == ControlPlaneCheckState::Verified {
        return "rerun crab replica verify --deep before enabling replica reads".to_owned();
    }
    if let Some(backfill) = backfill
        && backfill.missing_objects > 0
    {
        return "wait for Azure object replication to finish, inspect the first missing source blob's object replication status if progress stalls, and verify Crab can list the source container and HEAD the destination container before rerunning crab replica backfill status".to_owned();
    }
    "wait for Azure object replication to report completion, then rerun crab replica backfill status; Crab computes progress by listing source objects and HEAD-checking destination objects because Azure does not expose an aggregate percentage".to_owned()
}

#[cfg(any(feature = "replication-azure-control-plane", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExistingObjectBackfillVerification {
    objects_checked: u64,
    missing_objects: u64,
    first_missing: Option<String>,
}

#[cfg(any(feature = "replication-azure-control-plane", test))]
async fn verify_existing_object_backfill(
    source: &Store,
    destination: &Store,
    spec: &AzureExistingBlobBackfillSpec,
) -> Result<ExistingObjectBackfillVerification> {
    if spec.prefix_scope.is_empty() {
        return Ok(ExistingObjectBackfillVerification {
            objects_checked: 0,
            missing_objects: 1,
            first_missing: Some("root-prefix-inventory-required".to_owned()),
        });
    }
    if spec.prefix_scope.len() != spec.destination_prefix_scope.len() {
        return Err(CrabError::Configuration {
            key: "replication.control_plane.azure.backfill".into(),
            origin: "Azure backfill source and destination prefix scopes are inconsistent".into(),
        });
    }

    let mut objects_checked = 0_u64;
    let mut missing_objects = 0_u64;
    let mut first_missing = None;

    for source_prefix in &spec.prefix_scope {
        let prefix = ObjectPath::from(source_prefix.as_str());
        for meta in source.list_prefix(&prefix).await? {
            objects_checked = objects_checked.saturating_add(1);
            let source_key = meta.location.as_ref();
            let destination_key = backfill_destination_key(source_key, spec)?;
            let destination_path = ObjectPath::from(destination_key.as_str());
            match destination.head(&destination_path).await {
                Ok(_) => {}
                Err(CrabError::NotFound { .. }) => {
                    missing_objects = missing_objects.saturating_add(1);
                    if first_missing.is_none() {
                        first_missing = Some(destination_key);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(ExistingObjectBackfillVerification {
        objects_checked,
        missing_objects,
        first_missing,
    })
}

#[cfg(any(feature = "replication-azure-control-plane", test))]
fn backfill_destination_key(
    source_key: &str,
    spec: &AzureExistingBlobBackfillSpec,
) -> Result<String> {
    if source_key == ".crab" || source_key.starts_with(".crab/") {
        return Ok(source_key.to_owned());
    }

    for (source_prefix, destination_prefix) in spec
        .prefix_scope
        .iter()
        .zip(spec.destination_prefix_scope.iter())
    {
        if source_prefix == ".crab/" {
            continue;
        }
        let source_repo_prefix = source_prefix.trim_end_matches('/');
        let destination_repo_prefix = destination_prefix.trim_end_matches('/');
        if source_repo_prefix.is_empty() || destination_repo_prefix.is_empty() {
            continue;
        }
        if source_key == source_repo_prefix
            || source_key.starts_with(&format!("{source_repo_prefix}/"))
        {
            return repair_object_key_for_target_prefix(
                source_key,
                source_repo_prefix,
                destination_repo_prefix,
            );
        }
    }

    Err(CrabError::Configuration {
        key: "replication.control_plane.azure.backfill".into(),
        origin: format!(
            "Azure backfill source object {source_key} is outside the planned Crab prefix scope"
        ),
    })
}

fn gcs_pair_policy_state(
    source: ControlPlaneCheckState,
    destination: ControlPlaneCheckState,
) -> ControlPlaneCheckState {
    if source == ControlPlaneCheckState::Verified && destination == ControlPlaneCheckState::Verified
    {
        ControlPlaneCheckState::Verified
    } else if source == ControlPlaneCheckState::Missing
        || destination == ControlPlaneCheckState::Missing
    {
        ControlPlaneCheckState::Missing
    } else if source == ControlPlaneCheckState::Drifted
        || destination == ControlPlaneCheckState::Drifted
    {
        ControlPlaneCheckState::Drifted
    } else {
        ControlPlaneCheckState::Unsupported
    }
}

fn s3_control_plane_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
    region: &str,
    rpo: ReplicationRpo,
    backfill: bool,
) -> Vec<ControlPlaneRequest> {
    let mut requests = vec![
        control_request(
            ReplicationProviderKind::S3,
            "put-bucket-versioning",
            primary,
            managed_id(replica_name, "source-versioning"),
            false,
            serde_json::json!({"versioning": "Enabled", "ownership": ownership_json(replica_name)}),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "put-bucket-versioning",
            replica,
            managed_id(replica_name, "destination-versioning"),
            false,
            serde_json::json!({"versioning": "Enabled", "ownership": ownership_json(replica_name)}),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "create-replication-role",
            primary,
            managed_id(replica_name, "replication-role"),
            true,
            serde_json::json!({
                "role_name": managed_id(replica_name, "replication-role"),
                "policy_name": managed_id(replica_name, "replication-policy"),
                "source": primary,
                "destination": replica,
                "prefix_scope": ["{repo}/", ".crab/"],
                "permissions": [
                    "s3:GetReplicationConfiguration",
                    "s3:ListBucket",
                    "s3:GetObjectVersion",
                    "s3:GetObjectVersionAcl",
                    "s3:GetObjectVersionTagging",
                    "s3:ReplicateObject",
                    "s3:ReplicateDelete",
                    "s3:ReplicateTags"
                ],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "put-replication-configuration",
            primary,
            managed_id(replica_name, "replication-rule"),
            true,
            serde_json::json!({
                "destination": replica,
                "destination_region": region,
                "role_name": managed_id(replica_name, "replication-role"),
                "prefix_scope": ["{repo}/", ".crab/"],
                "replication_time_control": rpo == ReplicationRpo::Fast,
                "ownership": ownership_json(replica_name),
            }),
        ),
    ];
    requests.extend(s3_policy_validation_requests(
        replica_name,
        primary,
        replica,
    ));
    if backfill {
        requests.push(control_request(
            ReplicationProviderKind::S3,
            "create-batch-replication-job",
            primary,
            managed_id(replica_name, "batch-replication"),
            false,
            serde_json::json!({
                "destination": replica,
                "role_name": managed_id(replica_name, "replication-role"),
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ));
    }
    requests
}

fn gcs_control_plane_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
    _region: &str,
    rpo: ReplicationRpo,
    backfill: bool,
) -> Vec<ControlPlaneRequest> {
    let mut requests = vec![control_request(
        ReplicationProviderKind::Gcs,
        "validate-dual-region-replication",
        replica,
        managed_id(replica_name, "bucket-topology"),
        false,
        serde_json::json!({"primary": primary, "replica": replica, "ownership": ownership_json(replica_name)}),
    )];
    if rpo == ReplicationRpo::Fast {
        requests.push(control_request(
            ReplicationProviderKind::Gcs,
            "patch-bucket-rpo",
            replica,
            managed_id(replica_name, "async-turbo-rpo"),
            false,
            serde_json::json!({"rpo": "ASYNC_TURBO", "ownership": ownership_json(replica_name)}),
        ));
    }
    if backfill {
        requests.push(control_request(
            ReplicationProviderKind::Gcs,
            "create-storage-transfer-backfill-job",
            primary,
            managed_id(replica_name, "storage-transfer-backfill"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ));
    }
    requests.extend(gcs_policy_validation_requests(
        replica_name,
        primary,
        replica,
    ));
    requests
}

fn azure_control_plane_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
    region: &str,
    rpo: ReplicationRpo,
    backfill: bool,
) -> Vec<ControlPlaneRequest> {
    let mut requests = vec![
        control_request(
            ReplicationProviderKind::Azure,
            "enable-change-feed",
            primary,
            managed_id(replica_name, "source-change-feed"),
            false,
            serde_json::json!({"enabled": true, "ownership": ownership_json(replica_name)}),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "enable-blob-versioning",
            primary,
            managed_id(replica_name, "source-versioning"),
            false,
            serde_json::json!({"enabled": true, "ownership": ownership_json(replica_name)}),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "enable-blob-versioning",
            replica,
            managed_id(replica_name, "destination-versioning"),
            false,
            serde_json::json!({"enabled": true, "ownership": ownership_json(replica_name)}),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "put-object-replication-policy",
            primary,
            managed_id(replica_name, "object-replication-policy"),
            true,
            serde_json::json!({
                "destination": replica,
                "destination_region": region,
                "priority": rpo == ReplicationRpo::Fast,
                "existing_blob_replication": backfill,
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ),
    ];
    if backfill {
        requests.push(control_request(
            ReplicationProviderKind::Azure,
            "track-existing-blob-backfill",
            primary,
            managed_id(replica_name, "existing-blob-backfill"),
            false,
            serde_json::json!({
                "destination": replica,
                "copy_scope": "new-and-existing",
                "ownership": ownership_json(replica_name),
            }),
        ));
    }
    requests.extend(azure_policy_validation_requests(
        replica_name,
        primary,
        replica,
    ));
    requests
}

fn s3_policy_validation_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
) -> Vec<ControlPlaneRequest> {
    vec![
        control_request(
            ReplicationProviderKind::S3,
            "validate-replication-permissions",
            primary,
            managed_id(replica_name, "replication-permissions"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "required_for": ["replication-role", "bucket-policy", "object-lock-retention-reads"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-encryption-compatibility",
            primary,
            managed_id(replica_name, "encryption-compatibility"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "requires_kms_grants": true,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-lifecycle-retention-policy",
            primary,
            managed_id(replica_name, "lifecycle-retention"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-immutability-policy",
            primary,
            managed_id(replica_name, "object-lock"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "checks": ["object-lock", "legal-hold"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-public-access-policy",
            primary,
            managed_id(replica_name, "public-access"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-requester-pays",
            primary,
            managed_id(replica_name, "requester-pays"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::S3,
            "validate-cross-account-ownership",
            primary,
            managed_id(replica_name, "cross-account-ownership"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
    ]
}

fn gcs_policy_validation_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
) -> Vec<ControlPlaneRequest> {
    vec![
        control_request(
            ReplicationProviderKind::Gcs,
            "validate-replication-permissions",
            primary,
            managed_id(replica_name, "storage-transfer-permissions"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "required_for": ["storage-transfer-service", "pubsub-notifications"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Gcs,
            "validate-encryption-compatibility",
            primary,
            managed_id(replica_name, "cmek-compatibility"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "requires_cmek_grants": true,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Gcs,
            "validate-lifecycle-retention-policy",
            primary,
            managed_id(replica_name, "lifecycle-retention"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Gcs,
            "validate-public-access-policy",
            primary,
            managed_id(replica_name, "public-access"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Gcs,
            "validate-requester-pays",
            primary,
            managed_id(replica_name, "requester-pays"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
    ]
}

fn azure_policy_validation_requests(
    replica_name: &str,
    primary: &str,
    replica: &str,
) -> Vec<ControlPlaneRequest> {
    vec![
        control_request(
            ReplicationProviderKind::Azure,
            "validate-replication-permissions",
            primary,
            managed_id(replica_name, "replication-rbac"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "required_for": ["source-policy", "destination-policy"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "validate-encryption-compatibility",
            primary,
            managed_id(replica_name, "cmk-compatibility"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "requires_cmk_grants": true,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "validate-lifecycle-retention-policy",
            primary,
            managed_id(replica_name, "lifecycle-retention"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "prefix_scope": ["{repo}/", ".crab/"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "validate-immutability-policy",
            primary,
            managed_id(replica_name, "immutability-policy"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "checks": ["immutability-policy", "legal-hold"],
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "validate-public-access-policy",
            primary,
            managed_id(replica_name, "public-access"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
        control_request(
            ReplicationProviderKind::Azure,
            "validate-cross-tenant-replication",
            primary,
            managed_id(replica_name, "cross-tenant-replication"),
            false,
            serde_json::json!({
                "source": primary,
                "destination": replica,
                "ownership": ownership_json(replica_name),
            }),
        ),
    ]
}

fn control_request(
    provider: ReplicationProviderKind,
    action: &str,
    target: &str,
    managed_resource_id: String,
    reversible: bool,
    request: serde_json::Value,
) -> ControlPlaneRequest {
    ControlPlaneRequest {
        provider,
        action: action.to_owned(),
        target: target.to_owned(),
        request,
        reversible,
        managed_resource_id,
    }
}

fn indent_block(value: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Validate that a replica URL's raw cloud scheme matches its provider.
///
/// `crab://` replica URLs are allowed because the cloud cannot be inferred
/// from that scheme alone; the explicit provider selects the backend.
pub fn validate_replica_url_provider(
    provider: ReplicationProviderKind,
    replica_url: &str,
) -> Result<()> {
    let parsed = ObjectUrl::parse(replica_url)?;
    validate_replica_object_url_provider(provider, &parsed)
}

fn validate_replica_object_url_provider(
    provider: ReplicationProviderKind,
    parsed: &ObjectUrl,
) -> Result<()> {
    crab_storage::validate_static_env_url_provider(
        static_env_store_url_parts(parsed),
        provider.storage_provider_kind(),
    )
    .map_err(|error| replica_provider_error(error, provider, parsed))
}

fn replica_provider_error(
    error: crab_storage::StorageError,
    provider: ReplicationProviderKind,
    parsed: &ObjectUrl,
) -> CrabError {
    match error {
        crab_storage::StorageError::StaticEnvProviderMismatch { .. } => CrabError::Configuration {
            key: "replication.replica.url".into(),
            origin: format!(
                "provider {provider} does not match replica URL scheme for {}",
                parsed.bucket
            ),
        },
        other => CrabError::from(other),
    }
}

fn s3_plan(rpo: ReplicationRpo, backfill: bool) -> Vec<ReplicationAction> {
    let mut actions = vec![
        action(
            "Enable S3 versioning on primary and replica buckets",
            true,
            false,
        ),
        action(
            "Create or verify the S3 replication IAM role and bucket policy",
            true,
            false,
        ),
        action(
            "Install a bucket-scope S3 replication rule covering repo prefixes and .crab/",
            true,
            false,
        ),
        action(
            "Validate S3 encryption, lifecycle, Object Lock, public access, requester-pays, and ownership policy compatibility",
            true,
            false,
        ),
    ];
    if rpo == ReplicationRpo::Fast {
        actions.push(action(
            "Enable S3 Replication Time Control and metrics",
            false,
            false,
        ));
    }
    if backfill {
        actions.push(action(
            "Start S3 Batch Replication for existing objects",
            false,
            false,
        ));
    }
    actions
}

fn gcs_plan(rpo: ReplicationRpo, backfill: bool) -> Vec<ReplicationAction> {
    let mut actions = vec![
        action(
            "Verify the GCS replica bucket is dual-region or otherwise provider-replicated",
            true,
            false,
        ),
        action(
            "Verify Crab bucket-scope prefixes are covered by the bucket replication policy",
            true,
            false,
        ),
        action(
            "Validate GCS IAM, CMEK, lifecycle/retention, public access, and requester-pays compatibility",
            true,
            false,
        ),
    ];
    if rpo == ReplicationRpo::Fast {
        actions.push(action(
            "Set the GCS bucket RPO to ASYNC_TURBO",
            false,
            false,
        ));
    }
    if backfill {
        actions.push(action(
            "Start or verify a Storage Transfer Service backfill for existing Crab objects",
            false,
            false,
        ));
    }
    actions
}

fn azure_plan(rpo: ReplicationRpo, backfill: bool) -> Vec<ReplicationAction> {
    let mut actions = vec![
        action(
            "Enable Azure Blob change feed on the source storage account",
            true,
            false,
        ),
        action(
            "Enable blob versioning on source and destination storage accounts",
            true,
            false,
        ),
        action(
            "Create an object replication policy covering repo prefixes and .crab/",
            true,
            false,
        ),
        action(
            "Validate Azure RBAC, customer-managed keys, lifecycle/retention, immutability, public access, and cross-tenant policy compatibility",
            true,
            false,
        ),
    ];
    if rpo == ReplicationRpo::Fast {
        actions.push(action(
            "Enable Azure Object Replication priority replication when available",
            false,
            false,
        ));
    }
    if backfill {
        actions.push(action(
            "Track existing blob replication until pre-existing Crab objects are copied",
            false,
            false,
        ));
    }
    actions
}

fn action(description: &str, required: bool, automated: bool) -> ReplicationAction {
    ReplicationAction {
        description: description.to_owned(),
        required,
        automated,
    }
}

/// Status for a replica relative to the primary manifest.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicaStatus {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub url: String,
    pub region: String,
    pub backfill_required: bool,
    pub read_enabled: bool,
    pub primary_generation: Option<u64>,
    pub replica_generation: Option<u64>,
    pub ready: bool,
    pub lag_generations: Option<u64>,
    pub last_fallback_reason: Option<String>,
    pub last_fallback_class: Option<ReplicaFallbackClass>,
    pub last_fallback_at_ms: Option<u64>,
    pub last_fallback_operation: Option<String>,
    pub fallback_count: u64,
    pub primary_fallback_bytes: u64,
    pub last_selected_at_ms: Option<u64>,
    pub last_selected_operation: Option<String>,
    pub selected_count: u64,
    pub readiness_cache_hit: bool,
    pub readiness_cache_age_ms: Option<u64>,
    pub readiness_check_latency_ms: Option<u64>,
    pub readiness_object_probe_count: u64,
    pub readiness_object_read_count: u64,
}

/// Stable class for alerting on the latest primary fallback reason.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReplicaFallbackClass {
    Auth,
    ClientUnavailable,
    MissingObject,
    PolicyDrift,
    ReadinessFailed,
    StaleManifest,
    Unknown,
}

impl ReplicaFallbackClass {
    pub const ALL: [Self; 7] = [
        Self::Auth,
        Self::ClientUnavailable,
        Self::MissingObject,
        Self::PolicyDrift,
        Self::ReadinessFailed,
        Self::StaleManifest,
        Self::Unknown,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::ClientUnavailable => "client-unavailable",
            Self::MissingObject => "missing-object",
            Self::PolicyDrift => "policy-drift",
            Self::ReadinessFailed => "readiness-failed",
            Self::StaleManifest => "stale-manifest",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn from_reason(reason: Option<&str>) -> Self {
        let Some(reason) = reason else {
            return Self::Unknown;
        };
        let lower = reason.to_ascii_lowercase();
        if contains_any(
            &lower,
            &[
                "auth",
                "unauthorized",
                "forbidden",
                "permission",
                "credential",
                "access denied",
            ],
        ) {
            Self::Auth
        } else if contains_any(&lower, &["client unavailable", "failed to build"]) {
            Self::ClientUnavailable
        } else if contains_any(&lower, &["policy", "drift"]) {
            Self::PolicyDrift
        } else if lower.contains("manifest is stale") {
            Self::StaleManifest
        } else if contains_any(
            &lower,
            &[
                "missing",
                "not found",
                "referenced object",
                "pack index",
                "shard index",
            ],
        ) {
            Self::MissingObject
        } else if contains_any(&lower, &["readiness failed", "readiness failure"]) {
            Self::ReadinessFailed
        } else {
            Self::Unknown
        }
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

pub(crate) fn readiness_check_options_from_env() -> Result<ReadinessCheckOptions> {
    let mut options = ReadinessCheckOptions::default();
    if let Some(no_cache) = optional_env_bool(READINESS_NO_CACHE_ENV)? {
        options.bypass_cache = no_cache;
    }
    if let Some(ttl_ms) = optional_env_u64(READINESS_CACHE_TTL_MS_ENV)? {
        options.cache_ttl_ms = ttl_ms;
        if ttl_ms == 0 {
            options.bypass_cache = true;
        }
    }
    Ok(options)
}

fn optional_env_bool(name: &str) -> Result<Option<bool>> {
    match std::env::var(name) {
        Ok(value) => parse_env_bool(name, &value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CrabError::Configuration {
            key: name.into(),
            origin: "environment value must be valid UTF-8".into(),
        }),
    }
}

fn parse_env_bool(name: &str, value: &str) -> Result<bool> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(CrabError::Configuration {
            key: name.into(),
            origin: format!(
                "expected boolean value 1/0, true/false, yes/no, or on/off, got {value}"
            ),
        }),
    }
}

fn optional_env_u64(name: &str) -> Result<Option<u64>> {
    match std::env::var(name) {
        Ok(value) => parse_env_u64(name, &value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CrabError::Configuration {
            key: name.into(),
            origin: "environment value must be valid UTF-8".into(),
        }),
    }
}

fn parse_env_u64(name: &str, value: &str) -> Result<u64> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|err| CrabError::Configuration {
            key: name.into(),
            origin: format!("expected non-negative integer milliseconds, got {value}: {err}"),
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReplicaReadEvent {
    version: u32,
    timestamp_ms: u64,
    replica_name: String,
    provider: ReplicationProviderKind,
    url: String,
    region: String,
    repo_prefix: String,
    operation: String,
    outcome: ReplicaReadOutcome,
    primary_generation: Option<u64>,
    replica_generation: Option<u64>,
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    primary_fallback_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ReplicaReadOutcome {
    Selected,
    Fallback,
    PrimaryFallbackRead,
}

/// Active-active write admission status.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveStatus {
    pub mode: ReplicationMode,
    pub coordinator_configured: bool,
    pub coordinator_ready: bool,
    pub writes_enabled: bool,
    pub enabled_writers: usize,
    pub reason: Option<String>,
}

/// Active-active push request prepared for a managed coordinator.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActivePushPlan {
    pub writer: WriterConfig,
    pub coordinator_url: String,
    pub request: CommitRequest,
}

/// Coordinator proof collected before a destructive bucket-scope GC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveActiveBucketGcProtection {
    pub protected_keys: HashSet<String>,
    pub protected_repos: HashSet<String>,
}

/// One regional manifest repair action derived from coordinator truth.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveRepairAction {
    pub operation_id: String,
    pub manifest_generation: u64,
    pub region: String,
    pub writer: WriterConfig,
    pub source_region: String,
    pub refs: Vec<CoordinatedRefUpdate>,
    pub uploaded_objects: Vec<String>,
}

/// Repair plan for committed active-active transactions not materialized everywhere.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveRepairPlan {
    pub coordinator_epoch: u64,
    pub actions: Vec<ActiveActiveRepairAction>,
}

/// Coordinator failover operation applied to active-active write admission.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ActiveActiveFailoverOperation {
    Fence,
    Resume,
}

impl ActiveActiveFailoverOperation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fence => "fence",
            Self::Resume => "resume",
        }
    }
}

/// Result of a Crab-owned active-active failover operation.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActiveActiveFailoverOutcome {
    pub operation: ActiveActiveFailoverOperation,
    pub provider: ManagedCoordinatorProvider,
    pub coordinator_url: String,
    pub repo_prefix: String,
    pub previous_epoch: u64,
    pub coordinator_epoch: u64,
    pub previous_healthy: bool,
    pub healthy: bool,
    pub changed: bool,
    pub reason: Option<String>,
}

/// Operator proof required before re-admitting active-active writes after fencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveActiveResumeProof {
    repair_verified: bool,
}

impl ActiveActiveResumeProof {
    /// Confirms coordinator-backed repair and external failover checks completed.
    #[must_use]
    pub fn verified_after_repair() -> Self {
        Self {
            repair_verified: true,
        }
    }
}

/// Validate active-active configuration shape.
///
/// This only validates Crab's config contract. A concrete coordinator
/// adapter must still prove the backend is linearizable before writes run.
pub fn validate_active_active_config(replication: &ReplicationConfig) -> Result<()> {
    coordination_active_active::validate_active_active_config(&active_active_coordination_config(
        replication,
    ))
    .map_err(CrabError::from)
}

/// Build the coordinator commit request for an active-active push.
pub fn plan_active_active_push(
    replication: &ReplicationConfig,
    preferred_writer: Option<&str>,
    manifest_generation: u64,
    refs: Vec<CoordinatedRefUpdate>,
    uploaded_objects: Vec<String>,
) -> Result<ActiveActivePushPlan> {
    let plan = coordination_active_active::plan_active_active_push(
        &active_active_coordination_config(replication),
        preferred_writer,
        manifest_generation,
        refs,
        uploaded_objects,
    )
    .map_err(CrabError::from)?;
    Ok(ActiveActivePushPlan {
        writer: writer_from_coordination(plan.writer),
        coordinator_url: plan.coordinator_url,
        request: plan.request,
    })
}

/// Plan regional manifest repairs from a coordinator snapshot.
pub fn plan_active_active_repair(
    replication: &ReplicationConfig,
    snapshot: &CoordinatorRepairSnapshot,
) -> Result<ActiveActiveRepairPlan> {
    let plan = coordination_active_active::plan_active_active_repair(
        &active_active_coordination_config(replication),
        snapshot,
    )
    .map_err(CrabError::from)?;
    Ok(active_active_repair_plan_from_coordination(plan))
}

fn active_active_repair_plan_from_coordination(
    plan: coordination_active_active::ActiveActiveRepairPlan,
) -> ActiveActiveRepairPlan {
    ActiveActiveRepairPlan {
        coordinator_epoch: plan.coordinator_epoch,
        actions: plan
            .actions
            .into_iter()
            .map(|action| ActiveActiveRepairAction {
                operation_id: action.operation_id,
                manifest_generation: action.manifest_generation,
                region: action.region,
                writer: writer_from_coordination(action.writer),
                source_region: action.source_region,
                refs: action.refs,
                uploaded_objects: action.uploaded_objects,
            })
            .collect(),
    }
}

/// Select the active-active writer whose URL matches the push remote.
pub fn active_active_writer_name_for_remote(
    replication: &ReplicationConfig,
    remote_url: Option<&str>,
) -> Result<String> {
    coordination_active_active::active_active_writer_name_for_remote(
        &active_active_coordination_config(replication),
        remote_url,
    )
    .map_err(CrabError::from)
}

/// Converts CLI replication config into the coordination-domain active-active contract.
#[must_use]
pub fn active_active_coordination_config(
    replication: &ReplicationConfig,
) -> coordination_active_active::ActiveActiveReplicationConfig {
    coordination_active_active::ActiveActiveReplicationConfig {
        mode: match replication.mode {
            ReplicationMode::ReadReplica => {
                coordination_active_active::ActiveActiveMode::ReadReplica
            }
            ReplicationMode::ActiveActive => {
                coordination_active_active::ActiveActiveMode::ActiveActive
            }
        },
        coordinator: replication.coordinator.as_ref().map(|coordinator| {
            coordination_active_active::ActiveActiveCoordinatorConfig {
                kind: match coordinator.kind {
                    ReplicationCoordinatorKind::Managed => {
                        coordination_active_active::ActiveActiveCoordinatorKind::Managed
                    }
                },
                url: coordinator.url.clone(),
                region: coordinator.region.clone(),
                failover_regions: coordinator.failover_regions.clone(),
                consistency: match coordinator.consistency {
                    ReplicationCoordinatorConsistency::Linearizable => {
                        coordination_active_active::ActiveActiveCoordinatorConsistency::Linearizable
                    }
                },
            }
        }),
        writers: replication
            .writers
            .iter()
            .map(coordination_writer_config)
            .collect(),
    }
}

/// Converts the coordination-domain active-active contract into CLI replication config.
#[must_use]
pub fn replication_config_from_active_active_coordination(
    replication: &coordination_active_active::ActiveActiveReplicationConfig,
) -> ReplicationConfig {
    ReplicationConfig {
        primary: None,
        mode: match replication.mode {
            coordination_active_active::ActiveActiveMode::ReadReplica => {
                ReplicationMode::ReadReplica
            }
            coordination_active_active::ActiveActiveMode::ActiveActive => {
                ReplicationMode::ActiveActive
            }
        },
        coordinator: replication.coordinator.as_ref().map(|coordinator| {
            ReplicationCoordinatorConfig {
                kind: match coordinator.kind {
                    coordination_active_active::ActiveActiveCoordinatorKind::Managed => {
                        ReplicationCoordinatorKind::Managed
                    }
                },
                url: coordinator.url.clone(),
                region: coordinator.region.clone(),
                failover_regions: coordinator.failover_regions.clone(),
                consistency: match coordinator.consistency {
                    coordination_active_active::ActiveActiveCoordinatorConsistency::Linearizable => {
                        ReplicationCoordinatorConsistency::Linearizable
                    }
                },
            }
        }),
        writers: replication
            .writers
            .iter()
            .map(|writer| WriterConfig {
                name: writer.name.clone(),
                url: writer.url.clone(),
                region: writer.region.clone(),
                enabled: writer.enabled,
            })
            .collect(),
        replicas: Vec::new(),
    }
}

fn coordination_writer_config(
    writer: &WriterConfig,
) -> coordination_active_active::ActiveActiveWriterConfig {
    coordination_active_active::ActiveActiveWriterConfig {
        name: writer.name.clone(),
        url: writer.url.clone(),
        region: writer.region.clone(),
        enabled: writer.enabled,
    }
}

fn writer_from_coordination(
    writer: coordination_active_active::ActiveActiveWriterConfig,
) -> WriterConfig {
    WriterConfig {
        name: writer.name,
        url: writer.url,
        region: writer.region,
        enabled: writer.enabled,
    }
}

/// Summarize active-active readiness from local configuration.
#[must_use]
pub fn active_active_status(replication: Option<&ReplicationConfig>) -> ActiveActiveStatus {
    active_active_status_with_coordinator_status(replication, None)
}

/// Summarize active-active readiness from local config and coordinator status.
#[must_use]
pub fn active_active_status_with_coordinator_status(
    replication: Option<&ReplicationConfig>,
    coordinator_status: Option<&CoordinatorControlPlaneStatus>,
) -> ActiveActiveStatus {
    let Some(replication) = replication else {
        return ActiveActiveStatus {
            mode: ReplicationMode::ReadReplica,
            coordinator_configured: false,
            coordinator_ready: false,
            writes_enabled: false,
            enabled_writers: 0,
            reason: Some("replication is not configured".to_owned()),
        };
    };

    let enabled_writers = replication
        .writers
        .iter()
        .filter(|writer| writer.enabled)
        .count();
    if !replication.is_active_active() {
        return ActiveActiveStatus {
            mode: replication.mode,
            coordinator_configured: replication.coordinator.is_some(),
            coordinator_ready: false,
            writes_enabled: false,
            enabled_writers,
            reason: Some("replication mode is read-replica".to_owned()),
        };
    }

    match validate_active_active_config(replication) {
        Ok(()) => {
            let Some(status) = coordinator_status else {
                return ActiveActiveStatus {
                    mode: replication.mode,
                    coordinator_configured: true,
                    coordinator_ready: false,
                    writes_enabled: false,
                    enabled_writers,
                    reason: Some(
                        "managed coordinator adapter is not configured; writes fail closed"
                            .to_owned(),
                    ),
                };
            };
            if let Some(reason) = coordinator_status_config_mismatch(replication, status) {
                return ActiveActiveStatus {
                    mode: replication.mode,
                    coordinator_configured: true,
                    coordinator_ready: false,
                    writes_enabled: false,
                    enabled_writers,
                    reason: Some(reason),
                };
            }
            match validate_coordinator_write_admission(status) {
                Ok(()) => ActiveActiveStatus {
                    mode: replication.mode,
                    coordinator_configured: true,
                    coordinator_ready: true,
                    writes_enabled: true,
                    enabled_writers,
                    reason: None,
                },
                Err(e) => ActiveActiveStatus {
                    mode: replication.mode,
                    coordinator_configured: true,
                    coordinator_ready: false,
                    writes_enabled: false,
                    enabled_writers,
                    reason: Some(e.to_string()),
                },
            }
        }
        Err(e) => ActiveActiveStatus {
            mode: replication.mode,
            coordinator_configured: replication.coordinator.is_some(),
            coordinator_ready: false,
            writes_enabled: false,
            enabled_writers,
            reason: Some(e.to_string()),
        },
    }
}

fn coordinator_status_config_mismatch(
    replication: &ReplicationConfig,
    status: &CoordinatorControlPlaneStatus,
) -> Option<String> {
    let coordinator = replication.coordinator.as_ref()?;
    if let Some(expected_provider) = managed_coordinator_provider_from_url(&coordinator.url)
        && status.provider != expected_provider
    {
        return Some(format!(
            "coordinator status provider {} does not match configured coordinator URL {}",
            status.provider.as_str(),
            coordinator.url
        ));
    }
    if status.url != coordinator.url {
        return Some(format!(
            "coordinator status URL {} does not match configured {}",
            status.url, coordinator.url
        ));
    }
    if status.region != coordinator.region {
        return Some(format!(
            "coordinator status region {} does not match configured {}",
            status.region, coordinator.region
        ));
    }
    let configured_failover = coordinator
        .failover_regions
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let status_failover = status
        .failover_regions
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if status_failover != configured_failover {
        return Some(
            "coordinator status failover regions do not match configured failover regions"
                .to_owned(),
        );
    }
    None
}

fn managed_coordinator_provider_from_url(url: &str) -> Option<ManagedCoordinatorProvider> {
    coordination_active_active::active_active_coordinator_resource(url)
        .ok()
        .map(|resource| resource.provider)
}

/// Refuse active-active writes unless a concrete coordinator backend is wired.
pub fn ensure_active_active_write_admitted(config: &Config) -> Result<()> {
    ensure_active_active_mutation_admitted(config, "write admission")
}

/// Admit active-active writes only after a verified coordinator status check.
pub fn ensure_active_active_write_admitted_with_coordinator_status(
    config: &Config,
    coordinator_status: &CoordinatorControlPlaneStatus,
) -> Result<()> {
    ensure_active_active_mutation_admitted_with_status(
        config,
        "write admission",
        Some(coordinator_status),
    )
}

/// Refuse active-active maintenance that can mutate object storage or refs.
pub fn ensure_active_active_maintenance_admitted(config: &Config, operation: &str) -> Result<()> {
    ensure_active_active_mutation_admitted(config, operation)
}

/// Admit active-active maintenance only after a verified coordinator status check.
pub fn ensure_active_active_maintenance_admitted_with_coordinator_status(
    config: &Config,
    operation: &str,
    coordinator_status: &CoordinatorControlPlaneStatus,
) -> Result<()> {
    ensure_active_active_mutation_admitted_with_status(config, operation, Some(coordinator_status))
}

/// Return coordinator-owned objects that active-active maintenance must retain.
pub async fn active_active_gc_protected_keys(
    config: &Config,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    let Some(replication) = config.replication.as_ref() else {
        return Ok(HashSet::new());
    };
    if !replication.is_active_active() {
        return Ok(HashSet::new());
    }
    validate_active_active_config(replication)?;

    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active repo garbage collection requires a managed coordinator".into(),
        })?;
    let target = active_active_coordinator_resource(&coordinator.url)?;
    match target.provider {
        ManagedCoordinatorProvider::DynamoDb => {
            dynamodb_active_active_gc_protected_keys(config, coordinator, &target.name, repo_prefix)
                .await
        }
        ManagedCoordinatorProvider::Spanner => {
            spanner_active_active_gc_protected_keys(config, coordinator, &target.name, repo_prefix)
                .await
        }
        ManagedCoordinatorProvider::CosmosDb => {
            cosmosdb_active_active_gc_protected_keys(config, coordinator, &target.name, repo_prefix)
                .await
        }
    }
}

/// Coordinator fence held while a repo-scope GC run seals and deletes a batch.
///
/// A protected-key snapshot without this authority fence is only advisory:
/// another coordinator transaction could upload and commit between the read
/// and the object-store delete. The guard leaves a pre-fenced coordinator
/// untouched and deliberately requires operator resume after process loss.
pub struct ActiveActiveGcFence {
    coordinator: Arc<dyn WriteCoordinator>,
    repo_prefix: String,
    previous_healthy: bool,
    protected_keys: HashSet<String>,
    coordinator_epoch: u64,
}

impl ActiveActiveGcFence {
    /// Fences the configured coordinator and captures its protected objects.
    pub async fn acquire(config: &Config, repo_prefix: &str) -> Result<Option<Self>> {
        let Some(replication) = config.replication.as_ref() else {
            return Ok(None);
        };
        if !replication.is_active_active() {
            return Ok(None);
        }
        validate_active_active_config(replication)?;
        let coordinator = active_active_write_coordinator_for_repo(config, repo_prefix).await?;
        let health = coordinator.health().await.map_err(CrabError::from)?;
        if !health.healthy || !health.linearizable {
            return Err(CrabError::Configuration {
                key: "replication.coordinator".to_owned(),
                origin: health.reason.unwrap_or_else(|| {
                    "active-active coordinator is not healthy and linearizable for GC fencing"
                        .to_owned()
                }),
            });
        }
        let fence = coordinator
            .fence_writes(Some("repo garbage collection".to_owned()))
            .await
            .map_err(CrabError::from)?;
        let snapshot = match coordinator.gc_safety_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if fence.previous_healthy {
                    let _ = coordinator.resume_writes().await;
                }
                return Err(CrabError::from(error));
            }
        };
        Ok(Some(Self {
            coordinator,
            repo_prefix: repo_prefix.to_owned(),
            previous_healthy: fence.previous_healthy,
            protected_keys: snapshot.protected_keys(),
            coordinator_epoch: snapshot.coordinator_epoch,
        }))
    }

    /// Returns the fenced coordinator's protected object set.
    #[must_use]
    pub fn protected_keys(&self) -> &HashSet<String> {
        &self.protected_keys
    }

    /// Returns the epoch captured after fencing.
    #[must_use]
    pub fn coordinator_epoch(&self) -> u64 {
        self.coordinator_epoch
    }

    /// Resumes writes only when this guard created the fence.
    pub async fn release(self) -> Result<()> {
        if !self.previous_healthy {
            return Ok(());
        }
        self.coordinator
            .resume_writes()
            .await
            .map(|_| ())
            .map_err(|error| CrabError::Configuration {
                key: "replication.coordinator".to_owned(),
                origin: format!(
                    "failed to resume coordinator for repo {} after GC: {error}",
                    self.repo_prefix
                ),
            })
    }
}

/// Collect coordinator-owned objects for every active-active repo in a bucket.
pub async fn active_active_bucket_gc_protection(
    config: &Config,
    registry: &RefRegistry,
    current_repo_prefix: Option<&str>,
) -> Result<ActiveActiveBucketGcProtection> {
    validate_active_active_bucket_gc_current_repo(config, registry, current_repo_prefix)?;

    let mut protection = ActiveActiveBucketGcProtection::default();
    let mut repos = registry
        .active_active_coordinators
        .iter()
        .collect::<Vec<_>>();
    repos.sort_by(|left, right| left.0.cmp(right.0));

    for (repo_prefix, registration) in repos {
        let keys =
            active_active_gc_protected_keys_for_registration(repo_prefix, registration).await?;
        protection.protected_keys.extend(keys);
        protection.protected_repos.insert(repo_prefix.clone());
    }
    Ok(protection)
}

fn validate_active_active_bucket_gc_current_repo(
    config: &Config,
    registry: &RefRegistry,
    current_repo_prefix: Option<&str>,
) -> Result<()> {
    let Some(replication) = config.replication.as_ref() else {
        return Ok(());
    };
    if !replication.is_active_active() {
        return Ok(());
    }
    validate_active_active_config(replication)?;

    let repo_prefix = current_repo_prefix.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "active-active bucket garbage collection requires a configured primary remote so Crab can verify the current repo's coordinator registration".into(),
    })?;
    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active bucket garbage collection requires a managed coordinator".into(),
        })?;
    let expected = active_active_coordinator_registration(coordinator)?;
    let Some(registered) = registry.active_active_coordinators.get(repo_prefix) else {
        return Err(CrabError::Configuration {
            key: "gc.bucket.active_active_registration".into(),
            origin: format!(
                "active-active bucket garbage collection requires repo {repo_prefix} to register its coordinator in .crab/ref-registry before deleting shared .crab/ objects"
            ),
        });
    };
    if registered == &expected {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_registration".into(),
        origin: format!(
            "active-active bucket garbage collection found coordinator registration for repo {repo_prefix} that does not match local replication config"
        ),
    })
}

fn active_active_coordinator_registration(
    coordinator: &ReplicationCoordinatorConfig,
) -> Result<ActiveActiveCoordinatorRegistration> {
    let target = active_active_coordinator_resource(&coordinator.url)?;
    Ok(ActiveActiveCoordinatorRegistration {
        provider: target.provider.as_str().to_owned(),
        url: coordinator.url.clone(),
        region: coordinator.region.clone(),
        failover_regions: coordinator.failover_regions.clone(),
    })
}

/// Registers the configured active-active coordinator so bucket-scope GC can protect this repo.
pub async fn register_active_active_coordinator_for_repo(
    store: &Store,
    router: &StoreLayout,
    replication: &ReplicationConfig,
) -> Result<()> {
    if !replication.is_active_active() {
        return Ok(());
    }
    validate_active_active_config(replication)?;
    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active coordinator registration requires a managed coordinator".into(),
        })?;
    let registration = active_active_coordinator_registration(coordinator)?;
    let metadata_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    crab_metadata::ref_registry::register_active_active_coordinator_for_repo(
        store.as_storage(),
        &metadata_router,
        registration,
    )
    .await
    .map_err(CrabError::from)
}

/// Registers an active-active coordinator from the coordination-domain config contract.
pub async fn register_active_active_coordinator_for_repo_from_coordination_config(
    store: &Store,
    router: &StoreLayout,
    replication: &coordination_active_active::ActiveActiveReplicationConfig,
) -> Result<()> {
    let replication = replication_config_from_active_active_coordination(replication);
    register_active_active_coordinator_for_repo(store, router, &replication).await
}

/// Builds a live write coordinator after provider control-plane admission proof.
pub async fn active_active_write_coordinator_for_repo(
    config: &Config,
    repo_prefix: &str,
) -> Result<Arc<dyn WriteCoordinator>> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "active-active writes require replication config".into(),
        })?;
    crab_coordination::active_active_write_coordinator_for_repo(
        &active_active_coordination_config(replication),
        repo_prefix,
    )
    .await
    .map_err(CrabError::from)
}

/// Builds a live write coordinator from the coordination-domain config contract.
pub async fn active_active_write_coordinator_for_repo_from_coordination_config(
    replication: &coordination_active_active::ActiveActiveReplicationConfig,
    repo_prefix: &str,
) -> Result<Arc<dyn WriteCoordinator>> {
    crab_coordination::active_active_write_coordinator_for_repo(replication, repo_prefix)
        .await
        .map_err(CrabError::from)
}

async fn active_active_gc_protected_keys_for_registration(
    repo_prefix: &str,
    registration: &ActiveActiveCoordinatorRegistration,
) -> Result<HashSet<String>> {
    let target = active_active_coordinator_resource(&registration.url)?;
    if registration.provider != target.provider.as_str() {
        return Err(CrabError::Configuration {
            key: "gc.bucket.active_active_registration".into(),
            origin: format!(
                "active-active coordinator registration for repo {repo_prefix} declares provider {} but URL {} resolves to {}",
                registration.provider,
                registration.url,
                target.provider.as_str()
            ),
        });
    }

    match target.provider {
        ManagedCoordinatorProvider::DynamoDb => {
            dynamodb_active_active_gc_protected_keys_for_registration(
                registration,
                &target.name,
                repo_prefix,
            )
            .await
        }
        ManagedCoordinatorProvider::Spanner => {
            spanner_active_active_gc_protected_keys_for_registration(
                registration,
                &target.name,
                repo_prefix,
            )
            .await
        }
        ManagedCoordinatorProvider::CosmosDb => {
            cosmosdb_active_active_gc_protected_keys_for_registration(
                registration,
                &target.name,
                repo_prefix,
            )
            .await
        }
    }
}

fn validate_coordinator_gc_snapshot_admission(
    status: &CoordinatorControlPlaneStatus,
) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} coordinator {} cannot provide GC safety snapshots; bucket maintenance fails closed until control-plane drift is verified",
                status.provider.as_str(),
                status.name
            ),
        });
    }
    if let Some(check) = status
        .checks
        .iter()
        .find(|check| check.state != CoordinatorCheckState::Verified)
    {
        return Err(CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} is {}; bucket maintenance fails closed until every coordinator resource is verified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

fn coordinator_gc_snapshot_status_error(
    provider: ManagedCoordinatorProvider,
    name: &str,
    scope: &str,
    err: CrabError,
) -> CrabError {
    CrabError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "{} coordinator {name} cannot provide GC safety snapshots; {scope} fails closed until control-plane drift is verified: {err}",
            provider.as_str(),
        ),
    }
}

/// Build a regional manifest repair plan from coordinator truth.
pub async fn active_active_repair_plan_from_coordinator(
    config: &Config,
    repo_prefix: &str,
) -> Result<ActiveActiveRepairPlan> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "coordinator-backed repair requires active-active replication config".into(),
        })?;
    if !replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "coordinator-backed repair is only valid in active-active mode".into(),
        });
    }
    validate_active_active_config(replication)?;

    let plan = crab_coordination::active_active_repair_plan_from_coordinator(
        &active_active_coordination_config(replication),
        repo_prefix,
    )
    .await
    .map_err(CrabError::from)?;
    Ok(active_active_repair_plan_from_coordination(plan))
}

/// Repairs regional manifest projections from coordinator truth.
pub async fn apply_active_active_repair_from_coordinator(
    config: &Config,
    repo_prefix: &str,
) -> Result<ActiveActiveRepairPlan> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "coordinator-backed repair requires active-active replication config".into(),
        })?;
    if !replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "coordinator-backed repair is only valid in active-active mode".into(),
        });
    }
    validate_active_active_config(replication)?;

    let coordination_config = active_active_coordination_config(replication);
    let coordination_plan = crab_coordination::active_active_repair_plan_from_coordinator(
        &coordination_config,
        repo_prefix,
    )
    .await
    .map_err(CrabError::from)?;
    let plan = active_active_repair_plan_from_coordination(coordination_plan.clone());
    apply_active_active_repair_manifests(replication, &plan, repo_prefix).await?;
    crab_coordination::mark_active_active_repair_materialized(
        &coordination_config,
        repo_prefix,
        &coordination_plan,
    )
    .await
    .map_err(CrabError::from)?;
    Ok(plan)
}

/// Fences active-active writes by incrementing the coordinator epoch and marking it unhealthy.
pub async fn fence_active_active_writes(
    config: &Config,
    repo_prefix: &str,
    reason: Option<&str>,
) -> Result<ActiveActiveFailoverOutcome> {
    apply_active_active_failover_operation(
        config,
        repo_prefix,
        ActiveActiveFailoverOperation::Fence,
        reason,
    )
    .await
}

/// Resumes active-active writes after a fenced epoch and repair proof are verified.
pub async fn resume_active_active_writes(
    config: &Config,
    repo_prefix: &str,
    proof: ActiveActiveResumeProof,
) -> Result<ActiveActiveFailoverOutcome> {
    validate_active_active_resume_proof(proof)?;
    apply_active_active_failover_operation(
        config,
        repo_prefix,
        ActiveActiveFailoverOperation::Resume,
        None,
    )
    .await
}

fn validate_active_active_resume_proof(proof: ActiveActiveResumeProof) -> Result<()> {
    if !proof.repair_verified {
        return Err(CrabError::Configuration {
            key: "replication.failover.resume.repair_verified".into(),
            origin: "active-active resume requires proof that coordinator-backed repair and external provider failover checks completed".into(),
        });
    }
    Ok(())
}

/// Reads the configured coordinator's data-plane health after control-plane proof.
pub async fn active_active_coordinator_health(
    config: &Config,
    repo_prefix: &str,
) -> Result<CoordinatorHealth> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "active-active failover status requires replication config".into(),
        })?;
    if !replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "active-active failover status is only valid in active-active mode".into(),
        });
    }
    validate_active_active_config(replication)?;

    crab_coordination::active_active_coordinator_health(
        &active_active_coordination_config(replication),
        repo_prefix,
    )
    .await
    .map_err(CrabError::from)
}

async fn apply_active_active_failover_operation(
    config: &Config,
    repo_prefix: &str,
    operation: ActiveActiveFailoverOperation,
    reason: Option<&str>,
) -> Result<ActiveActiveFailoverOutcome> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "active-active failover requires replication config".into(),
        })?;
    if !replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "active-active failover is only valid in active-active mode".into(),
        });
    }
    validate_active_active_config(replication)?;

    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active failover requires a managed coordinator".into(),
        })?;
    let target = active_active_coordinator_resource(&coordinator.url)?;
    let coordination_config = active_active_coordination_config(replication);
    let outcome = match operation {
        ActiveActiveFailoverOperation::Fence => {
            crab_coordination::fence_active_active_writes(
                &coordination_config,
                repo_prefix,
                reason.map(str::to_owned),
            )
            .await
        }
        ActiveActiveFailoverOperation::Resume => {
            crab_coordination::resume_active_active_writes(&coordination_config, repo_prefix).await
        }
    }
    .map_err(CrabError::from)?;

    Ok(active_active_failover_outcome(
        operation,
        target.provider,
        coordinator,
        repo_prefix,
        outcome,
    ))
}

fn active_active_failover_outcome(
    operation: ActiveActiveFailoverOperation,
    provider: ManagedCoordinatorProvider,
    coordinator: &ReplicationCoordinatorConfig,
    repo_prefix: &str,
    outcome: CoordinatorFenceOutcome,
) -> ActiveActiveFailoverOutcome {
    ActiveActiveFailoverOutcome {
        operation,
        provider,
        coordinator_url: coordinator.url.clone(),
        repo_prefix: repo_prefix.to_owned(),
        previous_epoch: outcome.previous_epoch,
        coordinator_epoch: outcome.coordinator_epoch,
        previous_healthy: outcome.previous_healthy,
        healthy: outcome.healthy,
        changed: outcome.changed,
        reason: outcome.reason,
    }
}

fn active_active_coordinator_resource(
    url: &str,
) -> Result<coordination_active_active::ActiveActiveCoordinatorResource> {
    coordination_active_active::active_active_coordinator_resource(url).map_err(CrabError::from)
}

#[cfg(feature = "coordinator-dynamodb")]
async fn dynamodb_active_active_gc_protected_keys(
    config: &Config,
    coordinator: &ReplicationCoordinatorConfig,
    table_name: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::dynamodb_coordinator::{
        AwsDynamoDbCoordinatorBackend, AwsDynamoDbWriteCoordinatorClient, DynamoDbWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, dynamodb_coordinator_plan,
        inspect_coordinator_control_plane_plan_with_backend,
    };

    let plan = dynamodb_coordinator_plan(
        table_name,
        &coordinator.region,
        &coordinator.failover_regions,
    );
    let status =
        inspect_coordinator_control_plane_plan_with_backend(&plan, &AwsDynamoDbCoordinatorBackend)
            .await
            .map_err(|err| {
                coordinator_gc_snapshot_status_error(
                    ManagedCoordinatorProvider::DynamoDb,
                    table_name,
                    "maintenance",
                    err.into(),
                )
            })?;
    ensure_active_active_maintenance_admitted_with_coordinator_status(
        config,
        "repo garbage collection",
        &status,
    )?;

    let client = AwsDynamoDbWriteCoordinatorClient::for_region(&coordinator.region).await;
    let coordinator = DynamoDbWriteCoordinator::new(table_name.to_owned(), repo_prefix, client);
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(feature = "coordinator-dynamodb")]
async fn dynamodb_active_active_gc_protected_keys_for_registration(
    registration: &ActiveActiveCoordinatorRegistration,
    table_name: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::dynamodb_coordinator::{
        AwsDynamoDbCoordinatorBackend, AwsDynamoDbWriteCoordinatorClient, DynamoDbWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, dynamodb_coordinator_plan,
        inspect_coordinator_control_plane_plan_with_backend,
    };

    let plan = dynamodb_coordinator_plan(
        table_name,
        &registration.region,
        &registration.failover_regions,
    );
    let status =
        inspect_coordinator_control_plane_plan_with_backend(&plan, &AwsDynamoDbCoordinatorBackend)
            .await
            .map_err(|err| {
                coordinator_gc_snapshot_status_error(
                    ManagedCoordinatorProvider::DynamoDb,
                    table_name,
                    "bucket maintenance",
                    err.into(),
                )
            })?;
    validate_coordinator_gc_snapshot_admission(&status)?;

    let client = AwsDynamoDbWriteCoordinatorClient::for_region(&registration.region).await;
    let coordinator = DynamoDbWriteCoordinator::new(table_name.to_owned(), repo_prefix, client);
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(not(feature = "coordinator-dynamodb"))]
async fn dynamodb_active_active_gc_protected_keys(
    _config: &Config,
    _coordinator: &ReplicationCoordinatorConfig,
    table_name: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "DynamoDB active-active GC snapshot {table_name} requires the coordinator-dynamodb feature; maintenance fails closed"
        ),
    })
}

#[cfg(not(feature = "coordinator-dynamodb"))]
async fn dynamodb_active_active_gc_protected_keys_for_registration(
    _registration: &ActiveActiveCoordinatorRegistration,
    table_name: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_registration".into(),
        origin: format!(
            "DynamoDB active-active GC snapshot {table_name} requires the coordinator-dynamodb feature; bucket maintenance fails closed"
        ),
    })
}

#[cfg(feature = "coordinator-spanner")]
async fn spanner_active_active_gc_protected_keys(
    config: &Config,
    coordinator: &ReplicationCoordinatorConfig,
    instance_id: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::spanner_coordinator::{
        GoogleSpannerCoordinatorBackend, GoogleSpannerWriteCoordinatorClient, SPANNER_DATABASE_ID,
        SpannerWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, inspect_coordinator_control_plane_plan_with_backend,
        spanner_coordinator_plan,
    };

    let plan = spanner_coordinator_plan(
        instance_id,
        &coordinator.region,
        &coordinator.failover_regions,
    );
    let status = inspect_coordinator_control_plane_plan_with_backend(
        &plan,
        &GoogleSpannerCoordinatorBackend,
    )
    .await
    .map_err(|err| {
        coordinator_gc_snapshot_status_error(
            ManagedCoordinatorProvider::Spanner,
            instance_id,
            "maintenance",
            err.into(),
        )
    })?;
    ensure_active_active_maintenance_admitted_with_coordinator_status(
        config,
        "repo garbage collection",
        &status,
    )?;

    let client = GoogleSpannerWriteCoordinatorClient::new().await?;
    let coordinator = SpannerWriteCoordinator::new(
        instance_id.to_owned(),
        SPANNER_DATABASE_ID,
        repo_prefix,
        client,
    );
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(feature = "coordinator-spanner")]
async fn spanner_active_active_gc_protected_keys_for_registration(
    registration: &ActiveActiveCoordinatorRegistration,
    instance_id: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::spanner_coordinator::{
        GoogleSpannerCoordinatorBackend, GoogleSpannerWriteCoordinatorClient, SPANNER_DATABASE_ID,
        SpannerWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, inspect_coordinator_control_plane_plan_with_backend,
        spanner_coordinator_plan,
    };

    let plan = spanner_coordinator_plan(
        instance_id,
        &registration.region,
        &registration.failover_regions,
    );
    let status = inspect_coordinator_control_plane_plan_with_backend(
        &plan,
        &GoogleSpannerCoordinatorBackend,
    )
    .await
    .map_err(|err| {
        coordinator_gc_snapshot_status_error(
            ManagedCoordinatorProvider::Spanner,
            instance_id,
            "bucket maintenance",
            err.into(),
        )
    })?;
    validate_coordinator_gc_snapshot_admission(&status)?;

    let client = GoogleSpannerWriteCoordinatorClient::new().await?;
    let coordinator = SpannerWriteCoordinator::new(
        instance_id.to_owned(),
        SPANNER_DATABASE_ID,
        repo_prefix,
        client,
    );
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(not(feature = "coordinator-spanner"))]
async fn spanner_active_active_gc_protected_keys(
    _config: &Config,
    _coordinator: &ReplicationCoordinatorConfig,
    instance_id: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "Spanner active-active GC snapshot {instance_id} requires the coordinator-spanner feature; maintenance fails closed"
        ),
    })
}

#[cfg(not(feature = "coordinator-spanner"))]
async fn spanner_active_active_gc_protected_keys_for_registration(
    _registration: &ActiveActiveCoordinatorRegistration,
    instance_id: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_registration".into(),
        origin: format!(
            "Spanner active-active GC snapshot {instance_id} requires the coordinator-spanner feature; bucket maintenance fails closed"
        ),
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
async fn cosmosdb_active_active_gc_protected_keys(
    config: &Config,
    coordinator: &ReplicationCoordinatorConfig,
    account_name: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::cosmosdb_coordinator::{
        AzureCosmosDbCoordinatorBackend, AzureCosmosDbWriteCoordinatorClient,
        COSMOSDB_DATABASE_NAME, CosmosDbWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, cosmosdb_coordinator_plan,
        inspect_coordinator_control_plane_plan_with_backend,
    };

    let plan = cosmosdb_coordinator_plan(
        account_name,
        &coordinator.region,
        &coordinator.failover_regions,
    );
    let status = inspect_coordinator_control_plane_plan_with_backend(
        &plan,
        &AzureCosmosDbCoordinatorBackend,
    )
    .await
    .map_err(|err| {
        coordinator_gc_snapshot_status_error(
            ManagedCoordinatorProvider::CosmosDb,
            account_name,
            "maintenance",
            err.into(),
        )
    })?;
    ensure_active_active_maintenance_admitted_with_coordinator_status(
        config,
        "repo garbage collection",
        &status,
    )?;

    let client = AzureCosmosDbWriteCoordinatorClient::new()?;
    let coordinator = CosmosDbWriteCoordinator::new(
        account_name.to_owned(),
        COSMOSDB_DATABASE_NAME,
        repo_prefix,
        client,
    );
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(feature = "coordinator-cosmosdb")]
async fn cosmosdb_active_active_gc_protected_keys_for_registration(
    registration: &ActiveActiveCoordinatorRegistration,
    account_name: &str,
    repo_prefix: &str,
) -> Result<HashSet<String>> {
    use crab_coordination::cosmosdb_coordinator::{
        AzureCosmosDbCoordinatorBackend, AzureCosmosDbWriteCoordinatorClient,
        COSMOSDB_DATABASE_NAME, CosmosDbWriteCoordinator,
    };
    use crab_coordination::write_coordinator::{
        WriteCoordinator, cosmosdb_coordinator_plan,
        inspect_coordinator_control_plane_plan_with_backend,
    };

    let plan = cosmosdb_coordinator_plan(
        account_name,
        &registration.region,
        &registration.failover_regions,
    );
    let status = inspect_coordinator_control_plane_plan_with_backend(
        &plan,
        &AzureCosmosDbCoordinatorBackend,
    )
    .await
    .map_err(|err| {
        coordinator_gc_snapshot_status_error(
            ManagedCoordinatorProvider::CosmosDb,
            account_name,
            "bucket maintenance",
            err.into(),
        )
    })?;
    validate_coordinator_gc_snapshot_admission(&status)?;

    let client = AzureCosmosDbWriteCoordinatorClient::new()?;
    let coordinator = CosmosDbWriteCoordinator::new(
        account_name.to_owned(),
        COSMOSDB_DATABASE_NAME,
        repo_prefix,
        client,
    );
    Ok(coordinator.gc_safety_snapshot().await?.protected_keys())
}

#[cfg(not(feature = "coordinator-cosmosdb"))]
async fn cosmosdb_active_active_gc_protected_keys(
    _config: &Config,
    _coordinator: &ReplicationCoordinatorConfig,
    account_name: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "Cosmos DB active-active GC snapshot {account_name} requires the coordinator-cosmosdb feature; maintenance fails closed"
        ),
    })
}

#[cfg(not(feature = "coordinator-cosmosdb"))]
async fn cosmosdb_active_active_gc_protected_keys_for_registration(
    _registration: &ActiveActiveCoordinatorRegistration,
    account_name: &str,
    _repo_prefix: &str,
) -> Result<HashSet<String>> {
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_registration".into(),
        origin: format!(
            "Cosmos DB active-active GC snapshot {account_name} requires the coordinator-cosmosdb feature; bucket maintenance fails closed"
        ),
    })
}

async fn apply_active_active_repair_manifests(
    replication: &ReplicationConfig,
    plan: &ActiveActiveRepairPlan,
    primary_repo_path: &str,
) -> Result<()> {
    for action in &plan.actions {
        apply_active_active_repair_action(replication, action, primary_repo_path).await?;
    }
    Ok(())
}

async fn apply_active_active_repair_action(
    replication: &ReplicationConfig,
    action: &ActiveActiveRepairAction,
    primary_repo_path: &str,
) -> Result<()> {
    let source_writer = coordination_active_active::active_active_writer_for_region(
        &active_active_coordination_config(replication),
        &action.source_region,
    )
    .map(writer_from_coordination)
    .map_err(CrabError::from)?;
    let (source_store, source_prefix) = build_writer_store(&source_writer, primary_repo_path)?;
    let (target_store, target_prefix) = build_writer_store(&action.writer, primary_repo_path)?;
    let source_router = StoreLayout::new(source_store.clone(), source_prefix.clone());
    let target_router = StoreLayout::new(target_store.clone(), target_prefix.clone());
    let cancel = CancellationToken::new();
    let writer = crate::maintenance::GcWriterLeases::acquire(
        &target_store,
        target_router.global_prefix(),
        target_router.repo_prefix(),
        &cancel,
    )
    .await?;

    let operation = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CrabError::Cancelled),
        result = async {
            let (manifest, _) = read_manifest(&source_store, &source_router).await?;
            if manifest.generation < action.manifest_generation {
                return Err(CrabError::Configuration {
                    key: "replication.repair.source_manifest".into(),
                    origin: format!(
                        "source region {} manifest generation {} is behind coordinator generation {}",
                        action.source_region, manifest.generation, action.manifest_generation
                    ),
                });
            }

            verify_repair_uploaded_objects_present(
                &target_store,
                &action.uploaded_objects,
                &source_prefix,
                &target_prefix,
            )
            .await?;
            replicate_git_visibility_index(
                &source_store,
                &source_router,
                &target_store,
                &target_router,
                &manifest,
            )
            .await?;
            materialize_active_active_manifest_projection(&target_store, &target_router, &manifest)
                .await
        } => result,
    };
    let release = writer.release().await;
    match (operation, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

async fn replicate_git_visibility_index(
    source_store: &Store,
    source_router: &StoreLayout,
    target_store: &Store,
    target_router: &StoreLayout,
    manifest: &Manifest,
) -> Result<()> {
    if manifest.refs.is_empty() || manifest.pack_index_hash.is_empty() {
        return Ok(());
    }

    let source_storage_router = crab_storage::StoreLayout::new(
        source_store.as_storage().clone(),
        source_router.repo_prefix().to_owned(),
    );
    // Repair can only copy an existing proof; unlike push publication it
    // cannot reconstruct a digest-bound proof before materializing the target
    // manifest. Treat a mismatched legacy proof as corruption instead of
    // silently publishing a manifest without its visibility guard.
    let index = match crab_metadata::git_visibility::read(
        source_store.as_storage(),
        &source_storage_router,
        manifest.generation,
        &manifest.pack_index_hash,
        &manifest.git_validation_digest,
    )
    .await
    {
        Ok(index) => index,
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => return Ok(()),
        Err(error) => return Err(CrabError::from(error)),
    };
    if !index.matches_manifest(manifest) {
        return Err(CrabError::CorruptObject {
            path: source_storage_router
                .git_visibility_path(&manifest.git_validation_digest)
                .as_ref()
                .to_owned(),
            reason: "Git visibility proof does not match its source manifest".to_owned(),
        });
    }
    let target_storage_router = crab_storage::StoreLayout::new(
        target_store.as_storage().clone(),
        target_router.repo_prefix().to_owned(),
    );
    crab_metadata::git_visibility::upload_if_absent(
        target_store.as_storage(),
        &target_storage_router,
        &index,
    )
    .await
    .map_err(CrabError::from)
}

async fn verify_repair_uploaded_objects_present(
    target_store: &Store,
    uploaded_objects: &[String],
    source_prefix: &str,
    target_prefix: &str,
) -> Result<()> {
    for key in uploaded_objects {
        let target_key = repair_object_key_for_target_prefix(key, source_prefix, target_prefix)?;
        let path = ObjectPath::from(target_key.as_str());
        match target_store.head(&path).await {
            Ok(_) => {}
            Err(CrabError::NotFound { .. }) => {
                return Err(CrabError::Configuration {
                    key: "replication.repair.object".into(),
                    origin: format!(
                        "target region is missing replicated transaction object {target_key}; repair refuses to publish the manifest before object replication catches up"
                    ),
                });
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn repair_object_key_for_target_prefix(
    key: &str,
    source_prefix: &str,
    target_prefix: &str,
) -> Result<String> {
    if key == ".crab" || key.starts_with(".crab/") {
        return Ok(key.to_owned());
    }
    if key == source_prefix {
        return Ok(target_prefix.to_owned());
    }
    if let Some(rest) = key.strip_prefix(&format!("{source_prefix}/")) {
        return Ok(format!("{target_prefix}/{rest}"));
    }
    Err(CrabError::Configuration {
        key: "replication.repair.object".into(),
        origin: format!(
            "transaction object {key} is outside source repo prefix {source_prefix} and shared .crab/"
        ),
    })
}

/// Writes a regional active-active manifest projection without making it write authority.
pub async fn materialize_active_active_manifest_projection(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<()> {
    materialize_manifest_projection(store, router, manifest).await
}

fn build_writer_store(writer: &WriterConfig, primary_repo_path: &str) -> Result<(Store, String)> {
    let parsed = ObjectUrl::parse(&writer.url)?;
    let Some(provider) = ReplicationProviderKind::from_storage_provider_kind(parsed.cloud) else {
        return Err(CrabError::Configuration {
            key: "replication.writers.url".into(),
            origin: "active-active writer URLs must target S3, GCS, Azure, or crab:// storage"
                .into(),
        });
    };
    let selection = crab_storage::static_env_target_selection(
        crab_storage::StaticEnvStoreUrlParts {
            provider: parsed.cloud,
            form: static_env_store_url_form(parsed.form),
            bucket: &parsed.bucket,
            prefix: &parsed.prefix,
        },
        Some(provider.storage_provider_kind()),
        primary_repo_path,
    )
    .map_err(CrabError::from)?;
    let store = build_static_env_target_store(selection.target)?;
    Ok((store, selection.repo_prefix))
}

fn ensure_active_active_mutation_admitted(config: &Config, operation: &str) -> Result<()> {
    ensure_active_active_mutation_admitted_with_status(config, operation, None)
}

fn ensure_active_active_mutation_admitted_with_status(
    config: &Config,
    operation: &str,
    coordinator_status: Option<&CoordinatorControlPlaneStatus>,
) -> Result<()> {
    let Some(replication) = config.replication.as_ref() else {
        return Ok(());
    };
    if !replication.is_active_active() {
        return Ok(());
    }
    validate_active_active_config(replication)?;
    if let Some(status) = coordinator_status {
        let admission =
            active_active_status_with_coordinator_status(Some(replication), Some(status));
        if admission.writes_enabled {
            return Ok(());
        }
        return Err(CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "active-active {operation} requires verified coordinator status: {}",
                admission
                    .reason
                    .unwrap_or_else(|| "coordinator did not admit writes".into())
            ),
        });
    }
    Err(CrabError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "active-active {operation} requires a managed linearizable coordinator adapter; refusing unsafe primary-only mutation"
        ),
    })
}

/// Result of selecting a store for a read operation.
pub type ReadStoreSelection = crab_read::ReadStoreSelection<Store, StoreLayout>;

/// Result of selecting the primary store for a write operation.
pub struct WriteStoreSelection {
    pub store: Store,
    pub router: StoreLayout,
}

struct SelectedReadReplicaStore {
    replica: ReplicaConfig,
    target: ReadStoreTarget<Store, StoreLayout>,
}

/// Resolves repository stores according to Crab's replication write model.
pub struct StoreResolver<'a> {
    config: &'a Config,
    primary_url: crab_git::url::CrabUrl,
    cancel: &'a CancellationToken,
    read_policy: Option<ReadRoutingPolicy>,
}

impl<'a> StoreResolver<'a> {
    #[must_use]
    pub fn new<U>(config: &'a Config, primary_url: U, cancel: &'a CancellationToken) -> Self
    where
        U: Into<crab_git::url::CrabUrl>,
    {
        Self {
            config,
            primary_url: primary_url.into(),
            cancel,
            read_policy: None,
        }
    }

    #[must_use]
    pub fn with_read_policy(mut self, policy: ReadRoutingPolicy) -> Self {
        self.read_policy = Some(policy);
        self
    }

    /// Selects a replica-aware store for read operations.
    pub async fn read_store(&self, operation: &str) -> Result<ReadStoreSelection> {
        let cache_dir =
            crab_auth::token_cache::expand_token_cache_path(&self.config.auth.token_cache_path);
        let resolver = crab_auth_store::ManagedRepositoryResolver::new(cache_dir);
        let locator = resolver.classify(&format!(
            "crab://{}/{}",
            self.primary_url.bucket, self.primary_url.repo_path
        ))?;
        let resolved = crate::auth::build_repository_store(
            self.config,
            locator,
            managed_read_operation(operation),
            self.cancel,
        )
        .await?;
        let primary_store = resolved.store;
        let primary_router =
            StoreLayout::new(primary_store.clone(), resolved.repository_prefix.clone());
        if primary_store.storage_scope().is_some() {
            return Ok(ReadStoreSelection::primary(primary_store, primary_router));
        }
        let policy = match self.read_policy.as_ref() {
            Some(policy) => policy.clone(),
            None => read_routing_policy_from_env()?,
        };

        let Some(replication) = self.config.replication.as_ref() else {
            return Ok(ReadStoreSelection::primary(primary_store, primary_router));
        };

        select_read_store_with_replicas(
            primary_store,
            &resolved.repository_prefix,
            replication.replicas.iter(),
            operation,
            policy,
            build_replica_store,
        )
        .await
    }

    /// Selects the primary store for write-class operations.
    pub async fn write_store(&self, operation: &str) -> Result<WriteStoreSelection> {
        let store = crate::auth::build_store(
            self.config,
            self.primary_url.clone(),
            operation,
            self.cancel,
        )
        .await?;
        let router = StoreLayout::new(store.clone(), self.primary_url.repo_path.clone());
        Ok(WriteStoreSelection { store, router })
    }
}

fn managed_read_operation(operation: &str) -> crab_auth::TransferOperation {
    if operation.contains("hydrate") || operation.contains("smudge") {
        crab_auth::TransferOperation::Hydrate
    } else if operation.starts_with("clone") {
        crab_auth::TransferOperation::Clone
    } else {
        crab_auth::TransferOperation::Fetch
    }
}

fn read_routing_policy_from_env() -> Result<ReadRoutingPolicy> {
    read_routing_policy_from_env_value(std::env::var(READ_ROUTING_POLICY_ENV))
}

fn read_routing_policy_from_env_value(
    value: std::result::Result<String, std::env::VarError>,
) -> Result<ReadRoutingPolicy> {
    match value {
        Ok(value) => ReadRoutingPolicy::parse(&value).map_err(read_routing_policy_env_error),
        Err(std::env::VarError::NotPresent) => Ok(ReadRoutingPolicy::default()),
        Err(std::env::VarError::NotUnicode(_)) => Err(CrabError::Configuration {
            key: READ_ROUTING_POLICY_ENV.into(),
            origin: "replica read policy must be valid UTF-8".into(),
        }),
    }
}

fn read_routing_policy_env_error(error: crab_read::ReadError) -> CrabError {
    match error {
        crab_read::ReadError::Configuration { origin, .. } => CrabError::Configuration {
            key: READ_ROUTING_POLICY_ENV.into(),
            origin,
        },
        other => CrabError::from(other),
    }
}

async fn select_read_store_with_replicas<'replicas, I, F>(
    mut primary_store: Store,
    primary_repo_path: &str,
    replicas: I,
    operation: &str,
    policy: ReadRoutingPolicy,
    mut build_replica: F,
) -> Result<ReadStoreSelection>
where
    I: IntoIterator<Item = &'replicas ReplicaConfig>,
    F: FnMut(&ReplicaConfig, &str) -> Result<(Store, String)>,
{
    let primary_router = StoreLayout::new(primary_store.clone(), primary_repo_path.to_owned());
    let primary_selection =
        ReadStoreSelection::primary(primary_store.clone(), primary_router.clone());
    if matches!(
        policy,
        ReadRoutingPolicy::PreferPrimary | ReadRoutingPolicy::ReadDisabled
    ) {
        return Ok(primary_selection);
    }

    let replicas = replicas
        .into_iter()
        .map(ReadReplicaCandidate::from_replica_config_ref);
    let readiness_options = readiness_check_options_from_env()?;
    let choice = select_read_store_choice(primary_selection, replicas, &policy, |replica| {
        let built = build_replica(replica, primary_repo_path);
        let primary_store = primary_store.clone();
        let primary_router = primary_router.clone();

        async move {
            let (replica_store, replica_prefix) = match built {
                Ok(built) => built,
                Err(e) => {
                    return ReadReplicaProbeResult::fallback(
                        replica.name.clone(),
                        primary_repo_path.to_owned(),
                        replica.clone(),
                        None,
                        None,
                        Some(format!("replica client unavailable: {e}")),
                    );
                }
            };
            let replica_router = StoreLayout::new(replica_store.clone(), replica_prefix);
            match replica_readiness(
                &primary_store,
                &primary_router,
                &replica_store,
                &replica_router,
                replica,
                readiness_options,
            )
            .await
            {
                Ok(status) if status.ready => ReadReplicaProbeResult::ready(
                    replica.name.clone(),
                    replica_router.repo_prefix().to_owned(),
                    SelectedReadReplicaStore {
                        replica: replica.clone(),
                        target: ReadStoreTarget::new(replica_store, replica_router),
                    },
                    status.primary_generation,
                    status.replica_generation,
                ),
                Ok(status) => {
                    tracing::debug!(
                        replica = %replica.name,
                        reason = ?status.last_fallback_reason,
                        "replica is not ready for reads; using primary"
                    );
                    ReadReplicaProbeResult::fallback(
                        replica.name.clone(),
                        replica_router.repo_prefix().to_owned(),
                        replica.clone(),
                        status.primary_generation,
                        status.replica_generation,
                        status.last_fallback_reason,
                    )
                }
                Err(e) => {
                    tracing::debug!(
                        replica = %replica.name,
                        error = %e,
                        "replica readiness failed; using primary"
                    );
                    ReadReplicaProbeResult::fallback(
                        replica.name.clone(),
                        replica_router.repo_prefix().to_owned(),
                        replica.clone(),
                        None,
                        None,
                        Some(format!("replica readiness failed: {e}")),
                    )
                }
            }
        }
    })
    .await?;

    let primary_fallback_accounts = match choice {
        ReadStoreChoice::Replica {
            selected,
            fallbacks,
        } => {
            record_replica_fallback_events(&fallbacks, operation);
            record_replica_read_event(
                &selected.target.replica,
                &selected.repo_prefix,
                operation,
                ReplicaReadOutcome::Selected,
                selected.primary_generation,
                selected.replica_generation,
                None,
            );
            return Ok(selected.target.target.into_replica_selection(selected.name));
        }
        ReadStoreChoice::Primary { primary, fallbacks } => {
            primary_store = primary.store;
            record_replica_fallback_events(&fallbacks, operation);
            fallbacks
                .into_iter()
                .map(|fallback| (fallback.target, fallback.repo_prefix))
                .collect::<Vec<_>>()
        }
    };

    if !primary_fallback_accounts.is_empty() {
        let operation = operation.to_owned();
        primary_store = primary_store.with_read_byte_observer(Arc::new(move |bytes| {
            for (replica, repo_prefix) in &primary_fallback_accounts {
                record_replica_primary_fallback_bytes(replica, repo_prefix, &operation, bytes);
            }
        }));
    }
    let primary_router = StoreLayout::new(primary_store.clone(), primary_repo_path.to_owned());
    Ok(ReadStoreSelection::primary(primary_store, primary_router))
}

fn record_replica_fallback_events(
    fallbacks: &[ReadReplicaFallback<ReplicaConfig>],
    operation: &str,
) {
    for fallback in fallbacks {
        record_replica_read_event(
            &fallback.target,
            &fallback.repo_prefix,
            operation,
            ReplicaReadOutcome::Fallback,
            fallback.primary_generation,
            fallback.replica_generation,
            fallback.reason.clone(),
        );
    }
}

/// Pick the first ready read replica. Falls back to the primary for all
/// failures so reads never lose correctness due to replication lag.
pub async fn select_read_store<U>(
    config: &Config,
    primary_url: U,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<ReadStoreSelection>
where
    U: Into<crab_git::url::CrabUrl>,
{
    StoreResolver::new(config, primary_url, cancel)
        .read_store(operation)
        .await
}

/// Pick a read store with an explicit routing policy.
pub async fn select_read_store_with_policy<U>(
    config: &Config,
    primary_url: U,
    operation: &str,
    cancel: &CancellationToken,
    policy: ReadRoutingPolicy,
) -> Result<ReadStoreSelection>
where
    U: Into<crab_git::url::CrabUrl>,
{
    StoreResolver::new(config, primary_url, cancel)
        .with_read_policy(policy)
        .read_store(operation)
        .await
}

/// Compute status for every configured replica.
pub async fn replica_statuses(
    config: &Config,
    primary_url: &CrabUrl,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ReplicaStatus>> {
    replica_statuses_with_options(
        config,
        primary_url,
        operation,
        cancel,
        readiness_check_options_from_env()?,
    )
    .await
}

/// Compute status for every configured replica with explicit cache behavior.
pub async fn replica_statuses_with_options(
    config: &Config,
    primary_url: &CrabUrl,
    operation: &str,
    cancel: &CancellationToken,
    options: ReadinessCheckOptions,
) -> Result<Vec<ReplicaStatus>> {
    let primary_store = crate::auth::build_store(config, primary_url, operation, cancel).await?;
    let primary_router = StoreLayout::new(primary_store.clone(), primary_url.repo_path.clone());
    let Some(replication) = config.replication.as_ref() else {
        return Ok(Vec::new());
    };

    let mut statuses = Vec::with_capacity(replication.replicas.len());
    for replica in &replication.replicas {
        if cancel.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        match build_replica_store(replica, &primary_url.repo_path) {
            Ok((replica_store, replica_prefix)) => {
                let replica_router = StoreLayout::new(replica_store.clone(), replica_prefix);
                let readiness = tokio::select! {
                    result = replica_readiness(
                        &primary_store,
                        &primary_router,
                        &replica_store,
                        &replica_router,
                        replica,
                        options,
                    ) => result,
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                };
                match readiness {
                    Ok(status) => {
                        if cancel.is_cancelled() {
                            return Err(CrabError::Cancelled);
                        }
                        statuses.push(status_with_events(
                            status,
                            replica,
                            replica_router.repo_prefix(),
                        ));
                    }
                    Err(CrabError::Cancelled) => return Err(CrabError::Cancelled),
                    Err(e) => statuses.push(status_with_events(
                        failed_status(replica, e.to_string()),
                        replica,
                        replica_router.repo_prefix(),
                    )),
                }
            }
            Err(e) => statuses.push(status_with_events(
                failed_status(replica, e.to_string()),
                replica,
                &primary_url.repo_path,
            )),
        }
    }
    Ok(statuses)
}

fn failed_status(replica: &ReplicaConfig, reason: String) -> ReplicaStatus {
    let last_fallback_class = Some(ReplicaFallbackClass::from_reason(Some(&reason)));
    ReplicaStatus {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        backfill_required: replica.backfill,
        read_enabled: replica.read,
        primary_generation: None,
        replica_generation: None,
        ready: false,
        lag_generations: None,
        last_fallback_reason: Some(reason),
        last_fallback_class,
        last_fallback_at_ms: None,
        last_fallback_operation: None,
        fallback_count: 0,
        primary_fallback_bytes: 0,
        last_selected_at_ms: None,
        last_selected_operation: None,
        selected_count: 0,
        readiness_cache_hit: false,
        readiness_cache_age_ms: None,
        readiness_check_latency_ms: None,
        readiness_object_probe_count: 0,
        readiness_object_read_count: 0,
    }
}

async fn replica_readiness(
    primary_store: &Store,
    primary_router: &StoreLayout,
    replica_store: &Store,
    replica_router: &StoreLayout,
    replica: &ReplicaConfig,
    options: ReadinessCheckOptions,
) -> Result<ReplicaStatus> {
    let started = Instant::now();
    let (primary_manifest, primary_etag) = read_manifest(primary_store, primary_router).await?;
    let primary_generation = primary_manifest.generation;

    let replica_prefix = replica_router.repo_prefix();
    let now_ms = now_unix_ms();
    if let Some(cache_age_ms) = readiness_cache_hit(
        replica,
        replica_prefix,
        primary_generation,
        &primary_etag,
        now_ms,
        options,
    ) {
        return Ok(ReplicaStatus {
            name: replica.name.clone(),
            provider: replica.provider,
            url: replica.url.clone(),
            region: replica.region.clone(),
            backfill_required: replica.backfill,
            read_enabled: replica.read,
            primary_generation: Some(primary_generation),
            replica_generation: Some(primary_generation),
            ready: true,
            lag_generations: Some(0),
            last_fallback_reason: None,
            last_fallback_class: None,
            last_fallback_at_ms: None,
            last_fallback_operation: None,
            fallback_count: 0,
            primary_fallback_bytes: 0,
            last_selected_at_ms: None,
            last_selected_operation: None,
            selected_count: 0,
            readiness_cache_hit: true,
            readiness_cache_age_ms: Some(cache_age_ms),
            readiness_check_latency_ms: Some(elapsed_ms(started)),
            readiness_object_probe_count: 0,
            readiness_object_read_count: 0,
        });
    }

    let primary_read_router = crab_storage::StoreLayout::with_global_prefix(
        primary_store.as_storage().clone(),
        primary_router.repo_prefix().to_owned(),
        primary_router.global_prefix().to_owned(),
    );
    let replica_read_router = crab_storage::StoreLayout::with_global_prefix(
        replica_store.as_storage().clone(),
        replica_router.repo_prefix().to_owned(),
        replica_router.global_prefix().to_owned(),
    );
    let readiness = check_read_replica_readiness(
        primary_store.as_storage(),
        &primary_read_router,
        replica_store.as_storage(),
        &replica_read_router,
        options,
    )
    .await?;
    if let Some(reason) = readiness.reason {
        return Ok(status_with_readiness_stats(
            status_with_reason(
                replica,
                Some(readiness.primary_generation),
                readiness.replica_generation,
                reason,
            ),
            started,
            readiness.stats,
        ));
    }

    if options.max_object_probes.is_none() {
        write_readiness_cache(
            replica,
            replica_prefix,
            primary_generation,
            &primary_etag,
            now_ms,
        );
    }
    Ok(ReplicaStatus {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        backfill_required: replica.backfill,
        read_enabled: replica.read,
        primary_generation: Some(primary_generation),
        replica_generation: readiness.replica_generation,
        ready: true,
        lag_generations: readiness.lag_generations,
        last_fallback_reason: None,
        last_fallback_class: None,
        last_fallback_at_ms: None,
        last_fallback_operation: None,
        fallback_count: 0,
        primary_fallback_bytes: 0,
        last_selected_at_ms: None,
        last_selected_operation: None,
        selected_count: 0,
        readiness_cache_hit: false,
        readiness_cache_age_ms: None,
        readiness_check_latency_ms: Some(elapsed_ms(started)),
        readiness_object_probe_count: readiness.stats.object_probe_count,
        readiness_object_read_count: readiness.stats.object_read_count,
    })
}

fn status_with_reason(
    replica: &ReplicaConfig,
    primary_generation: Option<u64>,
    replica_generation: Option<u64>,
    reason: String,
) -> ReplicaStatus {
    let lag_generations = match (primary_generation, replica_generation) {
        (Some(primary), Some(replica)) => Some(primary.saturating_sub(replica)),
        _ => None,
    };
    let last_fallback_class = Some(ReplicaFallbackClass::from_reason(Some(&reason)));
    ReplicaStatus {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        backfill_required: replica.backfill,
        read_enabled: replica.read,
        primary_generation,
        replica_generation,
        ready: false,
        lag_generations,
        last_fallback_reason: Some(reason),
        last_fallback_class,
        last_fallback_at_ms: None,
        last_fallback_operation: None,
        fallback_count: 0,
        primary_fallback_bytes: 0,
        last_selected_at_ms: None,
        last_selected_operation: None,
        selected_count: 0,
        readiness_cache_hit: false,
        readiness_cache_age_ms: None,
        readiness_check_latency_ms: None,
        readiness_object_probe_count: 0,
        readiness_object_read_count: 0,
    }
}

fn status_with_readiness_stats(
    mut status: ReplicaStatus,
    started: Instant,
    stats: crab_read::ReadinessProbeStats,
) -> ReplicaStatus {
    status.readiness_check_latency_ms = Some(elapsed_ms(started));
    status.readiness_object_probe_count = stats.object_probe_count;
    status.readiness_object_read_count = stats.object_read_count;
    status
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn status_with_events(
    status: ReplicaStatus,
    replica: &ReplicaConfig,
    repo_prefix: &str,
) -> ReplicaStatus {
    let summary = read_replica_event_summary(replica, repo_prefix);
    status_with_event_summary(status, summary)
}

fn status_with_event_summary(
    mut status: ReplicaStatus,
    summary: ReplicaEventSummary,
) -> ReplicaStatus {
    if status.last_fallback_reason.is_none() {
        status.last_fallback_reason = summary.last_fallback_reason;
    }
    if status.last_fallback_class.is_none() {
        status.last_fallback_class = status
            .last_fallback_reason
            .as_deref()
            .map(|reason| ReplicaFallbackClass::from_reason(Some(reason)))
            .or(summary.last_fallback_class);
    }
    status.last_fallback_at_ms = summary.last_fallback_at_ms;
    status.last_fallback_operation = summary.last_fallback_operation;
    status.fallback_count = summary.fallback_count;
    status.primary_fallback_bytes = summary.primary_fallback_bytes;
    status.last_selected_at_ms = summary.last_selected_at_ms;
    status.last_selected_operation = summary.last_selected_operation;
    status.selected_count = summary.selected_count;
    status
}

fn build_replica_store(
    replica: &ReplicaConfig,
    primary_repo_path: &str,
) -> Result<(Store, String)> {
    let parsed = ObjectUrl::parse(&replica.url)?;
    let selection = replica_static_env_target_selection(&parsed, replica, primary_repo_path)?;
    let store = build_static_env_target_store(selection.target)?;
    Ok((store, selection.repo_prefix))
}

/// Returns the repo prefix used by a replica after provider URL normalization.
pub fn replica_effective_repo_prefix(
    replica: &ReplicaConfig,
    primary_repo_path: &str,
) -> Result<String> {
    let parsed = ObjectUrl::parse(&replica.url)?;
    Ok(replica_static_env_target_selection(&parsed, replica, primary_repo_path)?.repo_prefix)
}

fn replica_static_env_target_selection(
    parsed: &ObjectUrl,
    replica: &ReplicaConfig,
    primary_repo_path: &str,
) -> Result<crab_storage::StaticEnvStoreTargetSelection> {
    crab_storage::static_env_target_selection_for_provider(
        static_env_store_url_parts(parsed),
        replica.provider.storage_provider_kind(),
        primary_repo_path,
    )
    .map_err(|error| replica_provider_error(error, replica.provider, parsed))
}

fn static_env_store_url_parts(url: &ObjectUrl) -> crab_storage::StaticEnvStoreUrlParts<'_> {
    crab_storage::StaticEnvStoreUrlParts {
        provider: url.cloud,
        form: static_env_store_url_form(url.form),
        bucket: &url.bucket,
        prefix: &url.prefix,
    }
}

fn static_env_store_url_form(form: UrlForm) -> crab_storage::StaticEnvStoreUrlForm {
    match form {
        UrlForm::Raw => crab_storage::StaticEnvStoreUrlForm::Raw,
        UrlForm::Crab => crab_storage::StaticEnvStoreUrlForm::Crab,
    }
}

fn build_static_env_target_store(target: crab_storage::StaticEnvStoreTarget) -> Result<Store> {
    crab_storage::build_static_env_target_store(target)
        .map(Store::from)
        .map_err(CrabError::from)
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadinessCache {
    version: u32,
    replica_name: String,
    provider: ReplicationProviderKind,
    url: String,
    region: String,
    repo_prefix: String,
    generation: u64,
    primary_etag: String,
    written_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadinessCacheInvalidation {
    version: u32,
    replica_name: String,
    provider: ReplicationProviderKind,
    url: String,
    region: String,
    repo_prefix: String,
    invalidated_at_ms: u64,
    reason: String,
}

/// Synchronizes the local readiness cache invalidation marker with provider drift status.
pub fn sync_readiness_cache_control_plane(
    primary_repo_path: &str,
    replication: &ReplicationConfig,
    statuses: &[ControlPlaneStatus],
) {
    for replica in &replication.replicas {
        let Ok(repo_prefix) = replica_effective_repo_prefix(replica, primary_repo_path) else {
            continue;
        };
        let status = statuses
            .iter()
            .find(|status| status.replica_name == replica.name);
        if let Some(reason) = readiness_cache_invalidation_reason(status) {
            write_readiness_cache_invalidation(replica, &repo_prefix, &reason, now_unix_ms());
        } else {
            clear_readiness_cache_invalidation(replica, &repo_prefix);
        }
    }
}

fn readiness_cache_invalidation_reason(status: Option<&ControlPlaneStatus>) -> Option<String> {
    let Some(status) = status else {
        return Some("provider control-plane status is unavailable".to_owned());
    };
    if !status.backend_available {
        return Some("provider control-plane backend is unavailable".to_owned());
    }
    if !status.checked_drift {
        return Some("provider control-plane drift was not checked".to_owned());
    }
    status
        .checks
        .iter()
        .find(|check| check.state != ControlPlaneCheckState::Verified)
        .map(|check| {
            format!(
                "provider control-plane check {} is {}",
                check.code,
                check.state.as_str()
            )
        })
}

fn readiness_cache_hit(
    replica: &ReplicaConfig,
    repo_prefix: &str,
    generation: u64,
    primary_etag: &str,
    now_ms: u64,
    options: ReadinessCheckOptions,
) -> Option<u64> {
    if options.bypass_cache {
        return None;
    }
    if readiness_cache_is_invalidated(replica, repo_prefix) {
        return None;
    }
    let path = readiness_cache_path(replica, repo_prefix);
    let Ok(bytes) = std::fs::read(path) else {
        return None;
    };
    let Ok(cache) = serde_json::from_slice::<ReadinessCache>(&bytes) else {
        return None;
    };
    readiness_cache_age_ms(
        &cache,
        replica,
        repo_prefix,
        generation,
        primary_etag,
        now_ms,
        options,
    )
}

fn readiness_cache_age_ms(
    cache: &ReadinessCache,
    replica: &ReplicaConfig,
    repo_prefix: &str,
    generation: u64,
    primary_etag: &str,
    now_ms: u64,
    options: ReadinessCheckOptions,
) -> Option<u64> {
    if options.bypass_cache {
        return None;
    }
    if readiness_cache_is_invalidated(replica, repo_prefix) {
        return None;
    }
    if cache.version != READINESS_CACHE_VERSION
        || cache.replica_name != replica.name
        || cache.provider != replica.provider
        || cache.url != replica.url
        || cache.region != replica.region
        || cache.repo_prefix != repo_prefix
        || cache.generation < generation
        || cache.primary_etag != primary_etag
    {
        return None;
    }
    let age_ms = now_ms.saturating_sub(cache.written_at_ms);
    if age_ms > options.cache_ttl_ms {
        return None;
    }
    Some(age_ms)
}

fn readiness_cache_is_invalidated(replica: &ReplicaConfig, repo_prefix: &str) -> bool {
    let path = readiness_cache_invalidation_path(replica, repo_prefix);
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(marker) = serde_json::from_slice::<ReadinessCacheInvalidation>(&bytes) else {
        return false;
    };
    marker.version == READINESS_CACHE_INVALIDATION_VERSION
        && marker.replica_name == replica.name
        && marker.provider == replica.provider
        && marker.url == replica.url
        && marker.region == replica.region
        && marker.repo_prefix == repo_prefix
}

fn write_readiness_cache_invalidation(
    replica: &ReplicaConfig,
    repo_prefix: &str,
    reason: &str,
    invalidated_at_ms: u64,
) {
    let path = readiness_cache_invalidation_path(replica, repo_prefix);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let marker = ReadinessCacheInvalidation {
        version: READINESS_CACHE_INVALIDATION_VERSION,
        replica_name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        repo_prefix: repo_prefix.to_owned(),
        invalidated_at_ms,
        reason: reason.to_owned(),
    };
    if let Ok(bytes) = serde_json::to_vec(&marker) {
        let _ = std::fs::write(path, bytes);
    }
}

fn clear_readiness_cache_invalidation(replica: &ReplicaConfig, repo_prefix: &str) {
    let _ = std::fs::remove_file(readiness_cache_invalidation_path(replica, repo_prefix));
}

fn write_readiness_cache(
    replica: &ReplicaConfig,
    repo_prefix: &str,
    generation: u64,
    primary_etag: &str,
    written_at_ms: u64,
) {
    let path = readiness_cache_path(replica, repo_prefix);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = ReadinessCache {
        version: READINESS_CACHE_VERSION,
        replica_name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        repo_prefix: repo_prefix.to_owned(),
        generation,
        primary_etag: primary_etag.to_owned(),
        written_at_ms,
    };
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(path, bytes);
    }
}

fn readiness_cache_path(replica: &ReplicaConfig, repo_prefix: &str) -> PathBuf {
    replica_cache_dir(replica, repo_prefix).join("readiness.json")
}

fn readiness_cache_invalidation_path(replica: &ReplicaConfig, repo_prefix: &str) -> PathBuf {
    replica_cache_dir(replica, repo_prefix).join("readiness-invalidated.json")
}

fn record_replica_read_event(
    replica: &ReplicaConfig,
    repo_prefix: &str,
    operation: &str,
    outcome: ReplicaReadOutcome,
    primary_generation: Option<u64>,
    replica_generation: Option<u64>,
    reason: Option<String>,
) {
    let event = ReplicaReadEvent {
        version: READ_EVENT_VERSION,
        timestamp_ms: now_unix_ms(),
        replica_name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        repo_prefix: repo_prefix.to_owned(),
        operation: operation.to_owned(),
        outcome,
        primary_generation,
        replica_generation,
        reason,
        primary_fallback_bytes: None,
    };
    let path = replica_event_log_path(replica, repo_prefix);
    append_replica_read_event(&path, &event);
}

fn record_replica_primary_fallback_bytes(
    replica: &ReplicaConfig,
    repo_prefix: &str,
    operation: &str,
    bytes: u64,
) {
    let event = ReplicaReadEvent {
        version: READ_EVENT_VERSION,
        timestamp_ms: now_unix_ms(),
        replica_name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        repo_prefix: repo_prefix.to_owned(),
        operation: operation.to_owned(),
        outcome: ReplicaReadOutcome::PrimaryFallbackRead,
        primary_generation: None,
        replica_generation: None,
        reason: None,
        primary_fallback_bytes: Some(bytes),
    };
    let path = replica_event_log_path(replica, repo_prefix);
    append_replica_read_event(&path, &event);
}

fn append_replica_read_event(path: &Path, event: &ReplicaReadEvent) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_large_event_log(path);
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn rotate_large_event_log(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= READ_EVENT_LOG_MAX_BYTES {
        return;
    }
    let rotated = path.with_extension("jsonl.1");
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, rotated);
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReplicaEventSummary {
    last_fallback_reason: Option<String>,
    last_fallback_class: Option<ReplicaFallbackClass>,
    last_fallback_at_ms: Option<u64>,
    last_fallback_operation: Option<String>,
    fallback_count: u64,
    primary_fallback_bytes: u64,
    last_selected_at_ms: Option<u64>,
    last_selected_operation: Option<String>,
    selected_count: u64,
}

fn read_replica_event_summary(replica: &ReplicaConfig, repo_prefix: &str) -> ReplicaEventSummary {
    read_replica_event_summary_from_path(
        &replica_event_log_path(replica, repo_prefix),
        replica,
        repo_prefix,
    )
}

fn read_replica_event_summary_from_path(
    path: &Path,
    replica: &ReplicaConfig,
    repo_prefix: &str,
) -> ReplicaEventSummary {
    let Ok(body) = std::fs::read_to_string(path) else {
        return ReplicaEventSummary::default();
    };

    let mut summary = ReplicaEventSummary::default();
    for line in body.lines() {
        let Ok(event) = serde_json::from_str::<ReplicaReadEvent>(line) else {
            continue;
        };
        if !event_matches_replica(&event, replica, repo_prefix) {
            continue;
        }
        match event.outcome {
            ReplicaReadOutcome::Selected => {
                summary.selected_count = summary.selected_count.saturating_add(1);
                if summary
                    .last_selected_at_ms
                    .is_some_and(|timestamp| timestamp > event.timestamp_ms)
                {
                    continue;
                }
                summary.last_selected_at_ms = Some(event.timestamp_ms);
                summary.last_selected_operation = Some(event.operation);
            }
            ReplicaReadOutcome::Fallback => {
                summary.fallback_count = summary.fallback_count.saturating_add(1);
                if summary
                    .last_fallback_at_ms
                    .is_some_and(|timestamp| timestamp > event.timestamp_ms)
                {
                    continue;
                }
                summary.last_fallback_at_ms = Some(event.timestamp_ms);
                summary.last_fallback_operation = Some(event.operation);
                summary.last_fallback_class =
                    Some(ReplicaFallbackClass::from_reason(event.reason.as_deref()));
                summary.last_fallback_reason = event.reason;
            }
            ReplicaReadOutcome::PrimaryFallbackRead => {
                summary.primary_fallback_bytes = summary
                    .primary_fallback_bytes
                    .saturating_add(event.primary_fallback_bytes.unwrap_or(0));
            }
        }
    }
    summary
}

fn event_matches_replica(
    event: &ReplicaReadEvent,
    replica: &ReplicaConfig,
    repo_prefix: &str,
) -> bool {
    event.version == READ_EVENT_VERSION
        && event.replica_name == replica.name
        && event.provider == replica.provider
        && event.url == replica.url
        && event.region == replica.region
        && event.repo_prefix == repo_prefix
}

fn replica_event_log_path(replica: &ReplicaConfig, repo_prefix: &str) -> PathBuf {
    replica_cache_dir(replica, repo_prefix).join("events.jsonl")
}

fn replica_cache_dir(replica: &ReplicaConfig, repo_prefix: &str) -> PathBuf {
    replication_cache_base()
        .join("replication")
        .join(sanitize_cache_component(&replica.name))
        .join(readiness_cache_fingerprint(replica, repo_prefix))
}

fn replication_cache_base() -> PathBuf {
    std::env::var_os("CRAB_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join("crab"))
        })
        .unwrap_or_else(|| PathBuf::from(".crab-cache"))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn readiness_cache_fingerprint(replica: &ReplicaConfig, repo_prefix: &str) -> String {
    let material = format!(
        "{}\n{}\n{}\n{}\n{}",
        replica.name,
        replica.provider.as_str(),
        replica.url,
        replica.region,
        repo_prefix
    );
    let hex = blake3::hash(material.as_bytes()).to_hex().to_string();
    hex[..16].to_owned()
}

fn sanitize_cache_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Discover the project config path from a repository root.
#[must_use]
pub fn project_config_path(root: &Path) -> PathBuf {
    let mut current = root;
    loop {
        let candidate = current.join(".crab.toml");
        if candidate.is_file() {
            return candidate;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return root.join(".crab.toml"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::manifest::write_manifest_cas;
    use std::collections::{HashMap, HashSet};
    use std::fmt;
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bytes::Bytes;
    use crab_xet::shard::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader};
    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };

    use crate::metadata::manifest::{
        Manifest, PackManifestEntry, compact_pack_index, compact_shard_index, create_manifest,
    };
    use crate::metadata::segmented;
    use crab_coordination::write_coordinator::{
        CoordinatorCheckState, CoordinatorControlPlaneCheck, CoordinatorControlPlaneStatus,
        ManagedCoordinatorProvider,
    };
    use crab_xet::shard::ShardWriter;

    struct TestControlPlaneBackend {
        provider: ReplicationProviderKind,
        state: ControlPlaneCheckState,
        backend_available: bool,
        checked_drift: bool,
        replica_name: Option<String>,
        validation_state: Option<ControlPlaneCheckState>,
    }

    impl TestControlPlaneBackend {
        fn verified(provider: ReplicationProviderKind) -> Self {
            Self {
                provider,
                state: ControlPlaneCheckState::Verified,
                backend_available: true,
                checked_drift: true,
                replica_name: None,
                validation_state: None,
            }
        }

        fn drifted(provider: ReplicationProviderKind) -> Self {
            Self {
                provider,
                state: ControlPlaneCheckState::Drifted,
                backend_available: true,
                checked_drift: true,
                replica_name: None,
                validation_state: None,
            }
        }

        fn missing(provider: ReplicationProviderKind) -> Self {
            Self {
                provider,
                state: ControlPlaneCheckState::Missing,
                backend_available: true,
                checked_drift: true,
                replica_name: None,
                validation_state: None,
            }
        }

        fn unchecked(provider: ReplicationProviderKind) -> Self {
            Self {
                provider,
                state: ControlPlaneCheckState::Unknown,
                backend_available: true,
                checked_drift: false,
                replica_name: None,
                validation_state: None,
            }
        }

        fn for_replica(mut self, replica_name: &str) -> Self {
            self.replica_name = Some(replica_name.to_owned());
            self
        }

        fn with_validation_state(mut self, state: ControlPlaneCheckState) -> Self {
            self.validation_state = Some(state);
            self
        }
    }

    #[async_trait]
    impl ReplicationControlPlaneBackend for TestControlPlaneBackend {
        fn provider(&self) -> ReplicationProviderKind {
            self.provider
        }

        async fn apply(
            &self,
            plan: &ReplicationControlPlanePlan,
        ) -> Result<ControlPlaneApplyStatus> {
            Ok(ControlPlaneApplyStatus {
                provider: plan.setup.provider,
                applied: true,
                checked_drift: true,
                actions: plan
                    .requests
                    .iter()
                    .map(|request| request.action.clone())
                    .collect(),
                message: "applied through test backend".to_owned(),
            })
        }

        async fn status(&self, plan: &ReplicationControlPlanePlan) -> Result<ControlPlaneStatus> {
            Ok(ControlPlaneStatus {
                provider: plan.setup.provider,
                replica_name: self
                    .replica_name
                    .clone()
                    .unwrap_or_else(|| plan.ownership.replica_name.clone()),
                primary: plan.ownership.primary.clone(),
                replica: plan.ownership.replica.clone(),
                backend_available: self.backend_available,
                checked_drift: self.checked_drift,
                checks: plan
                    .requests
                    .iter()
                    .map(|request| {
                        let state = if base_control_plane_action(&request.action)
                            .starts_with("validate-")
                        {
                            self.validation_state.unwrap_or(self.state)
                        } else {
                            self.state
                        };
                        control_plane_check(
                            request,
                            state,
                            format!("{} checked by test backend", request.managed_resource_id),
                            "repair provider replication through crab replica add --apply",
                        )
                    })
                    .collect(),
            })
        }

        async fn remove(
            &self,
            plan: &ReplicationControlPlanePlan,
        ) -> Result<ControlPlaneApplyStatus> {
            Ok(ControlPlaneApplyStatus {
                provider: plan.setup.provider,
                applied: true,
                checked_drift: true,
                actions: plan
                    .requests
                    .iter()
                    .map(|request| request.action.clone())
                    .collect(),
                message: "removed through test backend".to_owned(),
            })
        }
    }

    #[derive(Clone, Default)]
    struct TestS3ControlPlaneClient {
        state: Arc<TestS3ControlPlaneState>,
    }

    #[derive(Default)]
    struct TestS3ControlPlaneState {
        versioned_buckets: Mutex<HashSet<String>>,
        roles: Mutex<HashMap<String, S3ReplicationRoleState>>,
        rules: Mutex<HashMap<(String, String), S3ReplicationRuleState>>,
        batches: Mutex<HashMap<String, S3BatchReplicationState>>,
        policy_states: Mutex<HashMap<String, ControlPlaneCheckState>>,
        calls: Mutex<Vec<String>>,
    }

    impl TestS3ControlPlaneClient {
        fn calls(&self) -> Vec<String> {
            self.state.calls.lock().unwrap().clone()
        }

        fn set_policy_state(&self, action: &str, state: ControlPlaneCheckState) {
            self.state
                .policy_states
                .lock()
                .unwrap()
                .insert(action.to_owned(), state);
        }

        fn insert_rule(&self, source_bucket: &str, rule_id: &str, state: S3ReplicationRuleState) {
            self.state
                .rules
                .lock()
                .unwrap()
                .insert((source_bucket.to_owned(), rule_id.to_owned()), state);
        }

        fn insert_batch(&self, job_id: &str, state: S3BatchReplicationState) {
            self.state
                .batches
                .lock()
                .unwrap()
                .insert(job_id.to_owned(), state);
        }
    }

    #[async_trait]
    impl S3ReplicationControlPlaneClient for TestS3ControlPlaneClient {
        async fn bucket_versioning_enabled(&self, bucket: &str) -> Result<bool> {
            Ok(self
                .state
                .versioned_buckets
                .lock()
                .unwrap()
                .contains(bucket))
        }

        async fn enable_bucket_versioning(&self, bucket: &str) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("enable-bucket-versioning:{bucket}"));
            self.state
                .versioned_buckets
                .lock()
                .unwrap()
                .insert(bucket.to_owned());
            Ok(())
        }

        async fn replication_role(
            &self,
            spec: &S3ReplicationRoleSpec,
        ) -> Result<Option<S3ReplicationRoleState>> {
            Ok(self
                .state
                .roles
                .lock()
                .unwrap()
                .get(&spec.role_name)
                .cloned())
        }

        async fn create_replication_role(
            &self,
            spec: &S3ReplicationRoleSpec,
        ) -> Result<S3ReplicationRoleState> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("create-role:{}", spec.role_name));
            let state = S3ReplicationRoleState {
                role_arn: format!("arn:aws:iam::123456789012:role/{}", spec.role_name),
                crab_managed: true,
                trust_policy_matches: true,
                policy_matches: true,
                source_bucket: spec.source_bucket.clone(),
                destination_bucket: spec.destination_bucket.clone(),
            };
            self.state
                .roles
                .lock()
                .unwrap()
                .insert(spec.role_name.clone(), state.clone());
            Ok(state)
        }

        async fn delete_replication_role(&self, spec: &S3ReplicationRoleSpec) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("delete-role:{}", spec.role_name));
            self.state.roles.lock().unwrap().remove(&spec.role_name);
            Ok(())
        }

        async fn replication_rule(
            &self,
            spec: &S3ReplicationRuleSpec,
        ) -> Result<Option<S3ReplicationRuleState>> {
            Ok(self
                .state
                .rules
                .lock()
                .unwrap()
                .get(&(spec.source_bucket.clone(), spec.rule_id.clone()))
                .cloned())
        }

        async fn put_replication_rule(&self, spec: &S3ReplicationRuleSpec) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("put-rule:{}", spec.rule_id));
            self.state.rules.lock().unwrap().insert(
                (spec.source_bucket.clone(), spec.rule_id.clone()),
                S3ReplicationRuleState {
                    crab_managed: true,
                    enabled: true,
                    destination_bucket: spec.destination_bucket.clone(),
                    destination_region: spec.destination_region.clone(),
                    role_arn: spec.role_arn.clone(),
                    rtc_enabled: spec.rtc_enabled,
                },
            );
            Ok(())
        }

        async fn remove_replication_rule(&self, source_bucket: &str, rule_id: &str) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("remove-rule:{rule_id}"));
            self.state
                .rules
                .lock()
                .unwrap()
                .remove(&(source_bucket.to_owned(), rule_id.to_owned()));
            Ok(())
        }

        async fn batch_replication_job(
            &self,
            spec: &S3BatchReplicationSpec,
        ) -> Result<Option<S3BatchReplicationState>> {
            Ok(self
                .state
                .batches
                .lock()
                .unwrap()
                .get(&spec.job_id)
                .cloned())
        }

        async fn create_batch_replication_job(&self, spec: &S3BatchReplicationSpec) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("create-batch:{}", spec.job_id));
            self.state.batches.lock().unwrap().insert(
                spec.job_id.clone(),
                S3BatchReplicationState {
                    job_id: spec.job_id.clone(),
                    crab_managed: true,
                    destination_bucket: spec.destination_bucket.clone(),
                    status: "Complete".to_owned(),
                    complete: true,
                },
            );
            Ok(())
        }

        async fn validate_policy(
            &self,
            spec: &S3PolicyValidationSpec,
        ) -> Result<ControlPlaneCheckState> {
            Ok(*self
                .state
                .policy_states
                .lock()
                .unwrap()
                .get(&spec.action)
                .unwrap_or(&ControlPlaneCheckState::Verified))
        }
    }

    #[derive(Clone, Default)]
    struct TestGcsControlPlaneClient {
        state: Arc<TestGcsControlPlaneState>,
    }

    #[derive(Default)]
    struct TestGcsControlPlaneState {
        buckets: Mutex<HashMap<String, GcsBucketReplicationState>>,
        backfills: Mutex<HashMap<String, GcsStorageTransferBackfillState>>,
        policy_states: Mutex<HashMap<String, ControlPlaneCheckState>>,
        rpo_metageneration_race: Mutex<HashSet<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl TestGcsControlPlaneClient {
        fn calls(&self) -> Vec<String> {
            self.state.calls.lock().unwrap().clone()
        }

        fn insert_bucket(&self, bucket: GcsBucketReplicationState) {
            self.state
                .buckets
                .lock()
                .unwrap()
                .insert(bucket.bucket.clone(), bucket);
        }

        fn insert_backfill(&self, job: GcsStorageTransferBackfillState) {
            self.state
                .backfills
                .lock()
                .unwrap()
                .insert(job.job_id.clone(), job);
        }

        fn race_rpo_metageneration_once(&self, bucket: &str) {
            self.state
                .rpo_metageneration_race
                .lock()
                .unwrap()
                .insert(bucket.to_owned());
        }
    }

    #[async_trait]
    impl GcsReplicationControlPlaneClient for TestGcsControlPlaneClient {
        async fn bucket_state(&self, bucket: &str) -> Result<Option<GcsBucketReplicationState>> {
            Ok(self.state.buckets.lock().unwrap().get(bucket).cloned())
        }

        async fn set_bucket_rpo(
            &self,
            bucket: &str,
            rpo: &str,
            if_metageneration_match: i64,
        ) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("set-rpo:{bucket}:{rpo}:{if_metageneration_match}"));
            let mut buckets = self.state.buckets.lock().unwrap();
            let Some(state) = buckets.get_mut(bucket) else {
                return Err(CrabError::Configuration {
                    key: "replication.control_plane.gcs.bucket".into(),
                    origin: format!("GCS bucket {bucket} does not exist in test backend"),
                });
            };
            if self
                .state
                .rpo_metageneration_race
                .lock()
                .unwrap()
                .remove(bucket)
            {
                state.metageneration = state.metageneration.saturating_add(1);
            }
            if state.metageneration != if_metageneration_match {
                return Err(CrabError::Configuration {
                    key: "replication.control_plane.gcs.rpo".into(),
                    origin: format!(
                        "GCS bucket {bucket} metadata changed before Crab could patch RPO"
                    ),
                });
            }
            state.rpo = Some(rpo.to_owned());
            state.metageneration = state.metageneration.saturating_add(1);
            Ok(())
        }

        async fn backfill_job(
            &self,
            spec: &GcsStorageTransferBackfillSpec,
        ) -> Result<Option<GcsStorageTransferBackfillState>> {
            Ok(self
                .state
                .backfills
                .lock()
                .unwrap()
                .get(&spec.job_id)
                .cloned())
        }

        async fn create_backfill_job(&self, spec: &GcsStorageTransferBackfillSpec) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("create-transfer:{}", spec.job_id));
            self.state.backfills.lock().unwrap().insert(
                spec.job_id.clone(),
                GcsStorageTransferBackfillState {
                    job_id: spec.job_id.clone(),
                    crab_managed: true,
                    destination_bucket: spec.destination_bucket.clone(),
                    status: "SUCCESS".to_owned(),
                    complete: true,
                    operation_name: Some(format!("transferOperations/{}", spec.job_id)),
                    objects_found: Some(1),
                    objects_copied: Some(1),
                    objects_skipped: Some(0),
                    objects_failed: Some(0),
                    bytes_found: Some(1),
                    bytes_copied: Some(1),
                    bytes_skipped: Some(0),
                    bytes_failed: Some(0),
                    error_message: None,
                },
            );
            Ok(())
        }

        async fn validate_policy(
            &self,
            spec: &GcsPolicyValidationSpec,
        ) -> Result<ControlPlaneCheckState> {
            Ok(*self
                .state
                .policy_states
                .lock()
                .unwrap()
                .get(&spec.action)
                .unwrap_or(&ControlPlaneCheckState::Verified))
        }
    }

    #[derive(Clone, Default)]
    struct TestAzureControlPlaneClient {
        state: Arc<TestAzureControlPlaneState>,
    }

    #[derive(Default)]
    struct TestAzureControlPlaneState {
        accounts: Mutex<HashMap<String, AzureBlobServiceState>>,
        policies: Mutex<HashMap<String, AzureObjectReplicationPolicyState>>,
        backfills: Mutex<HashMap<String, AzureExistingBlobBackfillState>>,
        policy_states: Mutex<HashMap<String, ControlPlaneCheckState>>,
        calls: Mutex<Vec<String>>,
    }

    impl TestAzureControlPlaneClient {
        fn calls(&self) -> Vec<String> {
            self.state.calls.lock().unwrap().clone()
        }

        fn insert_account(&self, account: AzureBlobServiceState) {
            self.state
                .accounts
                .lock()
                .unwrap()
                .insert(account.account.clone(), account);
        }

        fn insert_policy(&self, policy: AzureObjectReplicationPolicyState) {
            self.state
                .policies
                .lock()
                .unwrap()
                .insert(policy.policy_id.clone(), policy);
        }

        fn insert_backfill(&self, backfill: AzureExistingBlobBackfillState) {
            self.state
                .backfills
                .lock()
                .unwrap()
                .insert(backfill.job_id.clone(), backfill);
        }
    }

    #[async_trait]
    impl AzureReplicationControlPlaneClient for TestAzureControlPlaneClient {
        async fn blob_service_state(&self, account: &str) -> Result<Option<AzureBlobServiceState>> {
            Ok(self.state.accounts.lock().unwrap().get(account).cloned())
        }

        async fn set_change_feed(&self, account: &str, enabled: bool) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("set-change-feed:{account}:{enabled}"));
            let mut accounts = self.state.accounts.lock().unwrap();
            let Some(state) = accounts.get_mut(account) else {
                return Err(CrabError::Configuration {
                    key: "replication.control_plane.azure.account".into(),
                    origin: format!(
                        "Azure storage account {account} does not exist in test backend"
                    ),
                });
            };
            state.change_feed_enabled = enabled;
            Ok(())
        }

        async fn set_blob_versioning(&self, account: &str, enabled: bool) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("set-versioning:{account}:{enabled}"));
            let mut accounts = self.state.accounts.lock().unwrap();
            let Some(state) = accounts.get_mut(account) else {
                return Err(CrabError::Configuration {
                    key: "replication.control_plane.azure.account".into(),
                    origin: format!(
                        "Azure storage account {account} does not exist in test backend"
                    ),
                });
            };
            state.versioning_enabled = enabled;
            Ok(())
        }

        async fn object_replication_policy(
            &self,
            spec: &AzureObjectReplicationPolicySpec,
        ) -> Result<Option<AzureObjectReplicationPolicyState>> {
            Ok(self
                .state
                .policies
                .lock()
                .unwrap()
                .get(&spec.policy_id)
                .cloned())
        }

        async fn put_object_replication_policy(
            &self,
            spec: &AzureObjectReplicationPolicySpec,
        ) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("put-policy:{}", spec.policy_id));
            self.state.policies.lock().unwrap().insert(
                spec.policy_id.clone(),
                AzureObjectReplicationPolicyState {
                    policy_id: spec.policy_id.clone(),
                    crab_managed: true,
                    enabled: true,
                    source_account: spec.source_account.clone(),
                    source_container: spec.source_container.clone(),
                    destination_account: spec.destination_account.clone(),
                    destination_container: spec.destination_container.clone(),
                    destination_region: spec.destination_region.clone(),
                    prefix_scope: spec.prefix_scope.clone(),
                    priority: spec.priority,
                    existing_blob_replication: spec.existing_blob_replication,
                },
            );
            Ok(())
        }

        async fn remove_object_replication_policy(
            &self,
            spec: &AzureObjectReplicationPolicySpec,
        ) -> Result<()> {
            self.state
                .calls
                .lock()
                .unwrap()
                .push(format!("remove-policy:{}", spec.policy_id));
            self.state.policies.lock().unwrap().remove(&spec.policy_id);
            Ok(())
        }

        async fn existing_blob_backfill(
            &self,
            spec: &AzureExistingBlobBackfillSpec,
        ) -> Result<Option<AzureExistingBlobBackfillState>> {
            Ok(self
                .state
                .backfills
                .lock()
                .unwrap()
                .get(&spec.job_id)
                .cloned())
        }

        async fn validate_policy(
            &self,
            spec: &AzurePolicyValidationSpec,
        ) -> Result<ControlPlaneCheckState> {
            Ok(*self
                .state
                .policy_states
                .lock()
                .unwrap()
                .get(&spec.action)
                .unwrap_or(&ControlPlaneCheckState::Verified))
        }
    }

    fn test_gcs_bucket(bucket: &str, location_type: &str) -> GcsBucketReplicationState {
        GcsBucketReplicationState {
            bucket: bucket.to_owned(),
            metageneration: 1,
            location_type: location_type.to_owned(),
            rpo: None,
            public_access_prevention_enforced: true,
            requester_pays: false,
            has_cmek: false,
            has_retention_policy: false,
            has_delete_lifecycle_rule: false,
        }
    }

    fn test_azure_account(account: &str) -> AzureBlobServiceState {
        AzureBlobServiceState {
            account: account.to_owned(),
            change_feed_enabled: false,
            versioning_enabled: false,
        }
    }

    #[test]
    fn azure_policy_spec_uses_account_container_and_relative_prefixes() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/src-container/org/repo",
            "azure://replica/dst-container/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            false,
        );

        let spec = azure_object_replication_policy_spec(&plan).unwrap();

        assert_eq!(spec.source_account, "primary");
        assert_eq!(spec.source_container, "src-container");
        assert_eq!(spec.destination_account, "replica");
        assert_eq!(spec.destination_container, "dst-container");
        assert_eq!(spec.prefix_scope, vec!["org/repo/", ".crab/"]);
    }

    #[test]
    fn azure_backfill_spec_uses_source_and_destination_prefixes() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/src-container/org/repo",
            "azure://replica/dst-container/mirror/repo",
            "westus2",
            ReplicationRpo::Standard,
            true,
        );

        let spec = azure_existing_blob_backfill_spec(&plan).unwrap();

        assert_eq!(spec.source_account, "primary");
        assert_eq!(spec.source_container, "src-container");
        assert_eq!(spec.destination_account, "replica");
        assert_eq!(spec.destination_container, "dst-container");
        assert_eq!(spec.prefix_scope, vec!["org/repo/", ".crab/"]);
        assert_eq!(
            spec.destination_prefix_scope,
            vec!["mirror/repo/", ".crab/"]
        );
    }

    #[tokio::test]
    async fn azure_backfill_verifier_requires_destination_object_set() {
        let source = Store::new(Arc::new(InMemory::new()));
        let destination = Store::new(Arc::new(InMemory::new()));
        let spec = AzureExistingBlobBackfillSpec {
            job_id: "crab-replication-west-existing-blob-backfill".to_owned(),
            source_account: "primary".to_owned(),
            source_container: "src-container".to_owned(),
            destination_account: "replica".to_owned(),
            destination_container: "dst-container".to_owned(),
            prefix_scope: vec!["org/repo/".to_owned(), ".crab/".to_owned()],
            destination_prefix_scope: vec!["mirror/repo/".to_owned(), ".crab/".to_owned()],
        };
        source
            .put(
                &ObjectPath::from("org/repo/manifest"),
                Bytes::from_static(b"manifest"),
            )
            .await
            .unwrap();
        source
            .put(
                &ObjectPath::from("org/repo/packs/pack-a.pack"),
                Bytes::from_static(b"pack"),
            )
            .await
            .unwrap();
        source
            .put(
                &ObjectPath::from(".crab/xorbs/aa"),
                Bytes::from_static(b"xorb"),
            )
            .await
            .unwrap();
        destination
            .put(
                &ObjectPath::from("mirror/repo/manifest"),
                Bytes::from_static(b"manifest"),
            )
            .await
            .unwrap();
        destination
            .put(
                &ObjectPath::from(".crab/xorbs/aa"),
                Bytes::from_static(b"xorb"),
            )
            .await
            .unwrap();

        let incomplete = verify_existing_object_backfill(&source, &destination, &spec)
            .await
            .unwrap();

        assert_eq!(incomplete.objects_checked, 3);
        assert_eq!(incomplete.missing_objects, 1);
        assert_eq!(
            incomplete.first_missing.as_deref(),
            Some("mirror/repo/packs/pack-a.pack")
        );

        destination
            .put(
                &ObjectPath::from("mirror/repo/packs/pack-a.pack"),
                Bytes::from_static(b"pack"),
            )
            .await
            .unwrap();
        let complete = verify_existing_object_backfill(&source, &destination, &spec)
            .await
            .unwrap();

        assert_eq!(complete.objects_checked, 3);
        assert_eq!(complete.missing_objects, 0);
        assert!(complete.first_missing.is_none());
    }

    #[tokio::test]
    async fn azure_backfill_verifier_fails_closed_for_root_prefix() {
        let source = Store::new(Arc::new(InMemory::new()));
        let destination = Store::new(Arc::new(InMemory::new()));
        let spec = AzureExistingBlobBackfillSpec {
            job_id: "crab-replication-west-existing-blob-backfill".to_owned(),
            source_account: "primary".to_owned(),
            source_container: "src-container".to_owned(),
            destination_account: "replica".to_owned(),
            destination_container: "dst-container".to_owned(),
            prefix_scope: Vec::new(),
            destination_prefix_scope: Vec::new(),
        };

        let verification = verify_existing_object_backfill(&source, &destination, &spec)
            .await
            .unwrap();

        assert_eq!(verification.objects_checked, 0);
        assert_eq!(verification.missing_objects, 1);
        assert_eq!(
            verification.first_missing.as_deref(),
            Some("root-prefix-inventory-required")
        );
    }

    #[test]
    fn azure_replication_url_requires_account_and_container() {
        let err = azure_storage_target_from_url("azure://primary").unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("account/container"));
    }

    #[cfg(feature = "replication-azure-control-plane")]
    #[test]
    fn azure_sdk_policy_model_uses_container_rule_and_prefixes() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/src-container/org/repo",
            "azure://replica/dst-container/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            false,
        );
        let spec = azure_object_replication_policy_spec(&plan).unwrap();
        let model = azure_object_replication_policy_model(&spec);
        let properties = model.properties.unwrap();
        let rule = properties.rules.into_iter().next().unwrap();

        assert_eq!(properties.source_account, "primary");
        assert_eq!(properties.destination_account, "replica");
        assert_eq!(rule.source_container, "src-container");
        assert_eq!(rule.destination_container, "dst-container");
        assert_eq!(
            rule.filters.unwrap().prefix_match,
            vec!["org/repo/".to_owned(), ".crab/".to_owned()]
        );
    }

    #[cfg(feature = "replication-azure-control-plane")]
    #[test]
    fn azure_lifecycle_delete_covering_crab_prefix_is_drift() {
        let policy = azure_delete_lifecycle_policy("src-container/org/repo/");
        let prefix_scope = vec!["org/repo/".to_owned(), ".crab/".to_owned()];

        let state = azure_lifecycle_policy_check_state(
            &prefix_scope,
            "src-container",
            Some(&policy),
            &prefix_scope,
            "dst-container",
            None,
        );

        assert_eq!(state, ControlPlaneCheckState::Drifted);
    }

    #[cfg(feature = "replication-azure-control-plane")]
    #[test]
    fn azure_lifecycle_delete_outside_crab_prefix_is_verified() {
        let policy = azure_delete_lifecycle_policy("src-container/other/");
        let prefix_scope = vec!["org/repo/".to_owned(), ".crab/".to_owned()];

        let state = azure_lifecycle_policy_check_state(
            &prefix_scope,
            "src-container",
            Some(&policy),
            &prefix_scope,
            "dst-container",
            None,
        );

        assert_eq!(state, ControlPlaneCheckState::Verified);
    }

    #[cfg(feature = "replication-azure-control-plane")]
    fn azure_delete_lifecycle_policy(prefix: &str) -> azure_models::ManagementPolicy {
        let mut base_blob = azure_models::ManagementPolicyBaseBlob::new();
        base_blob.delete = Some(azure_models::DateAfterModification {
            days_after_modification_greater_than: Some(1.0),
            ..Default::default()
        });
        let mut filter = azure_models::ManagementPolicyFilter::new(vec!["blockBlob".to_owned()]);
        filter.prefix_match = vec![prefix.to_owned()];
        let definition = azure_models::ManagementPolicyDefinition {
            actions: azure_models::ManagementPolicyAction {
                base_blob: Some(base_blob),
                ..Default::default()
            },
            filters: Some(filter),
        };
        let rule = azure_models::ManagementPolicyRule::new(
            "delete-crab".to_owned(),
            azure_models::management_policy_rule::Type::Lifecycle,
            definition,
        );
        let mut policy = azure_models::ManagementPolicy::new();
        policy.properties = Some(azure_models::ManagementPolicyProperties::new(
            azure_models::ManagementPolicySchema::new(vec![rule]),
        ));
        policy
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    fn test_gcs_backfill_spec() -> GcsStorageTransferBackfillSpec {
        GcsStorageTransferBackfillSpec {
            job_id: "crab-replication-west-storage-transfer-backfill".to_owned(),
            source_bucket: "primary".to_owned(),
            destination_bucket: "replica".to_owned(),
            prefix_scope: vec!["org/repo/".to_owned(), ".crab/".to_owned()],
        }
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    #[test]
    fn gcs_storage_transfer_request_is_non_destructive() {
        let spec = test_gcs_backfill_spec();
        let request = gcs_transfer_create_request(&spec, "project-a").unwrap();

        assert_eq!(
            request["name"],
            "transferJobs/crab-replication-west-storage-transfer-backfill"
        );
        assert_eq!(request["projectId"], "project-a");
        assert_eq!(
            request["transferSpec"]["gcsDataSource"]["bucketName"],
            "primary"
        );
        assert_eq!(
            request["transferSpec"]["gcsDataSink"]["bucketName"],
            "replica"
        );
        assert_eq!(
            request["transferSpec"]["objectConditions"]["includePrefixes"],
            serde_json::json!(["org/repo/", ".crab/"])
        );
        assert_eq!(
            request["transferSpec"]["transferOptions"]["overwriteWhen"],
            "DIFFERENT"
        );
        assert_eq!(
            request["transferSpec"]["transferOptions"]["deleteObjectsUniqueInSink"],
            false
        );
        assert_eq!(
            request["transferSpec"]["transferOptions"]["deleteObjectsFromSourceAfterTransfer"],
            false
        );
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    #[test]
    fn gcs_storage_transfer_state_requires_successful_operation() {
        let spec = test_gcs_backfill_spec();
        let job = gcs_transfer_create_request(&spec, "project-a").unwrap();
        let operation = serde_json::json!({
            "name": "transferOperations/123",
            "done": true,
            "metadata": {
                "status": "SUCCESS",
                "transferJobName": "transferJobs/crab-replication-west-storage-transfer-backfill",
                "counters": {
                    "objectsFoundFromSource": "10",
                    "objectsCopiedToSink": "7",
                    "objectsFromSourceSkippedBySync": "3",
                    "objectsFromSourceFailed": "0",
                    "bytesFoundFromSource": "1000",
                    "bytesCopiedToSink": "700",
                    "bytesFromSourceSkippedBySync": "300",
                    "bytesFromSourceFailed": "0"
                }
            },
            "response": {},
        });

        let state = gcs_transfer_state_from_job(&spec, &job, Some(&operation)).unwrap();

        assert!(gcs_backfill_state_matches(&spec, &state));
        assert_eq!(state.status, "SUCCESS");
        assert_eq!(
            state.operation_name.as_deref(),
            Some("transferOperations/123")
        );
        assert_eq!(state.objects_found, Some(10));
        assert_eq!(state.objects_copied, Some(7));
        assert_eq!(state.objects_skipped, Some(3));
        assert_eq!(gcs_backfill_progress_percent(&state), Some(100));
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    #[test]
    fn gcs_storage_transfer_state_surfaces_failed_operation_details() {
        let spec = test_gcs_backfill_spec();
        let job = gcs_transfer_create_request(&spec, "project-a").unwrap();
        let operation = serde_json::json!({
            "name": "transferOperations/failed",
            "done": true,
            "metadata": {
                "counters": {
                    "objectsFoundFromSource": 10,
                    "objectsCopiedToSink": 4,
                    "objectsFromSourceSkippedBySync": 1,
                    "objectsFromSourceFailed": 5
                }
            },
            "error": { "message": "permission denied on source object" },
        });

        let state = gcs_transfer_state_from_job(&spec, &job, Some(&operation)).unwrap();

        assert_eq!(state.status, "FAILED");
        assert_eq!(state.objects_failed, Some(5));
        assert_eq!(
            state.error_message.as_deref(),
            Some("permission denied on source object")
        );
        assert_eq!(gcs_backfill_progress_percent(&state), Some(50));
        assert!(
            gcs_backfill_remediation(ControlPlaneCheckState::Missing, Some(&state))
                .contains("service agent")
        );
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    #[test]
    fn gcs_storage_transfer_state_rejects_drifted_destination() {
        let spec = test_gcs_backfill_spec();
        let mut job = gcs_transfer_create_request(&spec, "project-a").unwrap();
        job["transferSpec"]["gcsDataSink"]["bucketName"] = serde_json::json!("other-replica");
        let operation = serde_json::json!({
            "name": "transferOperations/123",
            "done": true,
            "metadata": { "status": "SUCCESS" },
            "response": {},
        });

        let state = gcs_transfer_state_from_job(&spec, &job, Some(&operation)).unwrap();

        assert!(!gcs_backfill_state_matches(&spec, &state));
        assert!(state.complete);
    }

    #[cfg(feature = "replication-gcs-control-plane")]
    #[test]
    fn gcs_storage_transfer_prefixes_reject_overlapping_scope() {
        let mut spec = test_gcs_backfill_spec();
        spec.prefix_scope = vec!["org/".to_owned(), "org/repo/".to_owned()];

        let err = gcs_transfer_create_request(&spec, "project-a").unwrap_err();

        assert!(err.to_string().contains("is a prefix of another"));
    }

    #[test]
    fn s3_fast_plan_includes_rtc_and_backfill() {
        let plan = setup_plan(
            ReplicationProviderKind::S3,
            "crab://primary/repo",
            "s3://replica/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            true,
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| a.description.contains("Replication Time Control"))
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| a.description.contains("Batch Replication"))
        );
    }

    #[test]
    fn gcs_backfill_plan_includes_storage_transfer_check() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Gcs,
            "gs://primary/org/repo",
            "gs://replica/org/repo",
            "nam4",
            ReplicationRpo::Fast,
            true,
        );
        let status = inspect_control_plane_plan(&plan);

        assert!(
            plan.requests
                .iter()
                .any(|request| request.action == "create-storage-transfer-backfill-job")
        );
        assert!(
            status
                .checks
                .iter()
                .any(|check| check.code == "provider.gcs.backfill.unverified")
        );
    }

    #[test]
    fn azure_backfill_plan_tracks_existing_blobs() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            true,
        );
        let status = inspect_control_plane_plan(&plan);

        assert!(
            plan.requests
                .iter()
                .any(|request| request.action == "track-existing-blob-backfill")
        );
        assert!(
            status
                .checks
                .iter()
                .any(|check| check.code == "provider.azure.backfill.unverified")
        );
    }

    #[test]
    fn provider_policy_validation_checks_are_planned() {
        let cases = [
            (
                ReplicationProviderKind::S3,
                "s3://primary/org/repo",
                "s3://replica/org/repo",
                "us-west-2",
                vec![
                    "provider.s3.replication-permissions.unverified",
                    "provider.s3.encryption.unverified",
                    "provider.s3.lifecycle-retention.unverified",
                    "provider.s3.immutability.unverified",
                    "provider.s3.public-access.unverified",
                    "provider.s3.requester-pays.unverified",
                    "provider.s3.cross-account-ownership.unverified",
                ],
            ),
            (
                ReplicationProviderKind::Gcs,
                "gs://primary/org/repo",
                "gs://replica/org/repo",
                "nam4",
                vec![
                    "provider.gcs.replication-permissions.unverified",
                    "provider.gcs.encryption.unverified",
                    "provider.gcs.lifecycle-retention.unverified",
                    "provider.gcs.public-access.unverified",
                    "provider.gcs.requester-pays.unverified",
                ],
            ),
            (
                ReplicationProviderKind::Azure,
                "azure://primary/org/repo",
                "azure://replica/org/repo",
                "westus2",
                vec![
                    "provider.azure.replication-permissions.unverified",
                    "provider.azure.encryption.unverified",
                    "provider.azure.lifecycle-retention.unverified",
                    "provider.azure.immutability.unverified",
                    "provider.azure.public-access.unverified",
                    "provider.azure.cross-tenant-replication.unverified",
                ],
            ),
        ];

        for (provider, primary, replica, region, expected_codes) in cases {
            let plan = control_plane_plan(
                "west",
                provider,
                primary,
                replica,
                region,
                ReplicationRpo::Standard,
                false,
            );
            let status = inspect_control_plane_plan(&plan);

            for expected_code in expected_codes {
                assert!(
                    status
                        .checks
                        .iter()
                        .any(|check| check.code == expected_code),
                    "missing {expected_code} in {status:?}"
                );
            }
        }
    }

    #[test]
    fn control_plane_plan_marks_crab_owned_s3_requests() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            true,
        );

        assert_eq!(plan.ownership.owner, "crab");
        assert!(
            plan.requests
                .iter()
                .any(|request| request.action == "put-replication-configuration")
        );
        assert!(
            plan.requests
                .iter()
                .any(|request| request.action == "create-replication-role")
        );
        assert!(
            plan.requests
                .iter()
                .any(|request| request.request.to_string().contains("crab:managed"))
        );
    }

    #[test]
    fn s3_control_plane_orders_role_before_replication_rule() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            false,
        );

        let role_index = plan
            .requests
            .iter()
            .position(|request| request.action == "create-replication-role")
            .unwrap();
        let rule_index = plan
            .requests
            .iter()
            .position(|request| request.action == "put-replication-configuration")
            .unwrap();

        assert!(role_index < rule_index);
        assert!(
            plan.requests
                .iter()
                .filter(|request| request.action == "put-bucket-versioning")
                .all(|request| !request.reversible)
        );
    }

    #[test]
    fn control_plane_remove_only_includes_reversible_requests() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Standard,
        };

        let plan = control_plane_remove_plan(&replica, "s3://primary/org/repo");

        assert!(!plan.requests.is_empty());
        assert!(
            plan.requests
                .iter()
                .all(|request| request.reversible && request.action.starts_with("remove:"))
        );
        assert_eq!(
            plan.requests
                .iter()
                .map(|request| request.action.as_str())
                .collect::<Vec<_>>(),
            vec![
                "remove:put-replication-configuration",
                "remove:create-replication-role"
            ]
        );
    }

    #[test]
    fn s3_batch_prefix_scope_resolves_repo_placeholder() {
        let prefixes = s3_resolved_prefix_scope(
            "s3://primary/org/repo",
            vec!["{repo}/".to_owned(), ".crab/".to_owned()],
        )
        .unwrap();

        assert_eq!(prefixes, vec!["org/repo/", ".crab/"]);
    }

    #[cfg(feature = "replication-s3-control-plane")]
    #[test]
    fn s3_batch_tag_match_requires_crab_ownership() {
        let spec = S3BatchReplicationSpec {
            replica_name: "west".to_owned(),
            job_id: "crab-replication-west-batch-replication".to_owned(),
            source_bucket: "primary".to_owned(),
            destination_bucket: "replica".to_owned(),
            role_arn: "arn:aws:iam::123456789012:role/crab-replication-west-replication-role"
                .to_owned(),
            prefix_scope: vec!["org/repo/".to_owned(), ".crab/".to_owned()],
        };
        let mut tags = s3_batch_tags(&spec).unwrap();

        assert!(s3_batch_tags_match(&spec, &tags));

        tags.retain(|tag| tag.key() != "crab:replica");

        assert!(!s3_batch_tags_match(&spec, &tags));
    }

    #[cfg(not(feature = "replication-azure-control-plane"))]
    #[tokio::test]
    async fn control_plane_apply_fails_closed_without_backend() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Fast,
            false,
        );

        let err = apply_control_plane_plan(&plan).await.unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn control_plane_backend_apply_and_status_verify_resources() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            true,
        );
        let backend = TestControlPlaneBackend::verified(ReplicationProviderKind::S3);

        let apply = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(apply.applied);
        assert_eq!(apply.actions.len(), plan.requests.len());
        assert!(status.backend_available);
        assert!(status.checked_drift);
        assert!(
            status
                .checks
                .iter()
                .all(|check| check.state == ControlPlaneCheckState::Verified)
        );
    }

    #[tokio::test]
    async fn control_plane_backend_provider_mismatch_fails() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            false,
        );
        let backend = TestControlPlaneBackend::verified(ReplicationProviderKind::Azure);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn control_plane_apply_allows_missing_resources_after_drift_check() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let backend = TestControlPlaneBackend::missing(ReplicationProviderKind::S3)
            .with_validation_state(ControlPlaneCheckState::Verified);

        let apply = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(apply.applied);
    }

    #[tokio::test]
    async fn control_plane_apply_rejects_missing_policy_validation_proof() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let backend = TestControlPlaneBackend::missing(ReplicationProviderKind::S3);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("missing safety proof"));
    }

    #[tokio::test]
    async fn s3_backend_apply_creates_missing_resources_and_verifies_status() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            true,
        );
        let client = TestS3ControlPlaneClient::default();
        let backend = S3ReplicationControlPlaneBackend::new(client.clone());

        let apply = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(apply.applied);
        assert!(
            status
                .checks
                .iter()
                .all(|check| check.state == ControlPlaneCheckState::Verified)
        );
        assert_eq!(
            client.calls(),
            vec![
                "enable-bucket-versioning:primary",
                "enable-bucket-versioning:replica",
                "create-role:crab-replication-west-replication-role",
                "put-rule:crab-replication-west-replication-rule",
                "create-batch:crab-replication-west-batch-replication",
            ]
        );
    }

    #[tokio::test]
    async fn s3_backend_status_blocks_incomplete_batch_replication_job() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            true,
        );
        let client = TestS3ControlPlaneClient::default();
        let backend = S3ReplicationControlPlaneBackend::new(client.clone());
        apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        client.insert_batch(
            "crab-replication-west-batch-replication",
            S3BatchReplicationState {
                job_id: "aws-job-123".to_owned(),
                crab_managed: true,
                destination_bucket: "replica".to_owned(),
                status: "Active".to_owned(),
                complete: false,
            },
        );

        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        let batch_check = status
            .checks
            .iter()
            .find(|check| check.action == "create-batch-replication-job")
            .unwrap();
        assert_eq!(batch_check.state, ControlPlaneCheckState::Missing);
        assert!(batch_check.message.contains("Active"));
    }

    #[tokio::test]
    async fn s3_backend_status_detects_replication_rule_drift() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let client = TestS3ControlPlaneClient::default();
        let backend = S3ReplicationControlPlaneBackend::new(client.clone());
        apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        client.insert_rule(
            "primary",
            "crab-replication-west-replication-rule",
            S3ReplicationRuleState {
                crab_managed: true,
                enabled: true,
                destination_bucket: "other-replica".to_owned(),
                destination_region: "us-west-2".to_owned(),
                role_arn: "arn:aws:iam::123456789012:role/crab-replication-west-replication-role"
                    .to_owned(),
                rtc_enabled: false,
            },
        );

        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        let rule_check = status
            .checks
            .iter()
            .find(|check| check.action == "put-replication-configuration")
            .unwrap();
        assert_eq!(rule_check.state, ControlPlaneCheckState::Drifted);
    }

    #[tokio::test]
    async fn s3_backend_apply_rejects_failed_policy_validation() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let client = TestS3ControlPlaneClient::default();
        client.set_policy_state(
            "validate-encryption-compatibility",
            ControlPlaneCheckState::Unsupported,
        );
        let backend = S3ReplicationControlPlaneBackend::new(client);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn gcs_backend_apply_sets_turbo_rpo_and_verifies_status() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Gcs,
            "gs://primary/org/repo",
            "gs://replica/org/repo",
            "nam4",
            ReplicationRpo::Fast,
            false,
        );
        let client = TestGcsControlPlaneClient::default();
        client.insert_bucket(test_gcs_bucket("primary", "DUAL_REGION"));
        client.insert_bucket(test_gcs_bucket("replica", "DUAL_REGION"));
        let backend = GcsReplicationControlPlaneBackend::new(client.clone());

        let apply = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(apply.applied);
        assert!(
            status
                .checks
                .iter()
                .all(|check| check.state == ControlPlaneCheckState::Verified)
        );
        assert_eq!(client.calls(), vec!["set-rpo:replica:ASYNC_TURBO:1"]);
    }

    #[tokio::test]
    async fn gcs_backend_apply_uses_metageneration_precondition_for_turbo_rpo() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Gcs,
            "gs://primary/org/repo",
            "gs://replica/org/repo",
            "nam4",
            ReplicationRpo::Fast,
            false,
        );
        let client = TestGcsControlPlaneClient::default();
        client.insert_bucket(test_gcs_bucket("primary", "DUAL_REGION"));
        client.insert_bucket(test_gcs_bucket("replica", "DUAL_REGION"));
        client.race_rpo_metageneration_once("replica");
        let backend = GcsReplicationControlPlaneBackend::new(client.clone());

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();
        let replica = client.bucket_state("replica").await.unwrap().unwrap();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("metadata changed"));
        assert_eq!(replica.rpo, None);
        assert_eq!(client.calls(), vec!["set-rpo:replica:ASYNC_TURBO:1"]);
    }

    #[tokio::test]
    async fn gcs_backend_rejects_turbo_rpo_on_non_dual_region_bucket() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Gcs,
            "gs://primary/org/repo",
            "gs://replica/org/repo",
            "us",
            ReplicationRpo::Fast,
            false,
        );
        let client = TestGcsControlPlaneClient::default();
        client.insert_bucket(test_gcs_bucket("primary", "MULTI_REGION"));
        client.insert_bucket(test_gcs_bucket("replica", "MULTI_REGION"));
        let backend = GcsReplicationControlPlaneBackend::new(client);

        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        let rpo_check = status
            .checks
            .iter()
            .find(|check| check.action == "patch-bucket-rpo")
            .unwrap();
        assert_eq!(rpo_check.state, ControlPlaneCheckState::Unsupported);
        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn gcs_backend_apply_rejects_incomplete_storage_transfer_backfill() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Gcs,
            "gs://primary/org/repo",
            "gs://replica/org/repo",
            "nam4",
            ReplicationRpo::Standard,
            true,
        );
        let client = TestGcsControlPlaneClient::default();
        client.insert_bucket(test_gcs_bucket("primary", "DUAL_REGION"));
        client.insert_bucket(test_gcs_bucket("replica", "DUAL_REGION"));
        client.insert_backfill(GcsStorageTransferBackfillState {
            job_id: "crab-replication-west-storage-transfer-backfill".to_owned(),
            crab_managed: true,
            destination_bucket: "replica".to_owned(),
            status: "IN_PROGRESS".to_owned(),
            complete: false,
            operation_name: Some("transferOperations/in-progress".to_owned()),
            objects_found: Some(10),
            objects_copied: Some(4),
            objects_skipped: Some(2),
            objects_failed: Some(0),
            bytes_found: Some(1000),
            bytes_copied: Some(400),
            bytes_skipped: Some(200),
            bytes_failed: Some(0),
            error_message: None,
        });
        let backend = GcsReplicationControlPlaneBackend::new(client);

        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        let check = status
            .checks
            .iter()
            .find(|check| check.action == "create-storage-transfer-backfill-job")
            .unwrap();
        assert_eq!(check.progress_percent, Some(60));
        assert!(check.message.contains("objects copied 4/10"));
        assert!(check.remediation.contains("service agent"));
        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("has not completed"));
    }

    #[test]
    fn gcs_remove_plan_does_not_revert_bucket_level_turbo_rpo() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::Gcs,
            url: "gs://replica/org/repo".into(),
            region: "nam4".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Fast,
        };

        let plan = control_plane_remove_plan(&replica, "gs://primary/org/repo");

        assert!(plan.requests.is_empty());
    }

    #[tokio::test]
    async fn azure_backend_apply_sets_prerequisites_policy_and_verifies_status() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Fast,
            false,
        );
        let client = TestAzureControlPlaneClient::default();
        client.insert_account(test_azure_account("primary"));
        client.insert_account(test_azure_account("replica"));
        let backend = AzureReplicationControlPlaneBackend::new(client.clone());

        let apply = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();
        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(apply.applied);
        assert!(
            status
                .checks
                .iter()
                .all(|check| check.state == ControlPlaneCheckState::Verified)
        );
        assert_eq!(
            client.calls(),
            vec![
                "set-change-feed:primary:true",
                "set-versioning:primary:true",
                "set-versioning:replica:true",
                "put-policy:crab-replication-west-object-replication-policy",
            ]
        );
    }

    #[tokio::test]
    async fn azure_backend_apply_rejects_drifted_object_replication_policy() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            false,
        );
        let client = TestAzureControlPlaneClient::default();
        let mut primary = test_azure_account("primary");
        primary.change_feed_enabled = true;
        primary.versioning_enabled = true;
        let mut replica = test_azure_account("replica");
        replica.versioning_enabled = true;
        client.insert_account(primary);
        client.insert_account(replica);
        let spec = azure_object_replication_policy_spec(&plan).unwrap();
        client.insert_policy(AzureObjectReplicationPolicyState {
            policy_id: spec.policy_id,
            crab_managed: true,
            enabled: true,
            source_account: spec.source_account,
            source_container: spec.source_container,
            destination_account: "other-replica".to_owned(),
            destination_container: spec.destination_container,
            destination_region: spec.destination_region,
            prefix_scope: spec.prefix_scope,
            priority: spec.priority,
            existing_blob_replication: spec.existing_blob_replication,
        });
        let backend = AzureReplicationControlPlaneBackend::new(client);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
    }

    #[tokio::test]
    async fn azure_backend_status_blocks_incomplete_existing_blob_backfill() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            true,
        );
        let client = TestAzureControlPlaneClient::default();
        let mut primary = test_azure_account("primary");
        primary.change_feed_enabled = true;
        primary.versioning_enabled = true;
        let mut replica = test_azure_account("replica");
        replica.versioning_enabled = true;
        client.insert_account(primary);
        client.insert_account(replica);
        let spec = azure_object_replication_policy_spec(&plan).unwrap();
        client.insert_policy(AzureObjectReplicationPolicyState {
            policy_id: spec.policy_id.clone(),
            crab_managed: true,
            enabled: true,
            source_account: spec.source_account,
            source_container: spec.source_container,
            destination_account: spec.destination_account.clone(),
            destination_container: spec.destination_container.clone(),
            destination_region: spec.destination_region,
            prefix_scope: spec.prefix_scope,
            priority: spec.priority,
            existing_blob_replication: spec.existing_blob_replication,
        });
        client.insert_backfill(AzureExistingBlobBackfillState {
            job_id: "crab-replication-west-existing-blob-backfill".to_owned(),
            crab_managed: true,
            destination_account: spec.destination_account,
            destination_container: spec.destination_container,
            status: "in_progress".to_owned(),
            complete: false,
            objects_checked: 10,
            missing_objects: 1,
            first_missing: Some("org/repo/packs/missing.pack".to_owned()),
        });
        let backend = AzureReplicationControlPlaneBackend::new(client);

        let status = inspect_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        let backfill_check = status
            .checks
            .iter()
            .find(|check| check.action == "track-existing-blob-backfill")
            .unwrap();
        assert_eq!(backfill_check.state, ControlPlaneCheckState::Missing);
        assert!(backfill_check.message.contains("in_progress"));
        assert!(
            backfill_check
                .message
                .contains("first missing destination object")
        );
        assert!(backfill_check.remediation.contains("HEAD the destination"));
        assert_eq!(backfill_check.progress_percent, Some(90));
    }

    #[test]
    fn azure_remove_plan_preserves_account_level_toggles() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::Azure,
            url: "azure://replica/org/repo".into(),
            region: "westus2".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Fast,
        };

        let plan = control_plane_remove_plan(&replica, "azure://primary/org/repo");

        assert_eq!(
            plan.requests
                .iter()
                .map(|request| request.action.as_str())
                .collect::<Vec<_>>(),
            vec!["remove:put-object-replication-policy"]
        );
    }

    #[tokio::test]
    async fn azure_backend_remove_deletes_only_object_replication_policy() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::Azure,
            url: "azure://replica/org/repo".into(),
            region: "westus2".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Standard,
        };
        let plan = control_plane_remove_plan(&replica, "azure://primary/org/repo");
        let client = TestAzureControlPlaneClient::default();
        let spec = azure_object_replication_policy_spec(&plan).unwrap();
        client.insert_policy(AzureObjectReplicationPolicyState {
            policy_id: spec.policy_id.clone(),
            crab_managed: true,
            enabled: true,
            source_account: spec.source_account,
            source_container: spec.source_container,
            destination_account: spec.destination_account,
            destination_container: spec.destination_container,
            destination_region: spec.destination_region,
            prefix_scope: spec.prefix_scope,
            priority: spec.priority,
            existing_blob_replication: spec.existing_blob_replication,
        });
        let backend = AzureReplicationControlPlaneBackend::new(client.clone());

        let status = remove_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(
            client.calls(),
            vec!["remove-policy:crab-replication-west-object-replication-policy"]
        );
    }

    #[tokio::test]
    async fn control_plane_apply_rejects_drifted_resources() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let backend = TestControlPlaneBackend::drifted(ReplicationProviderKind::S3);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
    }

    #[tokio::test]
    async fn control_plane_apply_requires_drift_checked_status() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let backend = TestControlPlaneBackend::unchecked(ReplicationProviderKind::S3);

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("drift"));
    }

    #[tokio::test]
    async fn control_plane_apply_rejects_status_for_different_replica() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Standard,
            false,
        );
        let backend =
            TestControlPlaneBackend::verified(ReplicationProviderKind::S3).for_replica("east");

        let err = apply_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn control_plane_remove_requires_verified_status() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Standard,
        };
        let plan = control_plane_remove_plan(&replica, "s3://primary/org/repo");
        let backend = TestControlPlaneBackend::drifted(ReplicationProviderKind::S3);

        let err = remove_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn control_plane_remove_uses_verified_backend_status() {
        let replica = ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            backfill: false,
            read: false,
            rpo: ReplicationRpo::Standard,
        };
        let plan = control_plane_remove_plan(&replica, "s3://primary/org/repo");
        let backend = TestControlPlaneBackend::verified(ReplicationProviderKind::S3);

        let status = remove_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert!(
            status
                .actions
                .iter()
                .all(|action| action.starts_with("remove:"))
        );
    }

    #[test]
    fn control_plane_export_contains_request_shape() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::Azure,
            "azure://primary/org/repo",
            "azure://replica/org/repo",
            "westus2",
            ReplicationRpo::Standard,
            false,
        );

        let export = export_control_plane_plan(&plan, ControlPlaneExportFormat::Bicep).unwrap();

        assert!(export.contains("put-object-replication-policy"));
    }

    #[test]
    fn control_plane_status_reports_unverified_checks_without_backend() {
        let plan = control_plane_plan(
            "west",
            ReplicationProviderKind::S3,
            "s3://primary/org/repo",
            "s3://replica/org/repo",
            "us-west-2",
            ReplicationRpo::Fast,
            true,
        );

        let status = inspect_control_plane_plan(&plan);

        assert_eq!(status.provider, ReplicationProviderKind::S3);
        assert_eq!(status.replica_name, "west");
        assert!(!status.backend_available);
        assert!(!status.checked_drift);
        assert!(status.checks.iter().all(|check| {
            check.state == ControlPlaneCheckState::Unknown && !check.managed_resource_id.is_empty()
        }));
        assert!(
            status
                .checks
                .iter()
                .any(|check| check.code == "provider.s3.replication-rule.unverified")
        );
        assert!(
            status
                .checks
                .iter()
                .any(|check| check.code == "provider.s3.batch-replication.unverified")
        );
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(
            ReplicationProviderKind::parse("aws").unwrap(),
            ReplicationProviderKind::S3
        );
        assert_eq!(
            ReplicationProviderKind::parse("gs").unwrap(),
            ReplicationProviderKind::Gcs
        );
        assert_eq!(
            ReplicationProviderKind::parse("az").unwrap(),
            ReplicationProviderKind::Azure
        );
    }

    #[test]
    fn replica_url_provider_must_match_raw_scheme() {
        assert!(
            validate_replica_url_provider(ReplicationProviderKind::S3, "s3://bucket/repo").is_ok()
        );
        assert!(
            validate_replica_url_provider(ReplicationProviderKind::Gcs, "gs://bucket/repo").is_ok()
        );
        assert!(
            validate_replica_url_provider(ReplicationProviderKind::Azure, "s3://bucket/repo")
                .is_err()
        );
        assert!(
            validate_replica_url_provider(ReplicationProviderKind::Azure, "crab://bucket/repo")
                .is_ok()
        );
    }

    #[test]
    fn active_active_requires_coordinator_and_writer() {
        let mut replication = ReplicationConfig {
            mode: ReplicationMode::ActiveActive,
            ..ReplicationConfig::default()
        };

        assert!(validate_active_active_config(&replication).is_err());

        replication.coordinator = Some(ReplicationCoordinatorConfig {
            kind: ReplicationCoordinatorKind::Managed,
            url: "dynamodb://crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            consistency: ReplicationCoordinatorConsistency::Linearizable,
        });
        replication.writers.push(WriterConfig {
            name: "east".into(),
            url: "crab://primary/repo".into(),
            region: "us-east-1".into(),
            enabled: true,
        });

        assert!(validate_active_active_config(&replication).is_ok());
    }

    #[test]
    fn active_active_write_admission_fails_closed_without_adapter() {
        let mut config = Config::default();
        config.replication = Some(ReplicationConfig {
            mode: ReplicationMode::ActiveActive,
            coordinator: Some(ReplicationCoordinatorConfig {
                kind: ReplicationCoordinatorKind::Managed,
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: Vec::new(),
                consistency: ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![WriterConfig {
                name: "east".into(),
                url: "crab://primary/repo".into(),
                region: "us-east-1".into(),
                enabled: true,
            }],
            ..ReplicationConfig::default()
        });

        let err = ensure_active_active_write_admitted(&config).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn active_active_gc_snapshot_fails_closed_without_data_plane() {
        let mut config = Config::default();
        config.replication = Some(ReplicationConfig {
            mode: ReplicationMode::ActiveActive,
            coordinator: Some(ReplicationCoordinatorConfig {
                kind: ReplicationCoordinatorKind::Managed,
                url: "cosmosdb://crab-coordinator".into(),
                region: "eastus".into(),
                failover_regions: vec!["westus2".into()],
                consistency: ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![WriterConfig {
                name: "east".into(),
                url: "crab://primary/repo".into(),
                region: "eastus".into(),
                enabled: true,
            }],
            ..ReplicationConfig::default()
        });

        let err = active_active_gc_protected_keys(&config, "org/repo")
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("maintenance fails closed"));
    }

    #[tokio::test]
    async fn active_active_bucket_gc_requires_current_repo_registration() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());
        let registry = RefRegistry::default();

        let err = active_active_bucket_gc_protection(&config, &registry, Some("org/repo"))
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("org/repo"));
    }

    #[tokio::test]
    async fn active_active_bucket_gc_rejects_mismatched_current_repo_registration() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());
        let mut registry = RefRegistry::default();
        registry.register_active_active_coordinator(
            "org/repo",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".into(),
                url: "dynamodb://other-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
            },
        );

        let err = active_active_bucket_gc_protection(&config, &registry, Some("org/repo"))
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn active_active_bucket_gc_fails_closed_for_unwired_registered_provider() {
        let config = Config::default();
        let mut registry = RefRegistry::default();
        registry.register_active_active_coordinator(
            "org/repo",
            ActiveActiveCoordinatorRegistration {
                provider: "spanner".into(),
                url: "spanner://crab-coordinator".into(),
                region: "nam3".into(),
                failover_regions: vec!["eur3".into()],
            },
        );

        let err = active_active_bucket_gc_protection(&config, &registry, None)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("bucket maintenance fails closed"));
    }

    #[test]
    fn active_active_status_enables_writes_for_verified_coordinator() {
        let replication = active_active_replication_for_push();
        let status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://crab-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        let active_active =
            active_active_status_with_coordinator_status(Some(&replication), Some(&status));

        assert!(active_active.coordinator_ready);
        assert!(active_active.writes_enabled);
        assert!(active_active.reason.is_none());
    }

    #[test]
    fn active_active_status_blocks_missing_coordinator_resource() {
        let replication = active_active_replication_for_push();
        let status = coordinator_status(
            CoordinatorCheckState::Missing,
            "dynamodb://crab-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        let active_active =
            active_active_status_with_coordinator_status(Some(&replication), Some(&status));

        assert!(!active_active.coordinator_ready);
        assert!(!active_active.writes_enabled);
        assert!(
            active_active
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("missing"))
        );
    }

    #[test]
    fn active_active_status_blocks_mismatched_coordinator_status() {
        let replication = active_active_replication_for_push();
        let status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://other-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        let active_active =
            active_active_status_with_coordinator_status(Some(&replication), Some(&status));

        assert!(!active_active.coordinator_ready);
        assert!(!active_active.writes_enabled);
        assert!(
            active_active
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("does not match"))
        );
    }

    #[test]
    fn active_active_status_blocks_wrong_provider_for_coordinator_url() {
        let replication = active_active_replication_for_push();
        let mut status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://crab-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );
        status.provider = ManagedCoordinatorProvider::Spanner;

        let active_active =
            active_active_status_with_coordinator_status(Some(&replication), Some(&status));

        assert!(!active_active.coordinator_ready);
        assert!(!active_active.writes_enabled);
        assert!(
            active_active
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("provider"))
        );
    }

    #[test]
    fn active_active_maintenance_fails_closed_without_adapter() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());

        let err =
            ensure_active_active_maintenance_admitted(&config, "garbage collection").unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("garbage collection"));
    }

    #[test]
    fn active_active_resume_requires_repair_verified_proof() {
        let err = validate_active_active_resume_proof(ActiveActiveResumeProof {
            repair_verified: false,
        })
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("repair"));
        validate_active_active_resume_proof(ActiveActiveResumeProof::verified_after_repair())
            .unwrap();
    }

    #[test]
    fn active_active_write_admission_accepts_verified_coordinator_status() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());
        let status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://crab-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        ensure_active_active_write_admitted_with_coordinator_status(&config, &status).unwrap();
    }

    #[test]
    fn active_active_write_admission_rejects_mismatched_coordinator_status() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());
        let status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://other-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        let err = ensure_active_active_write_admitted_with_coordinator_status(&config, &status)
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("verified coordinator status"));
    }

    #[test]
    fn active_active_maintenance_admission_accepts_verified_coordinator_status() {
        let mut config = Config::default();
        config.replication = Some(active_active_replication_for_push());
        let status = coordinator_status(
            CoordinatorCheckState::Verified,
            "dynamodb://crab-coordinator",
            "us-east-1",
            &["us-west-2".to_owned()],
        );

        ensure_active_active_maintenance_admitted_with_coordinator_status(
            &config,
            "garbage collection",
            &status,
        )
        .unwrap();
    }

    #[test]
    fn maintenance_guard_allows_read_replica_mode() {
        let mut config = Config::default();
        config.replication = Some(ReplicationConfig {
            mode: ReplicationMode::ReadReplica,
            ..ReplicationConfig::default()
        });

        ensure_active_active_maintenance_admitted(&config, "garbage collection").unwrap();
    }

    #[test]
    fn active_active_push_plan_selects_first_enabled_writer() {
        let replication = active_active_replication_for_push();
        let refs = vec![active_active_ref_update(
            "refs/heads/main",
            Some("old"),
            Some("new"),
            false,
        )];

        let plan = plan_active_active_push(&replication, None, 42, refs, Vec::new()).unwrap();

        assert_eq!(plan.writer.name, "east");
        assert_eq!(plan.coordinator_url, "dynamodb://crab-coordinator");
        assert_eq!(plan.request.writer, "east");
        assert_eq!(plan.request.region, "us-east-1");
        assert_eq!(plan.request.manifest_generation, 42);
        assert_eq!(plan.request.refs.len(), 1);
        assert_eq!(
            plan.request.target_regions,
            vec!["us-east-1".to_owned(), "us-west-2".to_owned()]
        );
        assert!(plan.request.operation_id.starts_with("crab-op-"));
    }

    #[test]
    fn active_active_push_plan_uses_preferred_enabled_writer() {
        let replication = active_active_replication_for_push();

        let plan = plan_active_active_push(
            &replication,
            Some("west"),
            7,
            vec![active_active_ref_update(
                "refs/heads/feature",
                None,
                Some("new"),
                false,
            )],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(plan.writer.name, "west");
        assert_eq!(plan.request.writer, "west");
        assert_eq!(plan.request.region, "us-west-2");
    }

    #[test]
    fn active_active_push_plan_rejects_disabled_preferred_writer() {
        let replication = active_active_replication_for_push();

        let err = plan_active_active_push(
            &replication,
            Some("disabled"),
            7,
            vec![active_active_ref_update(
                "refs/heads/main",
                Some("old"),
                Some("new"),
                false,
            )],
            Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn active_active_push_plan_rejects_empty_ref_set() {
        let replication = active_active_replication_for_push();

        let err =
            plan_active_active_push(&replication, None, 7, Vec::new(), Vec::new()).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn active_active_push_operation_id_is_stable_for_ref_order() {
        let replication = active_active_replication_for_push();
        let refs = vec![
            active_active_ref_update("refs/heads/main", Some("a"), Some("b"), false),
            active_active_ref_update("refs/tags/v1", None, Some("c"), true),
        ];
        let mut reversed = refs.clone();
        reversed.reverse();

        let first =
            plan_active_active_push(&replication, Some("east"), 9, refs, Vec::new()).unwrap();
        let second =
            plan_active_active_push(&replication, Some("east"), 9, reversed, Vec::new()).unwrap();

        assert_eq!(first.request.operation_id, second.request.operation_id);
    }

    #[test]
    fn active_active_push_operation_id_changes_for_ref_content() {
        let replication = active_active_replication_for_push();

        let first = plan_active_active_push(
            &replication,
            Some("east"),
            9,
            vec![active_active_ref_update(
                "refs/heads/main",
                Some("a"),
                Some("b"),
                false,
            )],
            Vec::new(),
        )
        .unwrap();
        let second = plan_active_active_push(
            &replication,
            Some("east"),
            9,
            vec![active_active_ref_update(
                "refs/heads/main",
                Some("a"),
                Some("c"),
                false,
            )],
            Vec::new(),
        )
        .unwrap();

        assert_ne!(first.request.operation_id, second.request.operation_id);
    }

    #[test]
    fn active_active_push_operation_id_changes_for_uploaded_objects() {
        let replication = active_active_replication_for_push();
        let refs = vec![active_active_ref_update(
            "refs/heads/main",
            Some("a"),
            Some("b"),
            false,
        )];

        let first = plan_active_active_push(
            &replication,
            Some("east"),
            9,
            refs.clone(),
            vec!["xorbs/aa/one".to_owned()],
        )
        .unwrap();
        let second = plan_active_active_push(
            &replication,
            Some("east"),
            9,
            refs,
            vec!["xorbs/bb/two".to_owned()],
        )
        .unwrap();

        assert_ne!(first.request.operation_id, second.request.operation_id);
        assert_eq!(
            first.request.uploaded_objects,
            vec!["xorbs/aa/one".to_owned()]
        );
    }

    #[test]
    fn active_active_push_operation_id_changes_for_target_regions() {
        let mut replication = active_active_replication_for_push();
        let refs = vec![active_active_ref_update(
            "refs/heads/main",
            Some("a"),
            Some("b"),
            false,
        )];
        let first =
            plan_active_active_push(&replication, Some("east"), 9, refs.clone(), Vec::new())
                .unwrap();

        replication
            .writers
            .iter_mut()
            .find(|writer| writer.name == "west")
            .unwrap()
            .enabled = false;
        let second =
            plan_active_active_push(&replication, Some("east"), 9, refs, Vec::new()).unwrap();

        assert_ne!(first.request.operation_id, second.request.operation_id);
        assert_eq!(second.request.target_regions, vec!["us-east-1".to_owned()]);
    }

    #[test]
    fn active_active_repair_plan_maps_gaps_to_enabled_writers() {
        let replication = active_active_replication_for_push();
        let snapshot = coordinator_repair_snapshot(vec![
            repair_gap("op-2", "us-west-2"),
            repair_gap("op-1", "us-east-1"),
        ]);

        let plan = plan_active_active_repair(&replication, &snapshot).unwrap();

        assert_eq!(plan.coordinator_epoch, 7);
        assert_eq!(plan.actions.len(), 2);
        assert_eq!(plan.actions[0].operation_id, "op-1");
        assert_eq!(plan.actions[0].writer.name, "east");
        assert_eq!(plan.actions[1].operation_id, "op-2");
        assert_eq!(plan.actions[1].writer.name, "west");
    }

    #[test]
    fn active_active_repair_plan_rejects_disabled_writer_region() {
        let replication = active_active_replication_for_push();
        let snapshot = coordinator_repair_snapshot(vec![repair_gap("op-1", "us-west-1")]);

        let err = plan_active_active_repair(&replication, &snapshot).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("disabled"));
    }

    #[test]
    fn active_active_repair_plan_rejects_missing_writer_region() {
        let replication = active_active_replication_for_push();
        let snapshot = coordinator_repair_snapshot(vec![repair_gap("op-1", "eu-west-1")]);

        let err = plan_active_active_repair(&replication, &snapshot).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("eu-west-1"));
    }

    #[test]
    fn repair_object_key_remaps_repo_local_prefix() {
        assert_eq!(
            repair_object_key_for_target_prefix(
                "source/repo/packs/pack-a.pack",
                "source/repo",
                "target/repo"
            )
            .unwrap(),
            "target/repo/packs/pack-a.pack"
        );
        assert_eq!(
            repair_object_key_for_target_prefix(".crab/xorbs/aa", "source/repo", "target/repo")
                .unwrap(),
            ".crab/xorbs/aa"
        );
    }

    #[tokio::test]
    async fn repair_materialization_refuses_missing_transaction_object() {
        let store = Store::new(Arc::new(InMemory::new()));

        let err = verify_repair_uploaded_objects_present(
            &store,
            &["source/repo/packs/pack-a.pack".to_owned()],
            "source/repo",
            "target/repo",
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("target/repo/packs/pack-a.pack"));
    }

    #[tokio::test]
    async fn repair_materialization_writes_manifest_projection() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "target/repo".to_owned());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 42;
        manifest
            .refs
            .insert("refs/heads/main".into(), "a".repeat(40));
        manifest.seal_git_validation();

        materialize_active_active_manifest_projection(&store, &router, &manifest)
            .await
            .unwrap();

        let (written, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(written, manifest);
    }

    #[tokio::test]
    async fn repair_refuses_to_replicate_mismatched_legacy_visibility() {
        let source = Store::new(Arc::new(InMemory::new()));
        let source_router = StoreLayout::new(source.clone(), "source/repo".to_owned());
        let target = Store::new(Arc::new(InMemory::new()));
        let target_router = StoreLayout::new(target.clone(), "target/repo".to_owned());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 7;
        manifest.pack_index_hash = "a".repeat(64);
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "1".repeat(40));
        manifest.seal_git_validation();
        let legacy = serde_json::json!({
            "version": 1,
            "generation": manifest.generation,
            "pack_index_hash": manifest.pack_index_hash,
            "refs": {"refs/heads/main": ["2".repeat(40)]},
        });
        source
            .put(
                &source_router
                    .git_visibility_v1_path(manifest.generation, &manifest.pack_index_hash),
                Bytes::from(serde_json::to_vec(&legacy).unwrap()),
            )
            .await
            .unwrap();

        let error = replicate_git_visibility_index(
            &source,
            &source_router,
            &target,
            &target_router,
            &manifest,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
        assert!(matches!(
            target
                .head(&target_router.git_visibility_path(&manifest.git_validation_digest))
                .await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[test]
    fn active_active_config_rejects_duplicate_enabled_writer_regions() {
        let mut replication = active_active_replication_for_push();
        replication.writers.push(WriterConfig {
            name: "east-again".into(),
            url: "s3://another/repo".into(),
            region: "us-east-1".into(),
            enabled: true,
        });

        let err = validate_active_active_config(&replication).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("us-east-1"));
    }

    #[test]
    fn active_active_config_rejects_unsupported_coordinator_url() {
        let mut replication = active_active_replication_for_push();
        replication.coordinator.as_mut().unwrap().url = "postgres://crab-coordinator".into();

        let err = validate_active_active_config(&replication).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("postgres"));
    }

    #[test]
    fn active_active_config_rejects_empty_coordinator_name() {
        let mut replication = active_active_replication_for_push();
        replication.coordinator.as_mut().unwrap().url = "dynamodb://".into();

        let err = validate_active_active_config(&replication).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("resource name"));
    }

    fn active_active_replication_for_push() -> ReplicationConfig {
        ReplicationConfig {
            mode: ReplicationMode::ActiveActive,
            coordinator: Some(ReplicationCoordinatorConfig {
                kind: ReplicationCoordinatorKind::Managed,
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
                consistency: ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![
                WriterConfig {
                    name: "east".into(),
                    url: "crab://primary/repo".into(),
                    region: "us-east-1".into(),
                    enabled: true,
                },
                WriterConfig {
                    name: "west".into(),
                    url: "s3://replica/repo".into(),
                    region: "us-west-2".into(),
                    enabled: true,
                },
                WriterConfig {
                    name: "disabled".into(),
                    url: "s3://disabled/repo".into(),
                    region: "us-west-1".into(),
                    enabled: false,
                },
            ],
            ..ReplicationConfig::default()
        }
    }

    fn active_active_ref_update(
        name: &str,
        expected: Option<&str>,
        new: Option<&str>,
        force: bool,
    ) -> CoordinatedRefUpdate {
        CoordinatedRefUpdate {
            name: name.to_owned(),
            expected: expected.map(str::to_owned),
            new: new.map(str::to_owned),
            force,
        }
    }

    fn coordinator_repair_snapshot(
        materialization_gaps: Vec<
            crab_coordination::write_coordinator::CoordinatorMaterializationGap,
        >,
    ) -> CoordinatorRepairSnapshot {
        CoordinatorRepairSnapshot {
            coordinator_epoch: 7,
            materialization_gaps,
        }
    }

    fn repair_gap(
        operation_id: &str,
        region: &str,
    ) -> crab_coordination::write_coordinator::CoordinatorMaterializationGap {
        crab_coordination::write_coordinator::CoordinatorMaterializationGap {
            operation_id: operation_id.to_owned(),
            manifest_generation: 42,
            region: region.to_owned(),
            writer: "west".into(),
            source_region: "us-west-2".into(),
            refs: vec![active_active_ref_update(
                "refs/heads/main",
                Some("a"),
                Some("b"),
                false,
            )],
            uploaded_objects: vec!["xorbs/aa/object".into()],
        }
    }

    fn coordinator_status(
        state: CoordinatorCheckState,
        url: &str,
        region: &str,
        failover_regions: &[String],
    ) -> CoordinatorControlPlaneStatus {
        CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            name: url.trim_start_matches("dynamodb://").to_owned(),
            url: url.to_owned(),
            region: region.to_owned(),
            failover_regions: failover_regions.to_vec(),
            backend_available: true,
            checked_drift: true,
            checks: vec![CoordinatorControlPlaneCheck {
                provider: ManagedCoordinatorProvider::DynamoDb,
                code: "coordinator.dynamodb.global-table.unverified".into(),
                state,
                action: "create-global-table".into(),
                target: url.to_owned(),
                managed_resource_id: "crab-coordinator-crab-coordinator-global-table".into(),
                message: "checked by test backend".into(),
                remediation:
                    "repair coordinator resources through crab replica coordinator add --apply"
                        .into(),
            }],
        }
    }

    #[test]
    fn readiness_cache_fingerprint_includes_effective_repo() {
        let replica = ReplicaConfig {
            name: "west".to_owned(),
            provider: ReplicationProviderKind::S3,
            url: "s3://bucket".to_owned(),
            region: "us-west-2".to_owned(),
            backfill: false,
            read: true,
            rpo: ReplicationRpo::Standard,
        };
        assert_ne!(
            readiness_cache_fingerprint(&replica, "org/repo-a"),
            readiness_cache_fingerprint(&replica, "org/repo-b")
        );

        let changed_url = ReplicaConfig {
            url: "s3://other-bucket".to_owned(),
            ..replica.clone()
        };
        assert_ne!(
            readiness_cache_fingerprint(&replica, "org/repo-a"),
            readiness_cache_fingerprint(&changed_url, "org/repo-a")
        );
    }

    #[test]
    fn readiness_cache_requires_fresh_primary_etag() {
        let replica = test_replica();
        let cache = test_readiness_cache(&replica, "org/repo", 7, "etag-a", 1_000);

        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_500,
                ReadinessCheckOptions::default(),
            ),
            Some(500)
        );
        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-b",
                1_500,
                ReadinessCheckOptions::default(),
            ),
            None
        );
    }

    #[test]
    fn readiness_cache_expires_and_can_be_bypassed() {
        let replica = test_replica();
        let cache = test_readiness_cache(&replica, "org/repo", 7, "etag-a", 1_000);

        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_101,
                ReadinessCheckOptions {
                    bypass_cache: false,
                    cache_ttl_ms: 100,
                    max_object_probes: None,
                },
            ),
            None
        );
        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_050,
                ReadinessCheckOptions::deep(),
            ),
            None
        );
    }

    #[test]
    fn readiness_cache_invalidation_marker_blocks_cache_hits() {
        let _cache = isolated_replica_cache();
        let replica = named_test_replica("marker");
        clear_replica_test_cache(&replica);
        let cache = test_readiness_cache(&replica, "org/repo", 7, "etag-a", 1_000);

        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_500,
                ReadinessCheckOptions::default(),
            ),
            Some(500)
        );

        write_readiness_cache_invalidation(&replica, "org/repo", "provider drift", 1_400);

        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_500,
                ReadinessCheckOptions::default(),
            ),
            None
        );

        clear_readiness_cache_invalidation(&replica, "org/repo");

        assert_eq!(
            readiness_cache_age_ms(
                &cache,
                &replica,
                "org/repo",
                7,
                "etag-a",
                1_500,
                ReadinessCheckOptions::default(),
            ),
            Some(500)
        );
        clear_replica_test_cache(&replica);
    }

    #[test]
    fn provider_control_plane_sync_invalidates_and_clears_readiness_cache() {
        let _cache = isolated_replica_cache();
        let replica = named_test_replica("sync-marker");
        clear_replica_test_cache(&replica);
        let replication = ReplicationConfig {
            primary: Some("crab://primary/org/repo".to_owned()),
            replicas: vec![replica.clone()],
            ..Default::default()
        };
        let drifted = control_plane_status_for_cache_test(
            &replica,
            true,
            true,
            ControlPlaneCheckState::Drifted,
        );

        sync_readiness_cache_control_plane("org/repo", &replication, &[drifted]);

        assert!(readiness_cache_is_invalidated(&replica, "org/repo"));

        let verified = control_plane_status_for_cache_test(
            &replica,
            true,
            true,
            ControlPlaneCheckState::Verified,
        );

        sync_readiness_cache_control_plane("org/repo", &replication, &[verified]);

        assert!(!readiness_cache_is_invalidated(&replica, "org/repo"));
        clear_replica_test_cache(&replica);
    }

    #[test]
    fn readiness_env_bool_accepts_operator_values() {
        for value in ["1", "true", "yes", "on"] {
            assert!(parse_env_bool("CRAB_TEST_BOOL", value).unwrap());
        }
        for value in ["0", "false", "no", "off"] {
            assert!(!parse_env_bool("CRAB_TEST_BOOL", value).unwrap());
        }

        let error = parse_env_bool("CRAB_TEST_BOOL", "maybe").unwrap_err();

        assert!(error.to_string().contains("expected boolean value"));
    }

    #[test]
    fn readiness_env_ttl_requires_integer_milliseconds() {
        assert_eq!(parse_env_u64("CRAB_TEST_TTL", "2500").unwrap(), 2500);

        let error = parse_env_u64("CRAB_TEST_TTL", "five-seconds").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("expected non-negative integer milliseconds")
        );
    }

    #[test]
    fn fallback_reason_classifies_operator_reasons() {
        assert_eq!(
            ReplicaFallbackClass::from_reason(Some("replica manifest is stale")),
            ReplicaFallbackClass::StaleManifest
        );
        assert_eq!(
            ReplicaFallbackClass::from_reason(Some("xorb missing at .crab/xorbs/abc")),
            ReplicaFallbackClass::MissingObject
        );
        assert_eq!(
            ReplicaFallbackClass::from_reason(Some("permission denied by replica")),
            ReplicaFallbackClass::Auth
        );
        assert_eq!(
            ReplicaFallbackClass::from_reason(Some("replica client unavailable: no token")),
            ReplicaFallbackClass::ClientUnavailable
        );
        assert_eq!(
            ReplicaFallbackClass::from_reason(Some("replica readiness failed: timeout")),
            ReplicaFallbackClass::ReadinessFailed
        );
        assert_eq!(
            ReplicaFallbackClass::from_reason(None),
            ReplicaFallbackClass::Unknown
        );
    }

    #[test]
    fn replica_event_summary_tracks_read_selection_and_latest_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let replica = test_replica();

        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 5,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "clone".to_owned(),
                outcome: ReplicaReadOutcome::Selected,
                primary_generation: Some(3),
                replica_generation: Some(3),
                reason: None,
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 10,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "fetch".to_owned(),
                outcome: ReplicaReadOutcome::Fallback,
                primary_generation: Some(3),
                replica_generation: Some(2),
                reason: Some("replica manifest is stale".to_owned()),
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 20,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "hydrate".to_owned(),
                outcome: ReplicaReadOutcome::Fallback,
                primary_generation: Some(3),
                replica_generation: Some(3),
                reason: Some("xorb missing".to_owned()),
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 21,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "hydrate".to_owned(),
                outcome: ReplicaReadOutcome::PrimaryFallbackRead,
                primary_generation: None,
                replica_generation: None,
                reason: None,
                primary_fallback_bytes: Some(512),
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 22,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "hydrate".to_owned(),
                outcome: ReplicaReadOutcome::PrimaryFallbackRead,
                primary_generation: None,
                replica_generation: None,
                reason: None,
                primary_fallback_bytes: Some(256),
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 25,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "fetch".to_owned(),
                outcome: ReplicaReadOutcome::Selected,
                primary_generation: Some(4),
                replica_generation: Some(4),
                reason: None,
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 30,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/other".to_owned(),
                operation: "fetch".to_owned(),
                outcome: ReplicaReadOutcome::Fallback,
                primary_generation: Some(4),
                replica_generation: Some(1),
                reason: Some("wrong repo".to_owned()),
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 40,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/other".to_owned(),
                operation: "hydrate".to_owned(),
                outcome: ReplicaReadOutcome::Selected,
                primary_generation: Some(4),
                replica_generation: Some(4),
                reason: None,
                primary_fallback_bytes: None,
            },
        );

        let summary = read_replica_event_summary_from_path(&path, &replica, "org/repo");

        assert_eq!(summary.selected_count, 2);
        assert_eq!(summary.last_selected_at_ms, Some(25));
        assert_eq!(summary.last_selected_operation.as_deref(), Some("fetch"));
        assert_eq!(summary.fallback_count, 2);
        assert_eq!(summary.primary_fallback_bytes, 768);
        assert_eq!(summary.last_fallback_at_ms, Some(20));
        assert_eq!(summary.last_fallback_operation.as_deref(), Some("hydrate"));
        assert_eq!(
            summary.last_fallback_reason.as_deref(),
            Some("xorb missing")
        );
        assert_eq!(
            summary.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
    }

    #[test]
    fn status_with_events_preserves_live_readiness_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let replica = test_replica();
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 42,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "fetch".to_owned(),
                outcome: ReplicaReadOutcome::Fallback,
                primary_generation: Some(3),
                replica_generation: Some(2),
                reason: Some("old fallback".to_owned()),
                primary_fallback_bytes: None,
            },
        );
        append_replica_read_event(
            &path,
            &ReplicaReadEvent {
                version: READ_EVENT_VERSION,
                timestamp_ms: 43,
                replica_name: replica.name.clone(),
                provider: replica.provider,
                url: replica.url.clone(),
                region: replica.region.clone(),
                repo_prefix: "org/repo".to_owned(),
                operation: "hydrate".to_owned(),
                outcome: ReplicaReadOutcome::Selected,
                primary_generation: Some(4),
                replica_generation: Some(4),
                reason: None,
                primary_fallback_bytes: None,
            },
        );
        let summary = read_replica_event_summary_from_path(&path, &replica, "org/repo");
        let status = status_with_event_summary(
            status_with_reason(
                &replica,
                Some(4),
                Some(3),
                "current readiness failure".to_owned(),
            ),
            summary,
        );

        assert_eq!(
            status.last_fallback_reason.as_deref(),
            Some("current readiness failure")
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::ReadinessFailed)
        );
        assert_eq!(status.last_fallback_at_ms, Some(42));
        assert_eq!(status.fallback_count, 1);
        assert_eq!(status.last_selected_at_ms, Some(43));
        assert_eq!(status.last_selected_operation.as_deref(), Some("hydrate"));
        assert_eq!(status.selected_count, 1);
    }

    #[derive(Clone)]
    struct HeadFailingInMemory {
        inner: Arc<InMemory>,
        fail_contains: String,
    }

    impl HeadFailingInMemory {
        fn new(fail_contains: &str) -> Self {
            Self {
                inner: Arc::new(InMemory::new()),
                fail_contains: fail_contains.to_owned(),
            }
        }

        fn injected_error() -> object_store::Error {
            object_store::Error::NotSupported {
                source: boxed_object_store_error("replica readiness probe unavailable"),
            }
        }
    }

    impl fmt::Debug for HeadFailingInMemory {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("HeadFailingInMemory")
                .field("fail_contains", &self.fail_contains)
                .finish()
        }
    }

    impl fmt::Display for HeadFailingInMemory {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("HeadFailingInMemory")
        }
    }

    #[async_trait]
    impl ObjectStore for HeadFailingInMemory {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if options.head && location.as_ref().contains(&self.fail_contains) {
                return Err(Self::injected_error());
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn boxed_object_store_error(
        msg: &'static str,
    ) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    }

    fn test_replica() -> ReplicaConfig {
        ReplicaConfig {
            name: "west".to_owned(),
            provider: ReplicationProviderKind::S3,
            url: "s3://bucket".to_owned(),
            region: "us-west-2".to_owned(),
            backfill: false,
            read: true,
            rpo: ReplicationRpo::Standard,
        }
    }

    fn named_test_replica(name: &str) -> ReplicaConfig {
        ReplicaConfig {
            name: name.to_owned(),
            provider: ReplicationProviderKind::S3,
            url: format!("s3://bucket-{name}"),
            region: "us-west-2".to_owned(),
            backfill: false,
            read: true,
            rpo: ReplicationRpo::Standard,
        }
    }

    fn control_plane_status_for_cache_test(
        replica: &ReplicaConfig,
        backend_available: bool,
        checked_drift: bool,
        state: ControlPlaneCheckState,
    ) -> ControlPlaneStatus {
        ControlPlaneStatus {
            provider: replica.provider,
            replica_name: replica.name.clone(),
            primary: "crab://primary/org/repo".to_owned(),
            replica: replica.url.clone(),
            backend_available,
            checked_drift,
            checks: vec![ControlPlaneCheck {
                provider: replica.provider,
                code: "provider.test.replication".to_owned(),
                state,
                action: "check".to_owned(),
                target: replica.url.clone(),
                managed_resource_id: format!("crab:replica:{}:provider.test", replica.name),
                message: "test provider status".to_owned(),
                remediation: "repair provider drift".to_owned(),
                progress_percent: None,
            }],
        }
    }

    fn clear_replica_test_cache(replica: &ReplicaConfig) {
        let _ = std::fs::remove_dir_all(replica_cache_dir(replica, "org/repo"));
    }

    struct IsolatedReplicaCache {
        _guard: crate::test::git_repo::CacheDirGuard,
        _dir: tempfile::TempDir,
    }

    fn isolated_replica_cache() -> IsolatedReplicaCache {
        let dir = tempfile::tempdir().expect("tempdir for replica cache");
        let guard = crate::test::git_repo::CacheDirGuard::new(dir.path());
        IsolatedReplicaCache {
            _guard: guard,
            _dir: dir,
        }
    }

    fn test_readiness_cache(
        replica: &ReplicaConfig,
        repo_prefix: &str,
        generation: u64,
        primary_etag: &str,
        written_at_ms: u64,
    ) -> ReadinessCache {
        ReadinessCache {
            version: READINESS_CACHE_VERSION,
            replica_name: replica.name.clone(),
            provider: replica.provider,
            url: replica.url.clone(),
            region: replica.region.clone(),
            repo_prefix: repo_prefix.to_owned(),
            generation,
            primary_etag: primary_etag.to_owned(),
            written_at_ms,
        }
    }

    fn memory_store_with_layout(repo_prefix: &str) -> (Store, StoreLayout) {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
        (store, router)
    }

    fn head_failing_memory_store_with_layout(
        repo_prefix: &str,
        fail_contains: &str,
    ) -> (Store, StoreLayout) {
        let store = Store::new(Arc::new(HeadFailingInMemory::new(fail_contains)));
        let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
        (store, router)
    }

    fn test_manifest(generation: u64) -> Manifest {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = generation;
        manifest.seal_git_validation();
        manifest
    }

    async fn write_test_manifest(store: &Store, router: &StoreLayout, manifest: &Manifest) {
        create_manifest(store, router, manifest)
            .await
            .expect("write test manifest");
    }

    async fn write_pack_generation(
        store: &Store,
        router: &StoreLayout,
        generation: u64,
        pack_id: &str,
        include_pack_index: bool,
        include_pack_object: bool,
        include_pack_metadata: bool,
    ) -> Manifest {
        let pack = test_pack_entry(pack_id);
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(generation, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(generation);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(store, router, &manifest).await;
        if include_pack_index {
            segmented::upload_write(store, router, &pack_write)
                .await
                .expect("upload pack index");
        }
        if include_pack_object {
            store
                .put(
                    &router.pack_path(&pack.pack_id),
                    Bytes::from_static(b"pack"),
                )
                .await
                .expect("upload pack object");
        }
        if include_pack_metadata {
            store
                .put(
                    &router.pack_metadata_path(&pack.pack_id),
                    Bytes::from_static(b"meta"),
                )
                .await
                .expect("upload pack metadata");
        }
        manifest
    }

    fn test_pack_entry(pack_id: &str) -> PackManifestEntry {
        let pack_id = canonical_test_pack_id(pack_id);
        PackManifestEntry {
            pack_id: pack_id.clone(),
            size: 42,
            content_hash: pack_id,
            ref_tips: vec!["b".repeat(40)],
            object_count: 1,
        }
    }

    fn canonical_test_pack_id(label: &str) -> String {
        if label.len() == 64 && label.chars().all(|ch| ch.is_ascii_hexdigit()) {
            label.to_owned()
        } else {
            blake3::hash(label.as_bytes()).to_hex().to_string()
        }
    }

    fn test_shard_with_xorb(seed: u64) -> (Bytes, MerkleHash, MerkleHash) {
        let xorb_hash = MerkleHash::from([seed, seed, seed, seed]);
        let chunk_hash = MerkleHash::from([
            seed.wrapping_add(1),
            seed.wrapping_add(1),
            seed.wrapping_add(1),
            seed.wrapping_add(1),
        ]);
        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, 1, 1024),
            chunks: vec![XorbChunkSequenceEntry::new(chunk_hash, 1024, 0)],
        });
        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb).expect("add xorb");
        let (bytes, shard_hash) = writer.finalize().expect("finalize shard");
        (Bytes::from(bytes), shard_hash, xorb_hash)
    }

    fn test_shard_with_xorbs(seed: u64, count: u64) -> (Bytes, MerkleHash, Vec<MerkleHash>) {
        let mut writer = ShardWriter::new();
        let mut xorb_hashes = Vec::new();
        for offset in 0..count {
            let value = seed.wrapping_add(offset);
            let xorb_hash = MerkleHash::from([value, value, value, value]);
            let chunk_value = value.wrapping_add(10_000);
            let chunk_hash = MerkleHash::from([chunk_value, chunk_value, chunk_value, chunk_value]);
            let xorb = Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb_hash, 1, 1024),
                chunks: vec![XorbChunkSequenceEntry::new(chunk_hash, 1024, 0)],
            });
            writer.add_xorb(xorb).expect("add xorb");
            xorb_hashes.push(xorb_hash);
        }
        let (bytes, shard_hash) = writer.finalize().expect("finalize shard");
        (Bytes::from(bytes), shard_hash, xorb_hashes)
    }

    #[tokio::test]
    async fn read_resolver_uses_ready_replica_for_user_read_operations() {
        let _cache = isolated_replica_cache();
        for (generation, operation, slug) in [
            (40, "clone:shard-sync", "clone"),
            (41, "fetch", "fetch"),
            (42, "hydrate", "hydrate"),
            (43, "mount", "mount"),
            (44, "sdk-read", "sdk-read"),
            (45, "smudge", "smudge"),
        ] {
            let (primary_store, primary_router) = memory_store_with_layout("org/repo");
            let (replica_store, replica_router) = memory_store_with_layout("org/repo");
            let replica = named_test_replica(&format!("ready-{slug}"));
            clear_replica_test_cache(&replica);
            let replicas = vec![replica.clone()];
            write_pack_generation(
                &primary_store,
                &primary_router,
                generation,
                "ready",
                true,
                true,
                true,
            )
            .await;
            write_pack_generation(
                &replica_store,
                &replica_router,
                generation,
                "ready",
                true,
                true,
                true,
            )
            .await;
            replica_store
                .put(
                    &replica_router.repo_path("packs/replica-only"),
                    Bytes::from_static(b"replica"),
                )
                .await
                .expect("write replica marker");

            let selection = select_read_store_with_replicas(
                primary_store,
                "org/repo",
                replicas.iter(),
                operation,
                ReadRoutingPolicy::PreferLocal,
                |_, _| Ok((replica_store.clone(), "org/repo".to_owned())),
            )
            .await
            .expect("select read store");

            assert_eq!(
                selection.source,
                ReadSource::Replica {
                    name: replica.name.clone()
                }
            );
            let (marker, _etag) = selection
                .store
                .get_with_etag(&selection.router.repo_path("packs/replica-only"))
                .await
                .expect("read replica marker");
            assert_eq!(marker.as_ref(), b"replica");
        }
    }

    #[tokio::test]
    async fn read_policy_can_force_primary_without_probe() {
        let _cache = isolated_replica_cache();
        for policy in [
            ReadRoutingPolicy::PreferPrimary,
            ReadRoutingPolicy::ReadDisabled,
        ] {
            let (primary_store, primary_router) = memory_store_with_layout("org/repo");
            let replica = named_test_replica("policy-primary");
            clear_replica_test_cache(&replica);
            let replicas = vec![replica];
            primary_store
                .put(
                    &primary_router.repo_path("packs/primary-policy"),
                    Bytes::from_static(b"primary"),
                )
                .await
                .expect("write primary marker");

            let selection = select_read_store_with_replicas(
                primary_store,
                "org/repo",
                replicas.iter(),
                "hydrate",
                policy,
                |_, _| panic!("primary policy must not build replica clients"),
            )
            .await
            .expect("select read store");

            assert_eq!(selection.source, ReadSource::Primary);
            let (marker, _etag) = selection
                .store
                .get_with_etag(&selection.router.repo_path("packs/primary-policy"))
                .await
                .expect("read primary marker");
            assert_eq!(marker.as_ref(), b"primary");
        }
    }

    #[tokio::test]
    async fn read_policy_can_force_named_replica() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (east_store, east_router) = memory_store_with_layout("org/repo");
        let (west_store, west_router) = memory_store_with_layout("org/repo");
        let east = named_test_replica("policy-east");
        let west = named_test_replica("policy-west");
        clear_replica_test_cache(&east);
        clear_replica_test_cache(&west);
        let replicas = vec![east.clone(), west.clone()];
        write_pack_generation(
            &primary_store,
            &primary_router,
            45,
            "policy",
            true,
            true,
            true,
        )
        .await;
        write_pack_generation(&east_store, &east_router, 45, "policy", true, true, true).await;
        write_pack_generation(&west_store, &west_router, 45, "policy", true, true, true).await;
        east_store
            .put(
                &east_router.repo_path("packs/replica-policy"),
                Bytes::from_static(b"east"),
            )
            .await
            .expect("write east marker");
        west_store
            .put(
                &west_router.repo_path("packs/replica-policy"),
                Bytes::from_static(b"west"),
            )
            .await
            .expect("write west marker");

        let selection = select_read_store_with_replicas(
            primary_store,
            "org/repo",
            replicas.iter(),
            "fetch",
            ReadRoutingPolicy::ReplicaName(west.name.clone()),
            |replica, _| {
                if replica.name == east.name {
                    Ok((east_store.clone(), "org/repo".to_owned()))
                } else {
                    Ok((west_store.clone(), "org/repo".to_owned()))
                }
            },
        )
        .await
        .expect("select read store");

        assert_eq!(
            selection.source,
            ReadSource::Replica {
                name: west.name.clone()
            }
        );
        let (marker, _etag) = selection
            .store
            .get_with_etag(&selection.router.repo_path("packs/replica-policy"))
            .await
            .expect("read forced replica marker");
        assert_eq!(marker.as_ref(), b"west");
    }

    #[test]
    fn read_policy_parses_operator_values() {
        assert_eq!(
            ReadRoutingPolicy::parse("prefer-local").unwrap(),
            ReadRoutingPolicy::PreferLocal
        );
        assert_eq!(
            ReadRoutingPolicy::parse("primary").unwrap(),
            ReadRoutingPolicy::PreferPrimary
        );
        assert_eq!(
            ReadRoutingPolicy::parse("disabled").unwrap(),
            ReadRoutingPolicy::ReadDisabled
        );
        assert_eq!(
            ReadRoutingPolicy::parse("replica:west").unwrap(),
            ReadRoutingPolicy::ReplicaName("west".into())
        );
    }

    #[test]
    fn read_policy_env_adapter_preserves_env_key_for_parse_errors() {
        let error = read_routing_policy_from_env_value(Ok("replica:".into())).unwrap_err();

        match error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(key, READ_ROUTING_POLICY_ENV);
                assert!(origin.contains("requires a replica name"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn direct_store_build_calls_are_classified() {
        let mut calls = direct_store_build_calls().expect("scan direct store builders");
        calls.sort();
        let mut expected = direct_store_classifications()
            .iter()
            .map(|classification| classification.call.to_owned())
            .collect::<Vec<_>>();
        expected.sort();

        for classification in direct_store_classifications() {
            assert!(
                !classification.reason.trim().is_empty(),
                "{} must explain why it bypasses StoreResolver read routing",
                classification.call
            );
            assert!(
                matches!(
                    classification.class,
                    "canonical-resolver"
                        | "primary-write-authority"
                        | "primary-maintenance"
                        | "primary-diagnostic"
                        | "domain-specific"
                ),
                "{} has unsupported direct-store classification {}",
                classification.call,
                classification.class
            );
        }

        assert_eq!(
            calls, expected,
            "new direct store construction must be routed through StoreResolver or classified with a primary-only/domain-specific reason"
        );
    }

    #[test]
    fn cli_store_operations_are_classified() {
        let mut operations = cli_store_operations().expect("scan CLI store operations");
        operations.sort();
        operations.dedup();
        let mut expected = cli_store_operation_classifications()
            .iter()
            .map(|classification| classification.operation.to_owned())
            .collect::<Vec<_>>();
        expected.sort();

        for classification in cli_store_operation_classifications() {
            assert!(
                !classification.reason.trim().is_empty(),
                "{} must explain why it bypasses replica read routing",
                classification.operation
            );
            assert!(
                matches!(
                    classification.class,
                    "primary-maintenance" | "primary-diagnostic"
                ),
                "{} has unsupported CLI store classification {}",
                classification.operation,
                classification.class
            );
        }

        assert_eq!(
            operations, expected,
            "new create_cli_store operation must be classified as primary-bound or moved to StoreResolver"
        );
    }

    #[derive(Clone, Copy)]
    struct DirectStoreClassification {
        call: &'static str,
        class: &'static str,
        reason: &'static str,
    }

    #[derive(Clone, Copy)]
    struct CliStoreOperationClassification {
        operation: &'static str,
        class: &'static str,
        reason: &'static str,
    }

    fn direct_store_classifications() -> &'static [DirectStoreClassification] {
        &[
            DirectStoreClassification {
                call: "src/cmd/doctor.rs::check_credentials",
                class: "primary-diagnostic",
                reason: "doctor validates credentials and list access for the configured primary remote",
            },
            DirectStoreClassification {
                call: "src/cmd/doctor.rs::run_cost_report",
                class: "primary-diagnostic",
                reason: "cost reporting inventories the configured primary remote rather than a lagging replica",
            },
            DirectStoreClassification {
                call: "src/cmd/du.rs::fetch_remote_size",
                class: "primary-diagnostic",
                reason: "remote size reporting must inspect the configured primary remote, not a lagging replica",
            },
            DirectStoreClassification {
                call: "src/cmd/lock.rs::setup",
                class: "primary-write-authority",
                reason: "lock operations are write coordination and must target the primary authority",
            },
            DirectStoreClassification {
                call: "src/cmd/metadb.rs::resolve_repo_store_in",
                class: "primary-maintenance",
                reason: "metadb diagnose and rebuild operate on durable metadata state for the configured primary remote",
            },
            DirectStoreClassification {
                call: "src/cmd/release.rs::publish_manifest",
                class: "primary-write-authority",
                reason: "release publication creates repository release manifest objects and must write to the primary authority",
            },
            DirectStoreClassification {
                call: "src/cmd/optimize/xorbs.rs::try_build_store",
                class: "primary-maintenance",
                reason: "xorb optimization reads and rewrites storage layout state and must not use a stale replica",
            },
            DirectStoreClassification {
                call: "src/cmd/workflow.rs::build_remote_store_for",
                class: "primary-write-authority",
                reason: "workflow cache push uploads cache artifacts and must write to the primary remote",
            },
            DirectStoreClassification {
                call: "src/cmd/workflow.rs::build_workflow_artifact_stores",
                class: "primary-write-authority",
                reason: "workflow cache push opens explicitly configured artifact remotes as write targets",
            },
            DirectStoreClassification {
                call: "src/git/protected_push.rs::protected_push_ref_updates",
                class: "primary-write-authority",
                reason: "protected push ref discovery is part of push admission and cannot trust stale replica refs",
            },
            DirectStoreClassification {
                call: "src/storage/resolver.rs::build_cloud_store",
                class: "domain-specific",
                reason: "raw import/export opens user-supplied source and target endpoints outside repository replica read routing",
            },
            DirectStoreClassification {
                call: "src/main.rs::create_cli_store",
                class: "primary-maintenance",
                reason: "bucket-level maintenance commands lack a repo read target and operate on primary storage state",
            },
            DirectStoreClassification {
                call: "src/replication/mod.rs::replica_statuses_with_options",
                class: "canonical-resolver",
                reason: "replica status compares replicas against a primary manifest baseline",
            },
            DirectStoreClassification {
                call: "src/replication/mod.rs::write_store",
                class: "canonical-resolver",
                reason: "write_store is the explicit primary-write resolver boundary",
            },
        ]
    }

    fn cli_store_operation_classifications() -> &'static [CliStoreOperationClassification] {
        &[
            CliStoreOperationClassification {
                operation: "compact",
                class: "primary-maintenance",
                reason: "compaction rewrites remote storage layout state and must target primary storage",
            },
            CliStoreOperationClassification {
                operation: "fsck",
                class: "primary-maintenance",
                reason: "fsck must inspect primary authority state; repair mode may mutate only that authority",
            },
            CliStoreOperationClassification {
                operation: "gc",
                class: "primary-maintenance",
                reason: "garbage collection and registry deregistration delete primary-authority objects",
            },
            CliStoreOperationClassification {
                operation: "repack",
                class: "primary-maintenance",
                reason: "repack rewrites remote pack state and must not derive authority from a replica",
            },
        ]
    }

    fn direct_store_build_calls() -> std::io::Result<Vec<String>> {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src_dir = manifest_dir.join("src");
        let mut files = Vec::new();
        collect_rust_sources(&src_dir, &mut files)?;

        let build_store_pattern = ["auth::", "build_", "store("].concat();
        let mut calls = Vec::new();
        for file in files {
            let body = std::fs::read_to_string(&file)?;
            let rel = file
                .strip_prefix(manifest_dir)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            let mut function = String::from("<module>");
            for line in body.lines() {
                if let Some(name) = rust_function_name(line) {
                    function = name;
                }
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains(&build_store_pattern) {
                    calls.push(format!("{rel}::{function}"));
                }
            }
        }
        Ok(calls)
    }

    fn cli_store_operations() -> std::io::Result<Vec<String>> {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let body = std::fs::read_to_string(manifest_dir.join("src/main.rs"))?;
        let mut operations = Vec::new();
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || !trimmed.contains("create_cli_store(") {
                continue;
            }
            if let Some(operation) = quoted_argument(trimmed) {
                operations.push(operation);
            }
        }
        Ok(operations)
    }

    fn quoted_argument(line: &str) -> Option<String> {
        let first = line.find('"')?;
        let rest = &line[first + 1..];
        let second = rest.find('"')?;
        Some(rest[..second].to_owned())
    }

    fn collect_rust_sources(
        dir: &std::path::Path,
        files: &mut Vec<std::path::PathBuf>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                collect_rust_sources(&path, files)?;
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
        Ok(())
    }

    fn rust_function_name(line: &str) -> Option<String> {
        let index = line.find("fn ")?;
        let rest = &line[index + 3..];
        let name = rest
            .split(|ch: char| ch == '(' || ch == '<' || ch.is_whitespace())
            .next()
            .filter(|name| !name.is_empty())?;
        Some(name.to_owned())
    }

    #[tokio::test]
    async fn read_resolver_falls_back_when_replica_manifest_precedes_pack_objects() {
        let _cache = isolated_replica_cache();
        for (generation, operation, slug) in [
            (50, "clone:shard-sync", "clone"),
            (51, "fetch", "fetch"),
            (52, "hydrate", "hydrate"),
            (53, "mount", "mount"),
            (54, "sdk-read", "sdk-read"),
            (55, "smudge", "smudge"),
        ] {
            let (primary_store, primary_router) = memory_store_with_layout("org/repo");
            let (replica_store, replica_router) = memory_store_with_layout("org/repo");
            let replica = named_test_replica(&format!("delayed-{slug}"));
            clear_replica_test_cache(&replica);
            let replicas = vec![replica.clone()];
            write_pack_generation(
                &primary_store,
                &primary_router,
                generation,
                "delayed",
                true,
                true,
                true,
            )
            .await;
            write_pack_generation(
                &replica_store,
                &replica_router,
                generation,
                "delayed",
                true,
                false,
                false,
            )
            .await;
            primary_store
                .put(
                    &primary_router.repo_path("packs/primary-only"),
                    Bytes::from_static(b"primary"),
                )
                .await
                .expect("write primary marker");

            let selection = select_read_store_with_replicas(
                primary_store,
                "org/repo",
                replicas.iter(),
                operation,
                ReadRoutingPolicy::PreferLocal,
                |_, _| Ok((replica_store.clone(), "org/repo".to_owned())),
            )
            .await
            .expect("select read store");

            assert_eq!(selection.source, ReadSource::Primary);
            let (marker, _etag) = selection
                .store
                .get_with_etag(&selection.router.repo_path("packs/primary-only"))
                .await
                .expect("read primary marker");
            assert_eq!(marker.as_ref(), b"primary");
            let summary = read_replica_event_summary(&replica, "org/repo");
            assert_eq!(summary.fallback_count, 1);
            assert_eq!(summary.primary_fallback_bytes, 7);
            assert_eq!(summary.last_fallback_operation.as_deref(), Some(operation));
            assert_eq!(
                summary.last_fallback_class,
                Some(ReplicaFallbackClass::MissingObject)
            );
        }
    }

    #[tokio::test]
    async fn read_resolver_falls_back_when_replica_client_auth_fails() {
        let _cache = isolated_replica_cache();
        for (operation, slug) in [
            ("clone:shard-sync", "clone"),
            ("fetch", "fetch"),
            ("hydrate", "hydrate"),
            ("mount", "mount"),
            ("sdk-read", "sdk-read"),
            ("smudge", "smudge"),
        ] {
            let (primary_store, primary_router) = memory_store_with_layout("org/repo");
            let replica = named_test_replica(&format!("auth-failed-{slug}"));
            clear_replica_test_cache(&replica);
            let replicas = vec![replica.clone()];
            primary_store
                .put(
                    &primary_router.repo_path("packs/primary-auth-fallback"),
                    Bytes::from_static(b"primary"),
                )
                .await
                .expect("write primary marker");

            let selection = select_read_store_with_replicas(
                primary_store,
                "org/repo",
                replicas.iter(),
                operation,
                ReadRoutingPolicy::PreferLocal,
                |replica, _| {
                    Err(CrabError::AuthFailed {
                        path: replica.url.clone(),
                    })
                },
            )
            .await
            .expect("select read store");

            assert_eq!(selection.source, ReadSource::Primary);
            let (marker, _etag) = selection
                .store
                .get_with_etag(&selection.router.repo_path("packs/primary-auth-fallback"))
                .await
                .expect("read primary marker");
            assert_eq!(marker.as_ref(), b"primary");
            let summary = read_replica_event_summary(&replica, "org/repo");
            assert_eq!(summary.fallback_count, 1);
            assert_eq!(summary.primary_fallback_bytes, 7);
            assert_eq!(summary.last_fallback_operation.as_deref(), Some(operation));
            assert_eq!(
                summary.last_fallback_class,
                Some(ReplicaFallbackClass::Auth)
            );
        }
    }

    #[tokio::test]
    async fn read_resolver_falls_back_when_replica_readiness_probe_fails() {
        let _cache = isolated_replica_cache();
        for (generation, operation, slug) in [
            (60, "clone:shard-sync", "clone"),
            (61, "fetch", "fetch"),
            (62, "hydrate", "hydrate"),
            (63, "mount", "mount"),
            (64, "sdk-read", "sdk-read"),
            (65, "smudge", "smudge"),
        ] {
            let (primary_store, primary_router) = memory_store_with_layout("org/repo");
            let flaky_pack_id = canonical_test_pack_id("flaky");
            let fail_contains = format!("pack-{flaky_pack_id}.pack");
            let (replica_store, replica_router) =
                head_failing_memory_store_with_layout("org/repo", &fail_contains);
            let replica = named_test_replica(&format!("probe-failed-{slug}"));
            clear_replica_test_cache(&replica);
            let replicas = vec![replica.clone()];
            write_pack_generation(
                &primary_store,
                &primary_router,
                generation,
                &flaky_pack_id,
                true,
                true,
                true,
            )
            .await;
            write_pack_generation(
                &replica_store,
                &replica_router,
                generation,
                &flaky_pack_id,
                true,
                true,
                true,
            )
            .await;
            primary_store
                .put(
                    &primary_router.repo_path("packs/primary-probe-fallback"),
                    Bytes::from_static(b"primary"),
                )
                .await
                .expect("write primary marker");

            let selection = select_read_store_with_replicas(
                primary_store,
                "org/repo",
                replicas.iter(),
                operation,
                ReadRoutingPolicy::PreferLocal,
                |_, _| Ok((replica_store.clone(), "org/repo".to_owned())),
            )
            .await
            .expect("select read store");

            assert_eq!(selection.source, ReadSource::Primary);
            let (marker, _etag) = selection
                .store
                .get_with_etag(&selection.router.repo_path("packs/primary-probe-fallback"))
                .await
                .expect("read primary marker");
            assert_eq!(marker.as_ref(), b"primary");
            let summary = read_replica_event_summary(&replica, "org/repo");
            assert_eq!(summary.fallback_count, 1);
            assert_eq!(summary.primary_fallback_bytes, 7);
            assert_eq!(summary.last_fallback_operation.as_deref(), Some(operation));
            assert_eq!(
                summary.last_fallback_class,
                Some(ReplicaFallbackClass::ReadinessFailed)
            );
        }
    }

    #[test]
    fn gcs_fast_plan_sets_turbo_rpo() {
        let plan = setup_plan(
            ReplicationProviderKind::Gcs,
            "crab://primary/repo",
            "gs://replica/repo",
            "us",
            ReplicationRpo::Fast,
            false,
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.description.contains("ASYNC_TURBO"))
        );
    }

    #[test]
    fn azure_plan_requires_change_feed_and_versioning() {
        let plan = setup_plan(
            ReplicationProviderKind::Azure,
            "crab://primary/repo",
            "azure://replica/repo",
            "westus2",
            ReplicationRpo::Standard,
            false,
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.description.contains("change feed"))
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.description.contains("versioning"))
        );
    }

    #[tokio::test]
    async fn readiness_accepts_replica_after_manifest_and_referenced_pack_objects_arrive() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let pack = test_pack_entry("pack-ready");
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(7, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(7);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");
        replica_store
            .put(
                &replica_router.pack_path(&pack.pack_id),
                Bytes::from_static(b"pack"),
            )
            .await
            .expect("upload pack object");
        replica_store
            .put(
                &replica_router.pack_metadata_path(&pack.pack_id),
                Bytes::from_static(b"meta"),
            )
            .await
            .expect("upload pack metadata");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(status.ready);
        assert_eq!(status.lag_generations, Some(0));
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, 2);
    }

    #[tokio::test]
    async fn readiness_cache_hit_skips_repeated_probes_but_deep_revalidates() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let replica = named_test_replica("cache-hit-revalidates");
        clear_replica_test_cache(&replica);
        let cache_pack_id = canonical_test_pack_id("cache");
        write_pack_generation(
            &primary_store,
            &primary_router,
            70,
            "cache",
            true,
            true,
            true,
        )
        .await;
        write_pack_generation(
            &replica_store,
            &replica_router,
            70,
            "cache",
            true,
            true,
            true,
        )
        .await;

        let first = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("first readiness check");

        assert!(first.ready);
        assert!(!first.readiness_cache_hit);
        assert_eq!(first.readiness_object_read_count, 1);
        assert_eq!(first.readiness_object_probe_count, 2);

        replica_store
            .delete(&replica_router.pack_path(&cache_pack_id))
            .await
            .expect("remove referenced pack after cache write");

        let cached = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("cached readiness check");

        assert!(cached.ready);
        assert!(cached.readiness_cache_hit);
        assert_eq!(cached.readiness_object_read_count, 0);
        assert_eq!(cached.readiness_object_probe_count, 0);

        let deep = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("deep readiness check");

        assert!(!deep.ready);
        assert!(!deep.readiness_cache_hit);
        assert_eq!(
            deep.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(deep.readiness_object_read_count, 1);
        assert_eq!(deep.readiness_object_probe_count, 1);
    }

    #[tokio::test]
    async fn readiness_cache_misses_after_primary_manifest_generation_advances() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let replica = named_test_replica("cache-primary-advanced");
        clear_replica_test_cache(&replica);
        write_pack_generation(
            &primary_store,
            &primary_router,
            80,
            "advanced",
            true,
            true,
            true,
        )
        .await;
        write_pack_generation(
            &replica_store,
            &replica_router,
            80,
            "advanced",
            true,
            true,
            true,
        )
        .await;
        let cached = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("write readiness cache");
        assert!(cached.ready);

        let (mut primary_manifest, primary_etag) = read_manifest(&primary_store, &primary_router)
            .await
            .expect("read primary manifest");
        primary_manifest.generation = 81;
        primary_manifest.seal_git_validation();
        write_manifest_cas(
            &primary_store,
            &primary_router,
            &primary_manifest,
            &primary_etag,
        )
        .await
        .expect("advance primary manifest");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("readiness after primary generation advance");

        assert!(!status.ready);
        assert!(!status.readiness_cache_hit);
        assert_eq!(status.replica_generation, Some(80));
        assert_eq!(status.lag_generations, Some(1));
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::StaleManifest)
        );
    }

    #[tokio::test]
    async fn readiness_rejects_manifest_before_pack_index_arrives() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let mut manifest = test_manifest(8);
        manifest.pack_index_hash = "c".repeat(64);
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert_eq!(
            status.last_fallback_reason.as_deref(),
            Some("pack index missing")
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, 0);
    }

    #[tokio::test]
    async fn readiness_rejects_manifest_before_referenced_pack_arrives() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let pack = test_pack_entry("pack-delayed");
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(9, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(9);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pack missing"))
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, 1);
    }

    #[tokio::test]
    async fn readiness_rejects_manifest_before_referenced_pack_metadata_arrives() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let pack = test_pack_entry("pack-metadata-delayed");
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(10, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(10);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");
        replica_store
            .put(
                &replica_router.pack_path(&pack.pack_id),
                Bytes::from_static(b"pack"),
            )
            .await
            .expect("upload pack object");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pack metadata missing"))
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, 2);
    }

    #[tokio::test]
    async fn readiness_rejects_stale_replica_manifest_generation() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        write_test_manifest(&primary_store, &primary_router, &test_manifest(9)).await;
        write_test_manifest(&replica_store, &replica_router, &test_manifest(8)).await;

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert_eq!(status.lag_generations, Some(1));
        assert_eq!(
            status.last_fallback_reason.as_deref(),
            Some("replica manifest is stale")
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::StaleManifest)
        );
    }

    #[tokio::test]
    async fn readiness_rejects_manifest_before_referenced_shard_arrives() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let shard_hash = MerkleHash::from([1u64, 2, 3, 4]).hex();
        let (shard_index_hash, _index, write) =
            compact_shard_index(10, &[shard_hash]).expect("build shard index");
        let mut manifest = test_manifest(10);
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &write)
            .await
            .expect("upload shard index");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("shard missing"))
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(status.readiness_object_read_count, 2);
        assert_eq!(status.readiness_object_probe_count, 0);
    }

    #[tokio::test]
    async fn readiness_rejects_manifest_before_referenced_xorb_arrives() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let (shard_bytes, shard_hash, xorb_hash) = test_shard_with_xorb(12);
        let (shard_index_hash, _index, write) =
            compact_shard_index(11, &[shard_hash.hex()]).expect("build shard index");
        let mut manifest = test_manifest(11);
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &write)
            .await
            .expect("upload shard index");
        replica_store
            .put(&replica_router.shard_path(&shard_hash), shard_bytes)
            .await
            .expect("upload shard");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!status.ready);
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("xorb missing"))
        );
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains(&xorb_hash.hex()))
        );
        assert_eq!(
            status.last_fallback_class,
            Some(ReplicaFallbackClass::MissingObject)
        );
        assert_eq!(status.readiness_object_read_count, 2);
        assert_eq!(status.readiness_object_probe_count, 1);
    }

    #[tokio::test]
    async fn readiness_reports_missing_referenced_shard() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let shard_hash = MerkleHash::from([1u64, 2, 3, 4]).hex();
        let (shard_index_hash, _index, write) =
            compact_shard_index(1, &[shard_hash]).expect("build shard index");
        segmented::upload_write(&replica_store, &replica_router, &write)
            .await
            .expect("upload shard index");

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");
        assert!(!status.ready);
        assert!(
            status
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("shard missing"))
        );
        assert_eq!(status.readiness_object_read_count, 2);
        assert_eq!(status.readiness_object_probe_count, 0);
    }

    #[tokio::test]
    async fn sampled_readiness_stops_after_object_probe_limit() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let packs = vec![test_pack_entry("sample-a"), test_pack_entry("sample-b")];
        let sampled_pack_id = packs[0].pack_id.clone();
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(91, &packs).expect("build pack index");
        let mut manifest = test_manifest(91);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");
        replica_store
            .put(
                &replica_router.pack_path(&sampled_pack_id),
                Bytes::from_static(b"pack"),
            )
            .await
            .expect("upload sampled pack");

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::sampled(1),
        )
        .await
        .expect("sampled readiness check");

        assert!(status.ready);
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, 1);

        replica_store
            .delete(&replica_router.pack_path(&sampled_pack_id))
            .await
            .expect("remove sampled pack after sampled proof");

        let uncached = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &test_replica(),
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("uncached readiness check");

        assert!(!uncached.ready);
        assert!(!uncached.readiness_cache_hit);
    }

    #[tokio::test]
    async fn readiness_large_pack_inventory_probe_count_is_linear() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let replica = named_test_replica("large-pack-inventory");
        clear_replica_test_cache(&replica);
        let packs = (0..64)
            .map(|index| test_pack_entry(&format!("large-{index}")))
            .collect::<Vec<_>>();
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(101, &packs).expect("build large pack index");
        let mut manifest = test_manifest(101);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload large pack index");
        for pack in &packs {
            replica_store
                .put(
                    &replica_router.pack_path(&pack.pack_id),
                    Bytes::from_static(b"pack"),
                )
                .await
                .expect("upload pack object");
            replica_store
                .put(
                    &replica_router.pack_metadata_path(&pack.pack_id),
                    Bytes::from_static(b"meta"),
                )
                .await
                .expect("upload pack metadata");
        }

        let status = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("large pack readiness check");

        assert!(status.ready);
        assert_eq!(status.readiness_object_read_count, 1);
        assert_eq!(status.readiness_object_probe_count, packs.len() as u64 * 2);
        clear_replica_test_cache(&replica);
    }

    #[tokio::test]
    async fn sampled_readiness_caps_large_xorb_inventory_without_cache() {
        let _cache = isolated_replica_cache();
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let replica = named_test_replica("large-xorb-sampled");
        clear_replica_test_cache(&replica);
        let (shard_bytes, shard_hash, xorb_hashes) = test_shard_with_xorbs(200, 96);
        let (shard_index_hash, _index, write) =
            compact_shard_index(102, &[shard_hash.hex()]).expect("build large shard index");
        let mut manifest = test_manifest(102);
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented::upload_write(&replica_store, &replica_router, &write)
            .await
            .expect("upload large shard index");
        replica_store
            .put(&replica_router.shard_path(&shard_hash), shard_bytes)
            .await
            .expect("upload large shard");
        for xorb_hash in xorb_hashes.iter().take(8) {
            replica_store
                .put(
                    &replica_router.xorb_path(xorb_hash),
                    Bytes::from_static(b"xorb"),
                )
                .await
                .expect("upload sampled xorb");
        }

        let sampled = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::sampled(8),
        )
        .await
        .expect("sampled large xorb readiness check");

        assert!(sampled.ready);
        assert!(!sampled.readiness_cache_hit);
        assert_eq!(sampled.readiness_object_read_count, 2);
        assert_eq!(sampled.readiness_object_probe_count, 8);

        let exhaustive = replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            &replica,
            ReadinessCheckOptions::default(),
        )
        .await
        .expect("exhaustive large xorb readiness check");

        assert!(!exhaustive.ready);
        assert!(!exhaustive.readiness_cache_hit);
        assert_eq!(exhaustive.readiness_object_read_count, 2);
        assert_eq!(exhaustive.readiness_object_probe_count, 9);
        assert!(
            exhaustive
                .last_fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("xorb missing"))
        );
        clear_replica_test_cache(&replica);
    }
}
