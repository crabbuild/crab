//! Active-active write coordination contracts.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::error::{CoordinationError, Result};

const DEFAULT_COMPLETED_OPERATION_RECORDS: usize = 1024;

/// State of an active-active push transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushTransactionState {
    Pending,
    ObjectsUploaded,
    Committed,
    Materialized,
    Aborted,
}

/// Ref update requested by an active-active push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatedRefUpdate {
    pub name: String,
    pub expected: Option<String>,
    pub new: Option<String>,
    #[serde(default)]
    pub force: bool,
}

/// Active-active commit request sent to the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitRequest {
    pub operation_id: String,
    pub writer: String,
    pub region: String,
    pub manifest_generation: u64,
    pub refs: Vec<CoordinatedRefUpdate>,
    #[serde(default)]
    pub uploaded_objects: Vec<String>,
    #[serde(default)]
    pub target_regions: Vec<String>,
}

/// Successful coordinator commit result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitOutcome {
    pub operation_id: String,
    pub coordinator_epoch: u64,
    pub writer: String,
    pub region: String,
    pub manifest_generation: u64,
    pub state: PushTransactionState,
}

/// Coordinator health snapshot used by active-active write admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorHealth {
    pub healthy: bool,
    pub epoch: u64,
    pub linearizable: bool,
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_summary: Option<CoordinatorStateSummary>,
}

/// Size and retention pressure for a managed coordinator's repo authority state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorStateSummary {
    pub transaction_count: usize,
    pub completed_operation_count: usize,
    pub max_completed_operations: usize,
    pub state_bytes: usize,
    pub max_state_bytes: Option<usize>,
}

/// Result of fencing or resuming coordinator write admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorFenceOutcome {
    pub previous_epoch: u64,
    pub coordinator_epoch: u64,
    pub previous_healthy: bool,
    pub healthy: bool,
    pub changed: bool,
    pub reason: Option<String>,
}

/// Object that GC must protect because a coordinator transaction still owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorProtectedObject {
    pub key: String,
    pub operation_id: String,
    pub state: PushTransactionState,
    pub manifest_generation: u64,
    pub writer: String,
    pub region: String,
}

/// Coordinator snapshot used by GC and repair before deleting remote objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorGcSafetySnapshot {
    pub coordinator_epoch: u64,
    pub protected_objects: Vec<CoordinatorProtectedObject>,
}

impl CoordinatorGcSafetySnapshot {
    #[must_use]
    pub fn protected_keys(&self) -> std::collections::HashSet<String> {
        self.protected_objects
            .iter()
            .map(|object| object.key.clone())
            .collect()
    }
}

/// Regional manifest materialization that should be repaired from coordinator truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorMaterializationGap {
    pub operation_id: String,
    pub manifest_generation: u64,
    pub region: String,
    pub writer: String,
    pub source_region: String,
    pub refs: Vec<CoordinatedRefUpdate>,
    pub uploaded_objects: Vec<String>,
}

/// Coordinator snapshot used by repair workers to rematerialize regional manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoordinatorRepairSnapshot {
    pub coordinator_epoch: u64,
    pub materialization_gaps: Vec<CoordinatorMaterializationGap>,
}

/// One versioned repository authority record read from a managed coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorStateRecord {
    pub version: u64,
    pub state: CoordinatorRepoState,
}

/// Serialized repository authority state for CAS-backed coordinators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorRepoState {
    pub epoch: u64,
    pub healthy: bool,
    #[serde(default)]
    pub fence_reason: Option<String>,
    pub refs: BTreeMap<String, String>,
    pub transactions: BTreeMap<String, CoordinatorTransactionRecord>,
    #[serde(default)]
    pub completed_operations: BTreeMap<String, CoordinatorCompletedOperationRecord>,
    #[serde(default)]
    pub next_completed_sequence: u64,
}

impl Default for CoordinatorRepoState {
    fn default() -> Self {
        Self {
            epoch: 1,
            healthy: true,
            fence_reason: None,
            refs: BTreeMap::new(),
            transactions: BTreeMap::new(),
            completed_operations: BTreeMap::new(),
            next_completed_sequence: 1,
        }
    }
}

/// Serialized active-active push transaction in a coordinator state record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorTransactionRecord {
    pub request: CommitRequest,
    pub outcome: Option<CommitOutcome>,
    pub state: PushTransactionState,
    pub materialized_regions: BTreeSet<String>,
    #[serde(default)]
    pub coordinator_epoch: u64,
}

/// Compact replay record for a terminal coordinator operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorCompletedOperationRecord {
    pub request_fingerprint: String,
    pub outcome: Option<CommitOutcome>,
    pub state: PushTransactionState,
    pub target_regions: BTreeSet<String>,
    pub coordinator_epoch: u64,
    pub sequence: u64,
}

/// Managed active-active coordinator backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ManagedCoordinatorProvider {
    DynamoDb,
    Spanner,
    CosmosDb,
}

impl ManagedCoordinatorProvider {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DynamoDb => "dynamodb",
            Self::Spanner => "spanner",
            Self::CosmosDb => "cosmosdb",
        }
    }
}

/// One management API operation needed to create a coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CoordinatorControlPlaneRequest {
    pub provider: ManagedCoordinatorProvider,
    pub action: String,
    pub target: String,
    pub request: serde_json::Value,
    pub reversible: bool,
    pub managed_resource_id: String,
}

/// Management plan for a managed coordinator backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CoordinatorControlPlanePlan {
    pub provider: ManagedCoordinatorProvider,
    pub name: String,
    pub url: String,
    pub region: String,
    pub failover_regions: Vec<String>,
    pub requests: Vec<CoordinatorControlPlaneRequest>,
}

/// Cloud control-plane apply result for coordinator management.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CoordinatorApplyStatus {
    pub provider: ManagedCoordinatorProvider,
    pub applied: bool,
    pub checked_drift: bool,
    pub actions: Vec<String>,
    pub message: String,
}

/// State of one managed coordinator control-plane check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinatorCheckState {
    Verified,
    Missing,
    Drifted,
    Unknown,
    Unsupported,
}

impl CoordinatorCheckState {
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

/// One managed coordinator resource check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CoordinatorControlPlaneCheck {
    pub provider: ManagedCoordinatorProvider,
    pub code: String,
    pub state: CoordinatorCheckState,
    pub action: String,
    pub target: String,
    pub managed_resource_id: String,
    pub message: String,
    pub remediation: String,
}

/// Control-plane status for a managed active-active coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CoordinatorControlPlaneStatus {
    pub provider: ManagedCoordinatorProvider,
    pub name: String,
    pub url: String,
    pub region: String,
    pub failover_regions: Vec<String>,
    pub backend_available: bool,
    pub checked_drift: bool,
    pub checks: Vec<CoordinatorControlPlaneCheck>,
}

/// Management backend for a linearizable active-active coordinator.
#[async_trait]
pub trait CoordinatorControlPlaneBackend: Send + Sync {
    fn provider(&self) -> ManagedCoordinatorProvider;
    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus>;
    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus>;
    async fn remove(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus>;
}

/// Apply managed coordinator operations.
pub fn apply_coordinator_control_plane_plan(
    plan: &CoordinatorControlPlanePlan,
) -> Result<CoordinatorApplyStatus> {
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "{} coordinator apply backend is not wired; {} Crab-owned operation(s) were planned and no cloud resources were changed",
            plan.provider.as_str(),
            plan.requests.len()
        ),
    })
}

/// Apply managed coordinator operations through a live backend.
pub async fn apply_coordinator_control_plane_plan_with_backend(
    plan: &CoordinatorControlPlanePlan,
    backend: &dyn CoordinatorControlPlaneBackend,
) -> Result<CoordinatorApplyStatus> {
    validate_coordinator_backend(plan, backend)?;
    let status = backend.status(plan).await?;
    validate_coordinator_status_matches_plan(plan, &status)?;
    validate_coordinator_apply_status(&status)?;
    backend.apply(plan).await
}

/// Inspect managed coordinator resources through a live backend.
pub async fn inspect_coordinator_control_plane_plan_with_backend(
    plan: &CoordinatorControlPlanePlan,
    backend: &dyn CoordinatorControlPlaneBackend,
) -> Result<CoordinatorControlPlaneStatus> {
    validate_coordinator_backend(plan, backend)?;
    let status = backend.status(plan).await?;
    validate_coordinator_status_matches_plan(plan, &status)?;
    Ok(status)
}

/// Remove managed coordinator resources after proving Crab ownership.
pub fn remove_coordinator_control_plane_plan(
    plan: &CoordinatorControlPlanePlan,
) -> Result<CoordinatorApplyStatus> {
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "{} coordinator remove backend is not wired; {} Crab-owned operation(s) were planned and no cloud resources were changed",
            plan.provider.as_str(),
            plan.requests.len()
        ),
    })
}

/// Remove managed coordinator resources after proving Crab ownership.
pub async fn remove_coordinator_control_plane_plan_with_backend(
    plan: &CoordinatorControlPlanePlan,
    backend: &dyn CoordinatorControlPlaneBackend,
) -> Result<CoordinatorApplyStatus> {
    validate_coordinator_backend(plan, backend)?;
    let status = backend.status(plan).await?;
    validate_coordinator_status_matches_plan(plan, &status)?;
    validate_coordinator_remove_status(&status)?;
    backend.remove(plan).await
}

/// Validates that coordinator control-plane status can admit active-active writes.
pub fn validate_coordinator_write_admission(status: &CoordinatorControlPlaneStatus) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} coordinator {} cannot admit writes until control-plane drift is verified",
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
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} is {}; active-active writes fail closed until every coordinator resource is verified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

pub(crate) fn coordination_error(error: CoordinationError) -> CoordinationError {
    error
}

fn validate_coordinator_backend(
    plan: &CoordinatorControlPlanePlan,
    backend: &dyn CoordinatorControlPlaneBackend,
) -> Result<()> {
    if backend.provider() == plan.provider {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "coordinator backend {} cannot manage {} coordinator plan",
            backend.provider().as_str(),
            plan.provider.as_str()
        ),
    })
}

fn validate_coordinator_status_matches_plan(
    plan: &CoordinatorControlPlanePlan,
    status: &CoordinatorControlPlaneStatus,
) -> Result<()> {
    let planned_failover_regions = plan
        .failover_regions
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let status_failover_regions = status
        .failover_regions
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if status.provider != plan.provider
        || status.name != plan.name
        || status.url != plan.url
        || status.region != plan.region
        || status_failover_regions != planned_failover_regions
    {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "coordinator control-plane status for {} does not match planned coordinator {}",
                status.name, plan.name
            ),
        });
    }
    Ok(())
}

fn validate_coordinator_remove_status(status: &CoordinatorControlPlaneStatus) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} coordinator {} cannot be removed until control-plane drift is verified",
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
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} is {}; refusing to remove coordinator resources that are missing, drifted, unsupported, or unverified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

fn validate_coordinator_apply_status(status: &CoordinatorControlPlaneStatus) -> Result<()> {
    if !status.backend_available || !status.checked_drift {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} coordinator {} cannot be applied until control-plane drift is verified",
                status.provider.as_str(),
                status.name
            ),
        });
    }
    if let Some(check) = status
        .checks
        .iter()
        .find(|check| !coordinator_check_allows_apply(check))
    {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".into(),
            origin: format!(
                "{} is {}; refusing to mutate coordinator resources that are missing safety proof, drifted, unsupported, or unverified",
                check.managed_resource_id,
                check.state.as_str()
            ),
        });
    }
    Ok(())
}

fn coordinator_check_allows_apply(check: &CoordinatorControlPlaneCheck) -> bool {
    match check.state {
        CoordinatorCheckState::Verified => true,
        CoordinatorCheckState::Missing => {
            !coordinator_base_action(&check.action).starts_with("validate-")
        }
        CoordinatorCheckState::Drifted
        | CoordinatorCheckState::Unknown
        | CoordinatorCheckState::Unsupported => false,
    }
}

fn coordinator_base_action(action: &str) -> &str {
    action.strip_prefix("remove:").unwrap_or(action)
}

/// Versioned CAS storage contract for managed coordinator data planes.
#[async_trait]
pub trait VersionedCoordinatorStateStore: Send + Sync {
    async fn read_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
    ) -> Result<Option<CoordinatorStateRecord>>;

    async fn compare_and_swap_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &CoordinatorRepoState,
    ) -> Result<bool>;
}

#[async_trait]
impl<T> VersionedCoordinatorStateStore for &T
where
    T: VersionedCoordinatorStateStore + ?Sized,
{
    async fn read_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
    ) -> Result<Option<CoordinatorStateRecord>> {
        (*self).read_repo_state(namespace, repo_key).await
    }

    async fn compare_and_swap_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &CoordinatorRepoState,
    ) -> Result<bool> {
        (*self)
            .compare_and_swap_repo_state(namespace, repo_key, expected_version, next_state)
            .await
    }
}

/// Linearizable authority for active-active repository writes.
#[async_trait]
pub trait WriteCoordinator: Send + Sync {
    async fn health(&self) -> Result<CoordinatorHealth>;
    async fn begin(&self, request: CommitRequest) -> Result<PushTransactionState>;
    async fn mark_objects_uploaded(&self, operation_id: &str) -> Result<PushTransactionState>;
    async fn commit(&self, request: CommitRequest) -> Result<CommitOutcome>;
    async fn mark_materialized(&self, operation_id: &str) -> Result<PushTransactionState>;
    async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> Result<PushTransactionState>;
    async fn abort(&self, operation_id: &str) -> Result<PushTransactionState>;
    async fn ref_value(&self, name: &str) -> Result<Option<String>>;
    async fn gc_safety_snapshot(&self) -> Result<CoordinatorGcSafetySnapshot>;
    async fn repair_snapshot(&self) -> Result<CoordinatorRepairSnapshot>;
    async fn fence_writes(&self, reason: Option<String>) -> Result<CoordinatorFenceOutcome>;
    async fn resume_writes(&self) -> Result<CoordinatorFenceOutcome>;
}

/// Commits an active-active push after immutable objects are uploaded.
///
/// The coordinator remains the only mutable authority. This helper gives the
/// push pipeline one monotonic transaction path to call after object upload:
/// begin, mark objects uploaded, and commit refs. Failed commits are aborted
/// when possible. Callers must mark regional materialization only after the
/// manifest projection is durably written in that region.
pub async fn commit_uploaded_push_refs(
    coordinator: &dyn WriteCoordinator,
    request: CommitRequest,
) -> Result<CommitOutcome> {
    let health = coordinator.health().await?;
    if !health.healthy {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator".to_owned(),
            origin: health.reason.unwrap_or_else(|| {
                "coordinator health check failed; active-active writes fail closed".to_owned()
            }),
        });
    }
    if !health.linearizable {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.consistency".to_owned(),
            origin: "coordinator does not report linearizable consistency; active-active writes fail closed".to_owned(),
        });
    }

    let operation_id = request.operation_id.clone();
    match coordinator.begin(request.clone()).await? {
        PushTransactionState::Pending => {
            coordinator.mark_objects_uploaded(&operation_id).await?;
        }
        PushTransactionState::ObjectsUploaded
        | PushTransactionState::Committed
        | PushTransactionState::Materialized => {}
        PushTransactionState::Aborted => {
            return Err(CoordinationError::CasConflict {
                path: format!("coordinator/transactions/{operation_id}"),
                expected_etag: None,
            });
        }
    }

    match coordinator.commit(request).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let _ = coordinator.abort(&operation_id).await;
            Err(error)
        }
    }
}

/// Commits an active-active push and marks the writer region materialized.
///
/// Prefer [`commit_uploaded_push_refs`] when a caller must durably write a
/// regional manifest projection before marking materialization complete.
pub async fn commit_uploaded_push(
    coordinator: &dyn WriteCoordinator,
    request: CommitRequest,
) -> Result<CommitOutcome> {
    let materialized_region = request.region.clone();
    let mut outcome = commit_uploaded_push_refs(coordinator, request).await?;
    if outcome.state != PushTransactionState::Materialized {
        outcome.state = coordinator
            .mark_region_materialized(&outcome.operation_id, &materialized_region)
            .await?;
    }
    Ok(outcome)
}

/// Generic CAS-backed implementation of the active-active write coordinator.
pub struct VersionedStateWriteCoordinator<C> {
    provider: &'static str,
    namespace: String,
    repo_key: String,
    client: C,
    max_cas_attempts: usize,
    max_state_bytes: usize,
    max_completed_operations: usize,
}

impl<C> VersionedStateWriteCoordinator<C> {
    #[must_use]
    pub fn new(
        provider: &'static str,
        namespace: impl Into<String>,
        repo_key: impl Into<String>,
        client: C,
    ) -> Self {
        Self {
            provider,
            namespace: namespace.into(),
            repo_key: repo_key.into(),
            client,
            max_cas_attempts: 16,
            max_state_bytes: 1_000_000,
            max_completed_operations: 1024,
        }
    }

    #[must_use]
    pub fn with_max_cas_attempts(mut self, max_cas_attempts: usize) -> Self {
        self.max_cas_attempts = max_cas_attempts.max(1);
        self
    }

    #[must_use]
    pub fn with_max_state_bytes(mut self, max_state_bytes: usize) -> Self {
        self.max_state_bytes = max_state_bytes.max(1);
        self
    }

    #[must_use]
    pub fn with_max_completed_operations(mut self, max_completed_operations: usize) -> Self {
        self.max_completed_operations = max_completed_operations.max(1);
        self
    }
}

impl<C> VersionedStateWriteCoordinator<C>
where
    C: VersionedCoordinatorStateStore,
{
    pub async fn health(&self) -> Result<CoordinatorHealth> {
        let state = self.load_state().await?;
        let state_summary = Some(versioned_state_summary(
            self.provider,
            &state,
            self.max_completed_operations,
            Some(self.max_state_bytes),
        )?);
        Ok(CoordinatorHealth {
            healthy: state.healthy,
            epoch: state.epoch,
            linearizable: true,
            reason: if state.healthy {
                None
            } else {
                Some(state.fence_reason.clone().unwrap_or_else(|| {
                    format!("{} coordinator is fenced unhealthy", self.provider)
                }))
            },
            state_summary,
        })
    }

    pub async fn begin(&self, request: CommitRequest) -> Result<PushTransactionState> {
        self.mutate_state(|state| {
            ensure_versioned_state_healthy(self.provider, state)?;
            if let Some(record) = state.transactions.get(&request.operation_id) {
                ensure_versioned_transaction_request(record, &request)?;
                ensure_versioned_transaction_epoch_current(state.epoch, record)?;
                return Ok(record.state);
            }
            if let Some(record) = state.completed_operations.get(&request.operation_id) {
                return coordinator_completed_operation_state(record, &request);
            }
            let coordinator_epoch = state.epoch;
            state.transactions.insert(
                request.operation_id.clone(),
                CoordinatorTransactionRecord {
                    request: request.clone(),
                    outcome: None,
                    state: PushTransactionState::Pending,
                    materialized_regions: BTreeSet::new(),
                    coordinator_epoch,
                },
            );
            Ok(PushTransactionState::Pending)
        })
        .await
    }

    pub async fn mark_objects_uploaded(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.transition(operation_id, PushTransactionState::ObjectsUploaded)
            .await
    }

    pub async fn commit(&self, request: CommitRequest) -> Result<CommitOutcome> {
        self.mutate_state(|state| {
            ensure_versioned_state_healthy(self.provider, state)?;
            let (materialized_regions, transaction_epoch) =
                if let Some(record) = state.transactions.get(&request.operation_id) {
                    if let Some(outcome) = record.outcome.as_ref() {
                        return Ok(outcome.clone());
                    }
                    if record.state == PushTransactionState::Aborted {
                        return Err(CoordinationError::CasConflict {
                            path: format!("coordinator/transactions/{}", request.operation_id),
                            expected_etag: None,
                        });
                    }
                    ensure_versioned_transaction_request(record, &request)?;
                    ensure_versioned_transaction_epoch_current(state.epoch, record)?;
                    (
                        record.materialized_regions.clone(),
                        record.coordinator_epoch,
                    )
                } else if let Some(record) = state.completed_operations.get(&request.operation_id) {
                    return coordinator_completed_operation_outcome(record, &request);
                } else {
                    (BTreeSet::new(), state.epoch)
                };

            for update in &request.refs {
                if update.force {
                    continue;
                }
                let current = state.refs.get(&update.name).cloned();
                if current != update.expected {
                    return Err(CoordinationError::NonFastForward {
                        ref_name: update.name.clone(),
                        have: current.unwrap_or_default(),
                        want: update.expected.clone().unwrap_or_default(),
                    });
                }
            }

            for update in &request.refs {
                match update.new.as_ref() {
                    Some(new) => {
                        state.refs.insert(update.name.clone(), new.clone());
                    }
                    None => {
                        state.refs.remove(&update.name);
                    }
                }
            }

            let outcome = CommitOutcome {
                operation_id: request.operation_id.clone(),
                coordinator_epoch: transaction_epoch,
                writer: request.writer.clone(),
                region: request.region.clone(),
                manifest_generation: request.manifest_generation,
                state: PushTransactionState::Committed,
            };
            state.transactions.insert(
                request.operation_id.clone(),
                CoordinatorTransactionRecord {
                    request: request.clone(),
                    outcome: Some(outcome.clone()),
                    state: PushTransactionState::Committed,
                    materialized_regions,
                    coordinator_epoch: transaction_epoch,
                },
            );
            Ok(outcome)
        })
        .await
    }

    pub async fn mark_materialized(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.mutate_state(|state| {
            ensure_versioned_state_healthy(self.provider, state)?;
            if let Some(record) = state.completed_operations.get(operation_id) {
                return coordinator_completed_materialized_state(record, operation_id);
            }
            let next = {
                let record = versioned_transaction_record_mut(state, operation_id)?;
                ensure_versioned_materializable(record)?;
                record.materialized_regions = coordinator_effective_target_regions(&record.request);
                update_versioned_materialization_state(record);
                record.state
            };
            compact_versioned_terminal_transaction(
                self.provider,
                state,
                operation_id,
                self.max_completed_operations,
            )?;
            Ok(next)
        })
        .await
    }

    pub async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> Result<PushTransactionState> {
        self.mutate_state(|state| {
            ensure_versioned_state_healthy(self.provider, state)?;
            if let Some(record) = state.completed_operations.get(operation_id) {
                return coordinator_completed_region_materialized_state(
                    record,
                    operation_id,
                    region,
                );
            }
            let next = {
                let record = versioned_transaction_record_mut(state, operation_id)?;
                ensure_versioned_materializable(record)?;
                let target_regions = coordinator_effective_target_regions(&record.request);
                if !target_regions.contains(region) {
                    return Err(CoordinationError::Configuration {
                        key: "replication.coordinator.materialization".to_owned(),
                        origin: format!("region {region} is not a materialization target"),
                    });
                }
                record.materialized_regions.insert(region.to_owned());
                update_versioned_materialization_state(record);
                record.state
            };
            compact_versioned_terminal_transaction(
                self.provider,
                state,
                operation_id,
                self.max_completed_operations,
            )?;
            Ok(next)
        })
        .await
    }

    pub async fn abort(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.transition(operation_id, PushTransactionState::Aborted)
            .await
    }

    pub async fn ref_value(&self, name: &str) -> Result<Option<String>> {
        let state = self.load_state().await?;
        Ok(state.refs.get(name).cloned())
    }

    pub async fn gc_safety_snapshot(&self) -> Result<CoordinatorGcSafetySnapshot> {
        let state = self.load_state().await?;
        ensure_versioned_state_healthy(self.provider, &state)?;
        let mut protected_objects = Vec::new();
        for (operation_id, record) in &state.transactions {
            if !versioned_transaction_state_protects_uploaded_objects(record.state) {
                continue;
            }
            for key in &record.request.uploaded_objects {
                protected_objects.push(CoordinatorProtectedObject {
                    key: key.clone(),
                    operation_id: operation_id.clone(),
                    state: record.state,
                    manifest_generation: record.request.manifest_generation,
                    writer: record.request.writer.clone(),
                    region: record.request.region.clone(),
                });
            }
        }
        protected_objects.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.operation_id.cmp(&right.operation_id))
        });
        Ok(CoordinatorGcSafetySnapshot {
            coordinator_epoch: state.epoch,
            protected_objects,
        })
    }

    pub async fn repair_snapshot(&self) -> Result<CoordinatorRepairSnapshot> {
        let state = self.load_state().await?;
        ensure_versioned_state_healthy(self.provider, &state)?;
        let mut materialization_gaps = Vec::new();
        for (operation_id, record) in &state.transactions {
            if record.outcome.is_none() || record.state == PushTransactionState::Aborted {
                continue;
            }
            for region in coordinator_effective_target_regions(&record.request) {
                if record.materialized_regions.contains(&region) {
                    continue;
                }
                materialization_gaps.push(CoordinatorMaterializationGap {
                    operation_id: operation_id.clone(),
                    manifest_generation: record.request.manifest_generation,
                    region,
                    writer: record.request.writer.clone(),
                    source_region: record.request.region.clone(),
                    refs: record.request.refs.clone(),
                    uploaded_objects: record.request.uploaded_objects.clone(),
                });
            }
        }
        materialization_gaps.sort_by(|left, right| {
            left.operation_id
                .cmp(&right.operation_id)
                .then_with(|| left.region.cmp(&right.region))
        });
        Ok(CoordinatorRepairSnapshot {
            coordinator_epoch: state.epoch,
            materialization_gaps,
        })
    }

    pub async fn fence_writes(&self, reason: Option<String>) -> Result<CoordinatorFenceOutcome> {
        self.mutate_state(|state| {
            let previous_epoch = state.epoch;
            let previous_healthy = state.healthy;
            let previous_reason = state.fence_reason.clone();
            let next_reason = reason.clone();

            if state.healthy {
                state.epoch = state.epoch.saturating_add(1);
                state.healthy = false;
            }
            if next_reason.is_some() {
                state.fence_reason = next_reason;
            }
            let changed = previous_epoch != state.epoch
                || previous_healthy != state.healthy
                || previous_reason != state.fence_reason;

            Ok(CoordinatorFenceOutcome {
                previous_epoch,
                coordinator_epoch: state.epoch,
                previous_healthy,
                healthy: state.healthy,
                changed,
                reason: state.fence_reason.clone(),
            })
        })
        .await
    }

    pub async fn resume_writes(&self) -> Result<CoordinatorFenceOutcome> {
        self.mutate_state(|state| {
            let previous_epoch = state.epoch;
            let previous_healthy = state.healthy;
            let previous_reason = state.fence_reason.clone();

            state.healthy = true;
            state.fence_reason = None;
            let changed = previous_healthy != state.healthy || previous_reason.is_some();

            Ok(CoordinatorFenceOutcome {
                previous_epoch,
                coordinator_epoch: state.epoch,
                previous_healthy,
                healthy: state.healthy,
                changed,
                reason: None,
            })
        })
        .await
    }

    async fn load_state(&self) -> Result<CoordinatorRepoState> {
        Ok(self
            .client
            .read_repo_state(&self.namespace, &self.repo_key)
            .await?
            .map_or_else(CoordinatorRepoState::default, |record| record.state))
    }

    async fn transition(
        &self,
        operation_id: &str,
        next: PushTransactionState,
    ) -> Result<PushTransactionState> {
        self.mutate_state(|state| {
            ensure_versioned_state_healthy(self.provider, state)?;
            let current_epoch = state.epoch;
            let record = versioned_transaction_record_mut(state, operation_id)?;
            if next != PushTransactionState::Aborted {
                ensure_versioned_transaction_epoch_current(current_epoch, record)?;
            }
            if !is_valid_versioned_transaction_transition(record.state, next) {
                return Err(CoordinationError::CasConflict {
                    path: format!("coordinator/transactions/{operation_id}"),
                    expected_etag: None,
                });
            }
            record.state = next;
            if let Some(outcome) = record.outcome.as_mut() {
                outcome.state = next;
            }
            let next = record.state;
            compact_versioned_terminal_transaction(
                self.provider,
                state,
                operation_id,
                self.max_completed_operations,
            )?;
            Ok(next)
        })
        .await
    }

    async fn mutate_state<T>(
        &self,
        mut mutation: impl FnMut(&mut CoordinatorRepoState) -> Result<T>,
    ) -> Result<T> {
        for _ in 0..self.max_cas_attempts {
            let record = self
                .client
                .read_repo_state(&self.namespace, &self.repo_key)
                .await?;
            let expected_version = record.as_ref().map(|record| record.version);
            let mut state =
                record.map_or_else(CoordinatorRepoState::default, |record| record.state);
            let result = mutation(&mut state)?;
            ensure_versioned_state_size(self.provider, self.max_state_bytes, &state)?;
            if self
                .client
                .compare_and_swap_repo_state(
                    &self.namespace,
                    &self.repo_key,
                    expected_version,
                    &state,
                )
                .await?
            {
                return Ok(result);
            }
        }
        Err(CoordinationError::CasConflict {
            path: format!("coordinator/{}/{}", self.provider, self.repo_key),
            expected_etag: None,
        })
    }
}

#[async_trait]
impl<C> WriteCoordinator for VersionedStateWriteCoordinator<C>
where
    C: VersionedCoordinatorStateStore,
{
    async fn health(&self) -> Result<CoordinatorHealth> {
        self.health().await
    }

    async fn begin(&self, request: CommitRequest) -> Result<PushTransactionState> {
        self.begin(request).await
    }

    async fn mark_objects_uploaded(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.mark_objects_uploaded(operation_id).await
    }

    async fn commit(&self, request: CommitRequest) -> Result<CommitOutcome> {
        self.commit(request).await
    }

    async fn mark_materialized(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.mark_materialized(operation_id).await
    }

    async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> Result<PushTransactionState> {
        self.mark_region_materialized(operation_id, region).await
    }

    async fn abort(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.abort(operation_id).await
    }

    async fn ref_value(&self, name: &str) -> Result<Option<String>> {
        self.ref_value(name).await
    }

    async fn gc_safety_snapshot(&self) -> Result<CoordinatorGcSafetySnapshot> {
        self.gc_safety_snapshot().await
    }

    async fn repair_snapshot(&self) -> Result<CoordinatorRepairSnapshot> {
        self.repair_snapshot().await
    }

    async fn fence_writes(&self, reason: Option<String>) -> Result<CoordinatorFenceOutcome> {
        self.fence_writes(reason).await
    }

    async fn resume_writes(&self) -> Result<CoordinatorFenceOutcome> {
        self.resume_writes().await
    }
}

/// In-memory implementation of the active-active write coordinator.
pub struct InMemoryWriteCoordinator {
    state: Mutex<CoordinatorRepoState>,
    max_completed_operations: usize,
}

impl Default for InMemoryWriteCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryWriteCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CoordinatorRepoState::default()),
            max_completed_operations: DEFAULT_COMPLETED_OPERATION_RECORDS,
        }
    }

    #[must_use]
    pub fn with_max_completed_operations(mut self, max_completed_operations: usize) -> Self {
        self.max_completed_operations = max_completed_operations.max(1);
        self
    }

    pub async fn seed_ref(&self, name: &str, value: &str) {
        let mut state = self.state.lock().await;
        state.refs.insert(name.to_owned(), value.to_owned());
    }

    pub async fn fence_epoch(&self) -> u64 {
        let mut state = self.state.lock().await;
        state.epoch = state.epoch.saturating_add(1);
        state.epoch
    }

    pub async fn set_healthy(&self, healthy: bool) {
        let mut state = self.state.lock().await;
        state.healthy = healthy;
    }

    pub async fn health(&self) -> Result<CoordinatorHealth> {
        let state = self.state.lock().await;
        let state_summary = Some(CoordinatorStateSummary {
            transaction_count: state.transactions.len(),
            completed_operation_count: state.completed_operations.len(),
            max_completed_operations: self.max_completed_operations,
            state_bytes: 0,
            max_state_bytes: None,
        });
        Ok(CoordinatorHealth {
            healthy: state.healthy,
            epoch: state.epoch,
            linearizable: true,
            reason: if state.healthy {
                None
            } else {
                Some(
                    state
                        .fence_reason
                        .clone()
                        .unwrap_or_else(|| "coordinator is fenced unhealthy".to_owned()),
                )
            },
            state_summary,
        })
    }

    pub async fn begin(&self, request: CommitRequest) -> Result<PushTransactionState> {
        let mut state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        if let Some(record) = state.transactions.get(&request.operation_id) {
            ensure_versioned_transaction_request(record, &request)?;
            ensure_versioned_transaction_epoch_current(state.epoch, record)?;
            return Ok(record.state);
        }
        if let Some(record) = state.completed_operations.get(&request.operation_id) {
            return coordinator_completed_operation_state(record, &request);
        }
        let coordinator_epoch = state.epoch;
        state.transactions.insert(
            request.operation_id.clone(),
            CoordinatorTransactionRecord {
                request,
                outcome: None,
                state: PushTransactionState::Pending,
                materialized_regions: BTreeSet::new(),
                coordinator_epoch,
            },
        );
        Ok(PushTransactionState::Pending)
    }

    pub async fn mark_objects_uploaded(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.transition(operation_id, PushTransactionState::ObjectsUploaded)
            .await
    }

    pub async fn commit(&self, request: CommitRequest) -> Result<CommitOutcome> {
        let mut state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;

        let (materialized_regions, transaction_epoch) =
            if let Some(record) = state.transactions.get(&request.operation_id) {
                if let Some(outcome) = record.outcome.as_ref() {
                    return Ok(outcome.clone());
                }
                if record.state == PushTransactionState::Aborted {
                    return Err(CoordinationError::CasConflict {
                        path: format!("coordinator/transactions/{}", request.operation_id),
                        expected_etag: None,
                    });
                }
                ensure_versioned_transaction_request(record, &request)?;
                ensure_versioned_transaction_epoch_current(state.epoch, record)?;
                (
                    record.materialized_regions.clone(),
                    record.coordinator_epoch,
                )
            } else if let Some(record) = state.completed_operations.get(&request.operation_id) {
                return coordinator_completed_operation_outcome(record, &request);
            } else {
                (BTreeSet::new(), state.epoch)
            };

        for update in &request.refs {
            if update.force {
                continue;
            }
            let current = state.refs.get(&update.name).cloned();
            if current != update.expected {
                return Err(CoordinationError::NonFastForward {
                    ref_name: update.name.clone(),
                    have: current.unwrap_or_default(),
                    want: update.expected.clone().unwrap_or_default(),
                });
            }
        }

        for update in &request.refs {
            match update.new.as_ref() {
                Some(new) => {
                    state.refs.insert(update.name.clone(), new.clone());
                }
                None => {
                    state.refs.remove(&update.name);
                }
            }
        }

        let outcome = CommitOutcome {
            operation_id: request.operation_id.clone(),
            coordinator_epoch: transaction_epoch,
            writer: request.writer.clone(),
            region: request.region.clone(),
            manifest_generation: request.manifest_generation,
            state: PushTransactionState::Committed,
        };
        state.transactions.insert(
            request.operation_id.clone(),
            CoordinatorTransactionRecord {
                request,
                outcome: Some(outcome.clone()),
                state: PushTransactionState::Committed,
                materialized_regions,
                coordinator_epoch: transaction_epoch,
            },
        );
        Ok(outcome)
    }

    pub async fn mark_materialized(&self, operation_id: &str) -> Result<PushTransactionState> {
        let mut state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        if let Some(record) = state.completed_operations.get(operation_id) {
            return coordinator_completed_materialized_state(record, operation_id);
        }
        let next = {
            let record = versioned_transaction_record_mut(&mut state, operation_id)?;
            ensure_versioned_materializable(record)?;
            record.materialized_regions = coordinator_effective_target_regions(&record.request);
            update_versioned_materialization_state(record);
            record.state
        };
        compact_versioned_terminal_transaction(
            "in-memory",
            &mut state,
            operation_id,
            self.max_completed_operations,
        )?;
        Ok(next)
    }

    pub async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> Result<PushTransactionState> {
        let mut state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        if let Some(record) = state.completed_operations.get(operation_id) {
            return coordinator_completed_region_materialized_state(record, operation_id, region);
        }
        let next = {
            let record = versioned_transaction_record_mut(&mut state, operation_id)?;
            ensure_versioned_materializable(record)?;
            let target_regions = coordinator_effective_target_regions(&record.request);
            if !target_regions.contains(region) {
                return Err(CoordinationError::Configuration {
                    key: "replication.coordinator.materialization".to_owned(),
                    origin: format!("region {region} is not a materialization target"),
                });
            }
            record.materialized_regions.insert(region.to_owned());
            update_versioned_materialization_state(record);
            record.state
        };
        compact_versioned_terminal_transaction(
            "in-memory",
            &mut state,
            operation_id,
            self.max_completed_operations,
        )?;
        Ok(next)
    }

    pub async fn abort(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.transition(operation_id, PushTransactionState::Aborted)
            .await
    }

    pub async fn ref_value(&self, name: &str) -> Result<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.refs.get(name).cloned())
    }

    pub async fn gc_safety_snapshot(&self) -> Result<CoordinatorGcSafetySnapshot> {
        let state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        let mut protected_objects = Vec::new();
        for (operation_id, record) in &state.transactions {
            if !versioned_transaction_state_protects_uploaded_objects(record.state) {
                continue;
            }
            for key in &record.request.uploaded_objects {
                protected_objects.push(CoordinatorProtectedObject {
                    key: key.clone(),
                    operation_id: operation_id.clone(),
                    state: record.state,
                    manifest_generation: record.request.manifest_generation,
                    writer: record.request.writer.clone(),
                    region: record.request.region.clone(),
                });
            }
        }
        protected_objects.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.operation_id.cmp(&right.operation_id))
        });
        Ok(CoordinatorGcSafetySnapshot {
            coordinator_epoch: state.epoch,
            protected_objects,
        })
    }

    pub async fn repair_snapshot(&self) -> Result<CoordinatorRepairSnapshot> {
        let state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        let mut materialization_gaps = Vec::new();
        for (operation_id, record) in &state.transactions {
            if record.outcome.is_none() || record.state == PushTransactionState::Aborted {
                continue;
            }
            for region in coordinator_effective_target_regions(&record.request) {
                if record.materialized_regions.contains(&region) {
                    continue;
                }
                materialization_gaps.push(CoordinatorMaterializationGap {
                    operation_id: operation_id.clone(),
                    manifest_generation: record.request.manifest_generation,
                    region,
                    writer: record.request.writer.clone(),
                    source_region: record.request.region.clone(),
                    refs: record.request.refs.clone(),
                    uploaded_objects: record.request.uploaded_objects.clone(),
                });
            }
        }
        materialization_gaps.sort_by(|left, right| {
            left.operation_id
                .cmp(&right.operation_id)
                .then_with(|| left.region.cmp(&right.region))
        });
        Ok(CoordinatorRepairSnapshot {
            coordinator_epoch: state.epoch,
            materialization_gaps,
        })
    }

    pub async fn fence_writes(&self, reason: Option<String>) -> Result<CoordinatorFenceOutcome> {
        let mut state = self.state.lock().await;
        let previous_epoch = state.epoch;
        let previous_healthy = state.healthy;
        let previous_reason = state.fence_reason.clone();

        if state.healthy {
            state.epoch = state.epoch.saturating_add(1);
            state.healthy = false;
        }
        if reason.is_some() {
            state.fence_reason = reason;
        }
        let changed = previous_epoch != state.epoch
            || previous_healthy != state.healthy
            || previous_reason != state.fence_reason;

        Ok(CoordinatorFenceOutcome {
            previous_epoch,
            coordinator_epoch: state.epoch,
            previous_healthy,
            healthy: state.healthy,
            changed,
            reason: state.fence_reason.clone(),
        })
    }

    pub async fn resume_writes(&self) -> Result<CoordinatorFenceOutcome> {
        let mut state = self.state.lock().await;
        let previous_epoch = state.epoch;
        let previous_healthy = state.healthy;
        let previous_reason = state.fence_reason.clone();

        state.healthy = true;
        state.fence_reason = None;
        let changed = previous_healthy != state.healthy || previous_reason.is_some();

        Ok(CoordinatorFenceOutcome {
            previous_epoch,
            coordinator_epoch: state.epoch,
            previous_healthy,
            healthy: state.healthy,
            changed,
            reason: None,
        })
    }

    async fn transition(
        &self,
        operation_id: &str,
        next: PushTransactionState,
    ) -> Result<PushTransactionState> {
        let mut state = self.state.lock().await;
        ensure_versioned_state_healthy("in-memory", &state)?;
        let current_epoch = state.epoch;
        let record = versioned_transaction_record_mut(&mut state, operation_id)?;
        if next != PushTransactionState::Aborted {
            ensure_versioned_transaction_epoch_current(current_epoch, record)?;
        }
        if !is_valid_versioned_transaction_transition(record.state, next) {
            return Err(CoordinationError::CasConflict {
                path: format!("coordinator/transactions/{operation_id}"),
                expected_etag: None,
            });
        }
        record.state = next;
        if let Some(outcome) = record.outcome.as_mut() {
            outcome.state = next;
        }
        let next = record.state;
        compact_versioned_terminal_transaction(
            "in-memory",
            &mut state,
            operation_id,
            self.max_completed_operations,
        )?;
        Ok(next)
    }
}

#[async_trait]
impl WriteCoordinator for InMemoryWriteCoordinator {
    async fn health(&self) -> Result<CoordinatorHealth> {
        self.health().await
    }

    async fn begin(&self, request: CommitRequest) -> Result<PushTransactionState> {
        self.begin(request).await
    }

    async fn mark_objects_uploaded(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.mark_objects_uploaded(operation_id).await
    }

    async fn commit(&self, request: CommitRequest) -> Result<CommitOutcome> {
        self.commit(request).await
    }

    async fn mark_materialized(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.mark_materialized(operation_id).await
    }

    async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> Result<PushTransactionState> {
        self.mark_region_materialized(operation_id, region).await
    }

    async fn abort(&self, operation_id: &str) -> Result<PushTransactionState> {
        self.abort(operation_id).await
    }

    async fn ref_value(&self, name: &str) -> Result<Option<String>> {
        self.ref_value(name).await
    }

    async fn gc_safety_snapshot(&self) -> Result<CoordinatorGcSafetySnapshot> {
        self.gc_safety_snapshot().await
    }

    async fn repair_snapshot(&self) -> Result<CoordinatorRepairSnapshot> {
        self.repair_snapshot().await
    }

    async fn fence_writes(&self, reason: Option<String>) -> Result<CoordinatorFenceOutcome> {
        self.fence_writes(reason).await
    }

    async fn resume_writes(&self) -> Result<CoordinatorFenceOutcome> {
        self.resume_writes().await
    }
}

fn ensure_versioned_state_healthy(provider: &str, state: &CoordinatorRepoState) -> Result<()> {
    if state.healthy {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".to_owned(),
        origin: format!(
            "{provider} coordinator health check failed; active-active writes fail closed"
        ),
    })
}

fn versioned_transaction_record_mut<'a>(
    state: &'a mut CoordinatorRepoState,
    operation_id: &str,
) -> Result<&'a mut CoordinatorTransactionRecord> {
    state
        .transactions
        .get_mut(operation_id)
        .ok_or_else(|| CoordinationError::NotFound {
            path: format!("coordinator/transactions/{operation_id}"),
        })
}

fn ensure_versioned_transaction_request(
    record: &CoordinatorTransactionRecord,
    request: &CommitRequest,
) -> Result<()> {
    if record.request == *request {
        return Ok(());
    }
    Err(CoordinationError::CasConflict {
        path: format!("coordinator/transactions/{}", request.operation_id),
        expected_etag: None,
    })
}

fn ensure_versioned_transaction_epoch_current(
    current_epoch: u64,
    record: &CoordinatorTransactionRecord,
) -> Result<()> {
    if record.outcome.is_some()
        || record.state == PushTransactionState::Aborted
        || record.coordinator_epoch == current_epoch
    {
        return Ok(());
    }
    Err(versioned_stale_transaction_epoch_error(
        &record.request.operation_id,
        record.coordinator_epoch,
        current_epoch,
    ))
}

fn versioned_stale_transaction_epoch_error(
    operation_id: &str,
    transaction_epoch: u64,
    current_epoch: u64,
) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.epoch".to_owned(),
        origin: format!(
            "transaction {operation_id} began in coordinator epoch {transaction_epoch}; current epoch is {current_epoch}; retry the push"
        ),
    }
}

fn ensure_versioned_materializable(record: &CoordinatorTransactionRecord) -> Result<()> {
    if matches!(
        record.state,
        PushTransactionState::Committed | PushTransactionState::Materialized
    ) {
        return Ok(());
    }
    Err(CoordinationError::CasConflict {
        path: format!("coordinator/transactions/{}", record.request.operation_id),
        expected_etag: None,
    })
}

fn update_versioned_materialization_state(record: &mut CoordinatorTransactionRecord) {
    let target_regions = coordinator_effective_target_regions(&record.request);
    record.state = if target_regions.is_subset(&record.materialized_regions) {
        PushTransactionState::Materialized
    } else {
        PushTransactionState::Committed
    };
    if let Some(outcome) = record.outcome.as_mut() {
        outcome.state = record.state;
    }
}

fn compact_versioned_terminal_transaction(
    provider: &str,
    state: &mut CoordinatorRepoState,
    operation_id: &str,
    max_completed_operations: usize,
) -> Result<()> {
    let Some(record) = state.transactions.get(operation_id) else {
        return Ok(());
    };
    if !matches!(
        record.state,
        PushTransactionState::Materialized | PushTransactionState::Aborted
    ) {
        return Ok(());
    }

    let sequence = next_completed_sequence(&mut state.next_completed_sequence);
    let completed = coordinator_completed_operation_record(
        &record.request,
        record.outcome.clone(),
        record.state,
        record.coordinator_epoch,
        sequence,
    )?;
    state
        .completed_operations
        .insert(operation_id.to_owned(), completed);
    state.transactions.remove(operation_id);
    prune_versioned_completed_operations(state, max_completed_operations);
    ensure_versioned_completed_operations_valid(provider, state)
}

fn prune_versioned_completed_operations(
    state: &mut CoordinatorRepoState,
    max_completed_operations: usize,
) {
    while state.completed_operations.len() > max_completed_operations {
        let Some(oldest) = state
            .completed_operations
            .iter()
            .min_by(|left, right| {
                left.1
                    .sequence
                    .cmp(&right.1.sequence)
                    .then_with(|| left.0.cmp(right.0))
            })
            .map(|(operation_id, _)| operation_id.clone())
        else {
            break;
        };
        state.completed_operations.remove(&oldest);
    }
}

fn ensure_versioned_completed_operations_valid(
    provider: &str,
    state: &CoordinatorRepoState,
) -> Result<()> {
    for (operation_id, record) in &state.completed_operations {
        if matches!(
            record.state,
            PushTransactionState::Materialized | PushTransactionState::Aborted
        ) {
            continue;
        }
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.completed_operations".to_owned(),
            origin: format!(
                "{provider} coordinator compacted operation {operation_id} has non-terminal state {:?}",
                record.state
            ),
        });
    }
    Ok(())
}

fn is_valid_versioned_transaction_transition(
    current: PushTransactionState,
    next: PushTransactionState,
) -> bool {
    if current == next {
        return true;
    }
    matches!(
        (current, next),
        (
            PushTransactionState::Pending,
            PushTransactionState::ObjectsUploaded | PushTransactionState::Aborted
        ) | (
            PushTransactionState::ObjectsUploaded,
            PushTransactionState::Committed | PushTransactionState::Aborted
        ) | (
            PushTransactionState::Committed,
            PushTransactionState::Materialized
        )
    )
}

fn versioned_transaction_state_protects_uploaded_objects(state: PushTransactionState) -> bool {
    matches!(
        state,
        PushTransactionState::Pending
            | PushTransactionState::ObjectsUploaded
            | PushTransactionState::Committed
    )
}

fn ensure_versioned_state_size(
    provider: &str,
    max_state_bytes: usize,
    state: &CoordinatorRepoState,
) -> Result<()> {
    let state_bytes = versioned_state_size_bytes(provider, state)?;
    if state_bytes <= max_state_bytes {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator.state_size".to_owned(),
        origin: format!(
            "{provider} coordinator repo state is {} bytes; Crab's configured state limit is {} bytes",
            state_bytes, max_state_bytes
        ),
    })
}

fn versioned_state_summary(
    provider: &str,
    state: &CoordinatorRepoState,
    max_completed_operations: usize,
    max_state_bytes: Option<usize>,
) -> Result<CoordinatorStateSummary> {
    Ok(CoordinatorStateSummary {
        transaction_count: state.transactions.len(),
        completed_operation_count: state.completed_operations.len(),
        max_completed_operations,
        state_bytes: versioned_state_size_bytes(provider, state)?,
        max_state_bytes,
    })
}

fn versioned_state_size_bytes(provider: &str, state: &CoordinatorRepoState) -> Result<usize> {
    serde_json::to_vec(state)
        .map(|bytes| bytes.len())
        .map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.state".to_owned(),
            origin: format!("failed to serialize {provider} coordinator state: {err}"),
        })
}

fn next_completed_sequence(next_completed_sequence: &mut u64) -> u64 {
    let sequence = (*next_completed_sequence).max(1);
    *next_completed_sequence = sequence.saturating_add(1);
    sequence
}

#[must_use]
pub fn dynamodb_coordinator_plan(
    name: &str,
    region: &str,
    failover_regions: &[String],
) -> CoordinatorControlPlanePlan {
    let mut regions = vec![region.to_owned()];
    for failover_region in failover_regions {
        if !regions.contains(failover_region) {
            regions.push(failover_region.clone());
        }
    }
    let witness_regions = dynamodb_default_witness_regions(&regions);
    let deployment_topology = if witness_regions.is_empty() && regions.len() == 3 {
        "three-full-replicas"
    } else if !witness_regions.is_empty() {
        "two-replicas-one-witness"
    } else {
        "invalid-mrsc-topology"
    };
    let tags = json!({
        "crab:managed": "true",
        "crab:resource": "write-coordinator",
        "crab:coordinator": name,
    });
    let target = format!("dynamodb://{name}");
    let requests = vec![
        coordinator_request(
            ManagedCoordinatorProvider::DynamoDb,
            "create-global-table",
            &target,
            format!("crab-coordinator-{name}-global-table"),
            true,
            json!({
                "table_name": name,
                "regions": regions,
                "witness_regions": witness_regions,
                "billing_mode": "PAY_PER_REQUEST",
                "keys": [
                    {"name": "pk", "type": "S"},
                    {"name": "sk", "type": "S"}
                ],
                "consistency_mode": "MRSC",
                "account_model": "same-account",
                "deployment_topology": deployment_topology,
                "ownership": tags,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::DynamoDb,
            "validate-linearizable-contract",
            &target,
            format!("crab-coordinator-{name}-linearizable-contract"),
            false,
            json!({
                "required_consistency": "multi-region-strong-consistency",
                "requires_same_account": true,
                "requires_conditional_writes": true,
                "transaction_strategy": "single-item-conditional-state-records",
                "unsupported_operations": ["TransactWriteItems", "TransactGetItems"],
                "fail_closed_if": [
                    "global-table-defaults-to-mrec",
                    "multi-account-global-table",
                    "transact-write-required-for-correctness"
                ],
                "ownership": tags,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::DynamoDb,
            "put-table-tags",
            &target,
            format!("crab-coordinator-{name}-tags"),
            true,
            json!({"ownership": tags}),
        ),
    ];
    CoordinatorControlPlanePlan {
        provider: ManagedCoordinatorProvider::DynamoDb,
        name: name.to_owned(),
        url: target,
        region: region.to_owned(),
        failover_regions: failover_regions.to_vec(),
        requests,
    }
}

fn dynamodb_default_witness_regions(regions: &[String]) -> Vec<String> {
    const REGION_SETS: &[&[&str]] = &[
        &["us-east-1", "us-east-2", "us-west-2"],
        &["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1"],
        &["ap-northeast-1", "ap-northeast-2", "ap-northeast-3"],
    ];
    if regions.len() != 2 {
        return Vec::new();
    }
    REGION_SETS
        .iter()
        .copied()
        .find(|region_set| {
            regions
                .iter()
                .all(|region| region_set.contains(&region.as_str()))
        })
        .and_then(|region_set| {
            region_set
                .iter()
                .find(|region| !regions.iter().any(|existing| existing == **region))
        })
        .map(|region| vec![(*region).to_owned()])
        .unwrap_or_default()
}

#[must_use]
pub fn spanner_coordinator_plan(
    name: &str,
    region: &str,
    failover_regions: &[String],
) -> CoordinatorControlPlanePlan {
    let mut regions = vec![region.to_owned()];
    for failover_region in failover_regions {
        if !regions.contains(failover_region) {
            regions.push(failover_region.clone());
        }
    }
    let labels = json!({
        "crab-managed": "true",
        "crab-resource": "write-coordinator",
        "crab-coordinator": name,
    });
    let target = format!("spanner://{name}");
    let requests = vec![
        coordinator_request(
            ManagedCoordinatorProvider::Spanner,
            "create-instance",
            &target,
            format!("crab-coordinator-{name}-instance"),
            true,
            json!({
                "instance_id": name,
                "instance_config_id": region,
                "regions": regions,
                "edition": "ENTERPRISE_PLUS",
                "required_consistency": "external-consistency",
                "labels": labels,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::Spanner,
            "validate-linearizable-contract",
            &target,
            format!("crab-coordinator-{name}-linearizable-contract"),
            false,
            json!({
                "required_consistency": "external-consistency",
                "requires_serializable_transactions": true,
                "requires_strong_reads": true,
                "transaction_authority": ["RepoEpoch", "RefState", "PushTransaction", "RepoState"],
                "fail_closed_if": [
                    "stale-read-used-for-write-admission",
                    "non-serializable-isolation",
                    "schema-drift"
                ],
                "labels": labels,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::Spanner,
            "create-database",
            &target,
            format!("crab-coordinator-{name}-database"),
            true,
            json!({
                "database_id": "crab_coordinator",
                "schema": [
                    "CREATE TABLE RepoEpoch (Repo STRING(MAX) NOT NULL, Epoch INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo)",
                    "CREATE TABLE RefState (Repo STRING(MAX) NOT NULL, Ref STRING(MAX) NOT NULL, Oid STRING(MAX), Epoch INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo, Ref)",
                    "CREATE TABLE PushTransaction (Repo STRING(MAX) NOT NULL, OperationId STRING(MAX) NOT NULL, State STRING(MAX) NOT NULL, Writer STRING(MAX) NOT NULL, Region STRING(MAX) NOT NULL, ManifestGeneration INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo, OperationId)",
                    "CREATE TABLE RepoState (Repo STRING(MAX) NOT NULL, Version INT64 NOT NULL, State STRING(MAX) NOT NULL, UpdatedAtMs INT64 NOT NULL) PRIMARY KEY (Repo)"
                ],
                "labels": labels,
            }),
        ),
    ];
    CoordinatorControlPlanePlan {
        provider: ManagedCoordinatorProvider::Spanner,
        name: name.to_owned(),
        url: target,
        region: region.to_owned(),
        failover_regions: failover_regions.to_vec(),
        requests,
    }
}

#[must_use]
pub fn cosmosdb_coordinator_plan(
    name: &str,
    region: &str,
    failover_regions: &[String],
) -> CoordinatorControlPlanePlan {
    let mut regions = vec![region.to_owned()];
    for failover_region in failover_regions {
        if !regions.contains(failover_region) {
            regions.push(failover_region.clone());
        }
    }
    let tags = json!({
        "crab:managed": "true",
        "crab:resource": "write-coordinator",
        "crab:coordinator": name,
    });
    let target = format!("cosmosdb://{name}");
    let requests = vec![
        coordinator_request(
            ManagedCoordinatorProvider::CosmosDb,
            "create-account",
            &target,
            format!("crab-coordinator-{name}-account"),
            true,
            json!({
                "account_name": name,
                "regions": regions,
                "consistency": "Strong",
                "write_mode": "single-write-region-with-fenced-failover",
                "automatic_failover": true,
                "tags": tags,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::CosmosDb,
            "validate-linearizable-contract",
            &target,
            format!("crab-coordinator-{name}-linearizable-contract"),
            false,
            json!({
                "required_consistency": "Strong",
                "write_mode": "single-write-region-with-fenced-failover",
                "multi_region_writes": "unsupported-for-active-active-git-ref-cas",
                "requires_session_tokens_for_reads_only": true,
                "transaction_authority": [
                    "repo_state"
                ],
                "fail_closed_if": [
                    "multi-region-writes-enabled",
                    "consistency-not-strong",
                    "failover-epoch-not-fenced"
                ],
                "tags": tags,
            }),
        ),
        coordinator_request(
            ManagedCoordinatorProvider::CosmosDb,
            "create-database-and-containers",
            &target,
            format!("crab-coordinator-{name}-containers"),
            true,
            json!({
                "database": "crab-coordinator",
                "containers": [
                    {"name": "repo_epoch", "partition_key": "/repo"},
                    {"name": "ref_state", "partition_key": "/repo"},
                    {"name": "push_transaction", "partition_key": "/repo"},
                    {"name": "repo_state", "partition_key": "/repo"}
                ],
                "tags": tags,
            }),
        ),
    ];
    CoordinatorControlPlanePlan {
        provider: ManagedCoordinatorProvider::CosmosDb,
        name: name.to_owned(),
        url: target,
        region: region.to_owned(),
        failover_regions: failover_regions.to_vec(),
        requests,
    }
}

#[must_use]
pub fn coordinator_control_plane_remove_plan(
    plan: &CoordinatorControlPlanePlan,
) -> CoordinatorControlPlanePlan {
    let mut remove = plan.clone();
    remove.requests = remove
        .requests
        .into_iter()
        .filter(|request| request.reversible)
        .map(|mut request| {
            request.action = format!("remove:{}", request.action);
            request
        })
        .collect();
    remove
}

/// Builds unknown checks for a planned coordinator without a live backend.
#[must_use]
pub fn inspect_coordinator_control_plane_plan(
    plan: &CoordinatorControlPlanePlan,
) -> CoordinatorControlPlaneStatus {
    let checks = plan
        .requests
        .iter()
        .map(|request| {
            coordinator_control_plane_check(
                request,
                CoordinatorCheckState::Unknown,
                format!(
                    "{} coordinator status backend is not wired for {}",
                    request.provider.as_str(),
                    request.action
                ),
                "rerun failover status after the managed coordinator status backend is available",
            )
        })
        .collect();

    CoordinatorControlPlaneStatus {
        provider: plan.provider,
        name: plan.name.clone(),
        url: plan.url.clone(),
        region: plan.region.clone(),
        failover_regions: plan.failover_regions.clone(),
        backend_available: false,
        checked_drift: false,
        checks,
    }
}

#[must_use]
pub fn coordinator_control_plane_check(
    request: &CoordinatorControlPlaneRequest,
    state: CoordinatorCheckState,
    message: String,
    remediation: &str,
) -> CoordinatorControlPlaneCheck {
    CoordinatorControlPlaneCheck {
        provider: request.provider,
        code: coordinator_check_code(request.provider, &request.action),
        state,
        action: request.action.clone(),
        target: request.target.clone(),
        managed_resource_id: request.managed_resource_id.clone(),
        message,
        remediation: remediation.to_owned(),
    }
}

fn coordinator_check_code(provider: ManagedCoordinatorProvider, action: &str) -> String {
    let provider = provider.as_str();
    match action {
        "create-global-table" => format!("coordinator.{provider}.global-table.unverified"),
        "validate-linearizable-contract" => {
            format!("coordinator.{provider}.linearizable-contract.unverified")
        }
        "put-table-tags" => format!("coordinator.{provider}.tags.unverified"),
        "create-instance" => format!("coordinator.{provider}.instance.unverified"),
        "create-database" => format!("coordinator.{provider}.database.unverified"),
        "create-account" => format!("coordinator.{provider}.account.unverified"),
        "create-database-and-containers" => {
            format!("coordinator.{provider}.containers.unverified")
        }
        other => format!(
            "coordinator.{provider}.{}.unverified",
            other.replace([':', '_'], "-")
        ),
    }
}

fn coordinator_request(
    provider: ManagedCoordinatorProvider,
    action: &str,
    target: &str,
    managed_resource_id: String,
    reversible: bool,
    request: serde_json::Value,
) -> CoordinatorControlPlaneRequest {
    CoordinatorControlPlaneRequest {
        provider,
        action: action.to_owned(),
        target: target.to_owned(),
        request,
        reversible,
        managed_resource_id,
    }
}

/// Stable fingerprint for an active-active commit request.
///
/// Completed-operation replay records use this to distinguish an idempotent
/// retry from an operation-id collision with different commit content.
pub fn coordinator_request_fingerprint(request: &CommitRequest) -> Result<String> {
    let bytes = serde_json::to_vec(request).map_err(|source| CoordinationError::Serialize {
        key: "replication.coordinator.operation_id".to_owned(),
        context: "failed to serialize coordinator commit request",
        source,
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Builds a compact replay record for a terminal coordinator operation.
///
/// Only materialized and aborted transactions are eligible for replay
/// compaction. Materialized operations must preserve the committed outcome.
pub fn coordinator_completed_operation_record(
    request: &CommitRequest,
    outcome: Option<CommitOutcome>,
    state: PushTransactionState,
    coordinator_epoch: u64,
    sequence: u64,
) -> Result<CoordinatorCompletedOperationRecord> {
    if state == PushTransactionState::Materialized && outcome.is_none() {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.completed_operations".to_owned(),
            origin: format!(
                "materialized operation {} is missing its committed outcome",
                request.operation_id
            ),
        });
    }
    if !matches!(
        state,
        PushTransactionState::Materialized | PushTransactionState::Aborted
    ) {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.completed_operations".to_owned(),
            origin: format!(
                "operation {} cannot be compacted from non-terminal state {:?}",
                request.operation_id, state
            ),
        });
    }
    Ok(CoordinatorCompletedOperationRecord {
        request_fingerprint: coordinator_request_fingerprint(request)?,
        outcome,
        state,
        target_regions: coordinator_effective_target_regions(request),
        coordinator_epoch,
        sequence,
    })
}

/// Returns the replayed state for a completed operation after request validation.
pub fn coordinator_completed_operation_state(
    record: &CoordinatorCompletedOperationRecord,
    request: &CommitRequest,
) -> Result<PushTransactionState> {
    ensure_completed_operation_request(record, request)?;
    Ok(record.state)
}

/// Returns the replayed commit outcome for a completed operation.
pub fn coordinator_completed_operation_outcome(
    record: &CoordinatorCompletedOperationRecord,
    request: &CommitRequest,
) -> Result<CommitOutcome> {
    ensure_completed_operation_request(record, request)?;
    if record.state == PushTransactionState::Aborted {
        return Err(CoordinationError::CasConflict {
            path: format!("coordinator/transactions/{}", request.operation_id),
            expected_etag: None,
        });
    }
    record
        .outcome
        .clone()
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.coordinator.completed_operations".to_owned(),
            origin: format!(
                "completed operation {} is missing its committed outcome",
                request.operation_id
            ),
        })
}

fn ensure_completed_operation_request(
    record: &CoordinatorCompletedOperationRecord,
    request: &CommitRequest,
) -> Result<()> {
    if record.request_fingerprint == coordinator_request_fingerprint(request)? {
        return Ok(());
    }
    Err(CoordinationError::CasConflict {
        path: format!("coordinator/transactions/{}", request.operation_id),
        expected_etag: None,
    })
}

/// Returns materialized state for a completed materialization replay record.
pub fn coordinator_completed_materialized_state(
    record: &CoordinatorCompletedOperationRecord,
    operation_id: &str,
) -> Result<PushTransactionState> {
    if record.state == PushTransactionState::Materialized {
        return Ok(PushTransactionState::Materialized);
    }
    Err(CoordinationError::CasConflict {
        path: format!("coordinator/transactions/{operation_id}"),
        expected_etag: None,
    })
}

/// Returns materialized state for a region if the replay record targeted it.
pub fn coordinator_completed_region_materialized_state(
    record: &CoordinatorCompletedOperationRecord,
    operation_id: &str,
    region: &str,
) -> Result<PushTransactionState> {
    coordinator_completed_materialized_state(record, operation_id)?;
    if record.target_regions.contains(region) {
        return Ok(PushTransactionState::Materialized);
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator.materialization".to_owned(),
        origin: format!("region {region} is not a materialization target"),
    })
}

/// Returns the deduplicated non-empty materialization target regions.
#[must_use]
pub fn coordinator_effective_target_regions(request: &CommitRequest) -> BTreeSet<String> {
    request
        .target_regions
        .iter()
        .chain(std::iter::once(&request.region))
        .filter_map(|region| {
            let region = region.trim();
            (!region.is_empty()).then(|| region.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn commit_request_defaults_missing_optional_lists() {
        let request: CommitRequest = serde_json::from_str(
            r#"{
                "operation_id": "op-1",
                "writer": "writer-a",
                "region": "us-west-2",
                "manifest_generation": 7,
                "refs": []
            }"#,
        )
        .unwrap();

        assert!(request.uploaded_objects.is_empty());
        assert!(request.target_regions.is_empty());
    }

    #[test]
    fn gc_safety_snapshot_reports_protected_keys() {
        let snapshot = CoordinatorGcSafetySnapshot {
            coordinator_epoch: 4,
            protected_objects: vec![
                CoordinatorProtectedObject {
                    key: "repo/.crab/xorbs/a".to_owned(),
                    operation_id: "op-1".to_owned(),
                    state: PushTransactionState::ObjectsUploaded,
                    manifest_generation: 2,
                    writer: "writer-a".to_owned(),
                    region: "us-west-2".to_owned(),
                },
                CoordinatorProtectedObject {
                    key: "repo/.crab/xorbs/b".to_owned(),
                    operation_id: "op-2".to_owned(),
                    state: PushTransactionState::Committed,
                    manifest_generation: 3,
                    writer: "writer-b".to_owned(),
                    region: "us-east-1".to_owned(),
                },
            ],
        };

        let keys = snapshot.protected_keys();
        assert!(keys.contains("repo/.crab/xorbs/a"));
        assert!(keys.contains("repo/.crab/xorbs/b"));
    }

    #[test]
    fn managed_provider_strings_match_wire_values() {
        assert_eq!(ManagedCoordinatorProvider::DynamoDb.as_str(), "dynamodb");
        assert_eq!(ManagedCoordinatorProvider::Spanner.as_str(), "spanner");
        assert_eq!(ManagedCoordinatorProvider::CosmosDb.as_str(), "cosmosdb");
    }

    #[test]
    fn dynamodb_plan_uses_witness_for_two_region_mrsc_topology() {
        let plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);

        let create = &plan.requests[0].request;

        assert_eq!(
            create["deployment_topology"].as_str(),
            Some("two-replicas-one-witness")
        );
        assert_eq!(create["witness_regions"].as_array().map(Vec::len), Some(1));
        assert_eq!(create["witness_regions"][0].as_str(), Some("us-east-2"));
    }

    #[test]
    fn remove_plan_keeps_only_reversible_actions() {
        let plan = spanner_coordinator_plan("crab-coordinator", "nam3", &["eur3".to_owned()]);

        let remove = coordinator_control_plane_remove_plan(&plan);

        assert_eq!(remove.requests.len(), 2);
        assert!(remove.requests.iter().all(|request| request.reversible));
        assert!(
            remove
                .requests
                .iter()
                .all(|request| request.action.starts_with("remove:"))
        );
    }

    #[test]
    fn unknown_inspection_builds_stable_check_codes() {
        let plan = cosmosdb_coordinator_plan("crab-coordinator", "eastus", &["westus2".to_owned()]);

        let status = inspect_coordinator_control_plane_plan(&plan);

        assert!(!status.backend_available);
        assert!(!status.checked_drift);
        assert_eq!(
            status.checks[0].code,
            "coordinator.cosmosdb.account.unverified"
        );
        assert_eq!(status.checks[0].state, CoordinatorCheckState::Unknown);
    }

    #[tokio::test]
    async fn versioned_state_coordinator_commits_and_materializes_ref() {
        let store = TestVersionedStateStore::default();
        store.seed_ref("refs/heads/main", "abc");
        let coordinator = VersionedStateWriteCoordinator::new("test", "namespace", "repo", &store);
        let mut request = request("op-versioned-1");
        request.uploaded_objects = vec!["repo/.crab/xorbs/a".to_owned()];

        assert_eq!(
            coordinator.begin(request.clone()).await.unwrap(),
            PushTransactionState::Pending
        );
        assert_eq!(
            coordinator
                .mark_objects_uploaded(&request.operation_id)
                .await
                .unwrap(),
            PushTransactionState::ObjectsUploaded
        );
        let outcome = coordinator.commit(request.clone()).await.unwrap();

        assert_eq!(outcome.state, PushTransactionState::Committed);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("bcd".to_owned())
        );
        assert_eq!(
            coordinator
                .mark_region_materialized(&request.operation_id, "us-west-2")
                .await
                .unwrap(),
            PushTransactionState::Materialized
        );
    }

    #[tokio::test]
    async fn versioned_state_coordinator_reports_non_fast_forward() {
        let store = TestVersionedStateStore::default();
        store.seed_ref("refs/heads/main", "actual");
        let coordinator = VersionedStateWriteCoordinator::new("test", "namespace", "repo", &store);

        let err = coordinator
            .commit(request("op-versioned-2"))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            CoordinationError::NonFastForward {
                ref_name,
                have,
                want
            } if ref_name == "refs/heads/main" && have == "actual" && want == "abc"
        ));
    }

    #[tokio::test]
    async fn in_memory_coordinator_commits_and_materializes_ref() {
        let coordinator = InMemoryWriteCoordinator::new();
        coordinator.seed_ref("refs/heads/main", "abc").await;
        let mut request = request("op-memory-1");
        request.uploaded_objects = vec!["repo/.crab/xorbs/a".to_owned()];

        assert_eq!(
            coordinator.begin(request.clone()).await.unwrap(),
            PushTransactionState::Pending
        );
        assert_eq!(
            coordinator
                .mark_objects_uploaded(&request.operation_id)
                .await
                .unwrap(),
            PushTransactionState::ObjectsUploaded
        );
        let outcome = coordinator.commit(request.clone()).await.unwrap();

        assert_eq!(outcome.state, PushTransactionState::Committed);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("bcd".to_owned())
        );
        assert_eq!(
            coordinator
                .mark_region_materialized(&request.operation_id, "us-west-2")
                .await
                .unwrap(),
            PushTransactionState::Materialized
        );
    }

    #[tokio::test]
    async fn in_memory_coordinator_reports_non_fast_forward() {
        let coordinator = InMemoryWriteCoordinator::new();
        coordinator.seed_ref("refs/heads/main", "actual").await;

        let err = coordinator
            .commit(request("op-memory-2"))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            CoordinationError::NonFastForward {
                ref_name,
                have,
                want
            } if ref_name == "refs/heads/main" && have == "actual" && want == "abc"
        ));
    }

    #[test]
    fn completed_operation_record_requires_terminal_state() {
        let err = coordinator_completed_operation_record(
            &request("op-1"),
            None,
            PushTransactionState::Committed,
            3,
            1,
        )
        .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
    }

    #[test]
    fn materialized_completed_operation_preserves_outcome() {
        let request = request("op-1");
        let outcome = CommitOutcome {
            operation_id: request.operation_id.clone(),
            coordinator_epoch: 4,
            writer: request.writer.clone(),
            region: request.region.clone(),
            manifest_generation: request.manifest_generation,
            state: PushTransactionState::Materialized,
        };

        let record = coordinator_completed_operation_record(
            &request,
            Some(outcome.clone()),
            PushTransactionState::Materialized,
            4,
            7,
        )
        .unwrap();

        assert_eq!(
            coordinator_completed_operation_outcome(&record, &request).unwrap(),
            outcome
        );
    }

    #[test]
    fn completed_operation_rejects_operation_id_collision() {
        let request = request("op-1");
        let record = coordinator_completed_operation_record(
            &request,
            None,
            PushTransactionState::Aborted,
            4,
            7,
        )
        .unwrap();
        let mut different = request;
        different.refs[0].new = Some("def".to_owned());

        let err = coordinator_completed_operation_state(&record, &different).unwrap_err();

        assert!(matches!(err, CoordinationError::CasConflict { .. }));
    }

    #[test]
    fn target_regions_are_trimmed_and_deduplicated() {
        let mut request = request("op-1");
        request.region = "us-west-2".to_owned();
        request.target_regions = vec![
            "us-east-1".to_owned(),
            " ".to_owned(),
            "us-west-2".to_owned(),
        ];

        let regions = coordinator_effective_target_regions(&request);

        assert_eq!(
            regions,
            BTreeSet::from(["us-east-1".to_owned(), "us-west-2".to_owned()])
        );
    }

    #[derive(Default)]
    struct TestVersionedStateStore {
        record: Mutex<Option<CoordinatorStateRecord>>,
    }

    impl TestVersionedStateStore {
        fn seed_ref(&self, name: &str, value: &str) {
            let mut state = CoordinatorRepoState::default();
            state.refs.insert(name.to_owned(), value.to_owned());
            self.record
                .lock()
                .unwrap()
                .replace(CoordinatorStateRecord { version: 1, state });
        }
    }

    #[async_trait::async_trait]
    impl VersionedCoordinatorStateStore for TestVersionedStateStore {
        async fn read_repo_state(
            &self,
            _namespace: &str,
            _repo_key: &str,
        ) -> Result<Option<CoordinatorStateRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }

        async fn compare_and_swap_repo_state(
            &self,
            _namespace: &str,
            _repo_key: &str,
            expected_version: Option<u64>,
            next_state: &CoordinatorRepoState,
        ) -> Result<bool> {
            let mut record = self.record.lock().unwrap();
            let current_version = record.as_ref().map(|record| record.version);
            if current_version != expected_version {
                return Ok(false);
            }
            record.replace(CoordinatorStateRecord {
                version: expected_version.unwrap_or_default().saturating_add(1),
                state: next_state.clone(),
            });
            Ok(true)
        }
    }

    fn request(operation_id: &str) -> CommitRequest {
        CommitRequest {
            operation_id: operation_id.to_owned(),
            writer: "writer-a".to_owned(),
            region: "us-west-2".to_owned(),
            manifest_generation: 9,
            refs: vec![CoordinatedRefUpdate {
                name: "refs/heads/main".to_owned(),
                expected: Some("abc".to_owned()),
                new: Some("bcd".to_owned()),
                force: false,
            }],
            uploaded_objects: Vec::new(),
            target_regions: Vec::new(),
        }
    }
}
