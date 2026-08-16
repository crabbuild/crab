//! Cosmos DB coordinator control-plane backend.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "coordinator-cosmosdb")]
use std::fmt;
#[cfg(feature = "coordinator-cosmosdb")]
use std::sync::Arc;
#[cfg(feature = "coordinator-cosmosdb")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
#[cfg(feature = "coordinator-cosmosdb")]
use serde_json::Value;

use crate::error::{CoordinationError, Result};
#[cfg(feature = "coordinator-cosmosdb")]
use crate::write_coordinator::coordination_error;
use crate::write_coordinator::{
    CommitOutcome, CommitRequest, CoordinatorApplyStatus, CoordinatorCheckState,
    CoordinatorControlPlaneBackend, CoordinatorControlPlanePlan, CoordinatorControlPlaneRequest,
    CoordinatorControlPlaneStatus, CoordinatorFenceOutcome, CoordinatorGcSafetySnapshot,
    CoordinatorHealth, CoordinatorRepairSnapshot, ManagedCoordinatorProvider, PushTransactionState,
    VersionedCoordinatorStateStore, VersionedStateWriteCoordinator, WriteCoordinator,
    coordinator_control_plane_check,
};
#[cfg(feature = "coordinator-cosmosdb")]
use crate::write_coordinator::{CoordinatorRepoState, CoordinatorStateRecord};

pub const COSMOSDB_DATABASE_NAME: &str = "crab-coordinator";

const TAG_MANAGED: &str = "crab:managed";
const TAG_RESOURCE: &str = "crab:resource";
const TAG_COORDINATOR: &str = "crab:coordinator";
const COSMOSDB_STATE_CONTAINER: &str = "repo_state";
#[cfg(feature = "coordinator-cosmosdb")]
const COSMOSDB_API_VERSION: &str = "2018-12-31";

/// Existing Cosmos DB coordinator account state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosmosDbCoordinatorAccount {
    pub account_name: String,
    pub regions: Vec<String>,
    pub failover_priority_regions: Vec<String>,
    pub consistency: String,
    pub write_mode: String,
    pub multi_region_writes_enabled: bool,
    pub automatic_failover: bool,
    pub tags: BTreeMap<String, String>,
    pub database: Option<CosmosDbCoordinatorDatabase>,
}

/// Existing Cosmos DB coordinator database state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosmosDbCoordinatorDatabase {
    pub database_name: String,
    pub containers: Vec<String>,
}

/// Create request for a Cosmos DB coordinator account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosmosDbCreateCoordinatorAccount {
    pub account_name: String,
    pub regions: Vec<String>,
    pub failover_priority_regions: Vec<String>,
    pub consistency: String,
    pub write_mode: String,
    pub automatic_failover: bool,
    pub tags: BTreeMap<String, String>,
}

/// Minimal Cosmos DB control-plane client needed by Crab-owned coordinator setup.
#[async_trait]
pub trait CosmosDbCoordinatorControlPlaneClient {
    async fn describe_account(
        &self,
        account_name: &str,
    ) -> Result<Option<CosmosDbCoordinatorAccount>>;
    async fn create_account(&self, request: CosmosDbCreateCoordinatorAccount) -> Result<()>;
    async fn create_database_and_containers(
        &self,
        account_name: &str,
        database: CosmosDbCoordinatorDatabase,
    ) -> Result<()>;
    async fn delete_account(&self, account_name: &str) -> Result<()>;
}

#[async_trait]
impl<T> CosmosDbCoordinatorControlPlaneClient for &T
where
    T: CosmosDbCoordinatorControlPlaneClient + Send + Sync + ?Sized,
{
    async fn describe_account(
        &self,
        account_name: &str,
    ) -> Result<Option<CosmosDbCoordinatorAccount>> {
        (*self).describe_account(account_name).await
    }

    async fn create_account(&self, request: CosmosDbCreateCoordinatorAccount) -> Result<()> {
        (*self).create_account(request).await
    }

    async fn create_database_and_containers(
        &self,
        account_name: &str,
        database: CosmosDbCoordinatorDatabase,
    ) -> Result<()> {
        (*self)
            .create_database_and_containers(account_name, database)
            .await
    }

    async fn delete_account(&self, account_name: &str) -> Result<()> {
        (*self).delete_account(account_name).await
    }
}

/// Cosmos DB data-plane client for one strong single-writer repo state record.
pub trait CosmosDbWriteCoordinatorClient: VersionedCoordinatorStateStore {}

impl<T> CosmosDbWriteCoordinatorClient for T where T: VersionedCoordinatorStateStore {}

/// Cosmos DB implementation of the active-active write coordinator data plane.
pub struct CosmosDbWriteCoordinator<C> {
    inner: VersionedStateWriteCoordinator<C>,
}

impl<C> CosmosDbWriteCoordinator<C> {
    #[must_use]
    pub fn new(
        account_name: impl Into<String>,
        database_name: impl Into<String>,
        repo_key: impl Into<String>,
        client: C,
    ) -> Self {
        let namespace = format!("{}/{}", account_name.into(), database_name.into());
        Self {
            inner: VersionedStateWriteCoordinator::new("cosmosdb", namespace, repo_key, client),
        }
    }
}

#[async_trait]
impl<C> WriteCoordinator for CosmosDbWriteCoordinator<C>
where
    C: CosmosDbWriteCoordinatorClient,
{
    async fn health(&self) -> crate::Result<CoordinatorHealth> {
        self.inner.health().await
    }

    async fn begin(&self, request: CommitRequest) -> crate::Result<PushTransactionState> {
        self.inner.begin(request).await
    }

    async fn mark_objects_uploaded(
        &self,
        operation_id: &str,
    ) -> crate::Result<PushTransactionState> {
        self.inner.mark_objects_uploaded(operation_id).await
    }

    async fn commit(&self, request: CommitRequest) -> crate::Result<CommitOutcome> {
        self.inner.commit(request).await
    }

    async fn mark_materialized(&self, operation_id: &str) -> crate::Result<PushTransactionState> {
        self.inner.mark_materialized(operation_id).await
    }

    async fn mark_region_materialized(
        &self,
        operation_id: &str,
        region: &str,
    ) -> crate::Result<PushTransactionState> {
        self.inner
            .mark_region_materialized(operation_id, region)
            .await
    }

    async fn abort(&self, operation_id: &str) -> crate::Result<PushTransactionState> {
        self.inner.abort(operation_id).await
    }

    async fn ref_value(&self, name: &str) -> crate::Result<Option<String>> {
        self.inner.ref_value(name).await
    }

    async fn gc_safety_snapshot(&self) -> crate::Result<CoordinatorGcSafetySnapshot> {
        self.inner.gc_safety_snapshot().await
    }

    async fn repair_snapshot(&self) -> crate::Result<CoordinatorRepairSnapshot> {
        self.inner.repair_snapshot().await
    }

    async fn fence_writes(&self, reason: Option<String>) -> crate::Result<CoordinatorFenceOutcome> {
        self.inner.fence_writes(reason).await
    }

    async fn resume_writes(&self) -> crate::Result<CoordinatorFenceOutcome> {
        self.inner.resume_writes().await
    }
}

/// Azure Cosmos DB REST data-plane client for the active-active coordinator.
#[cfg(feature = "coordinator-cosmosdb")]
#[derive(Clone)]
pub struct AzureCosmosDbWriteCoordinatorClient {
    credential: Arc<dyn azure_core::auth::TokenCredential>,
    http: reqwest::Client,
}

#[cfg(feature = "coordinator-cosmosdb")]
impl AzureCosmosDbWriteCoordinatorClient {
    pub fn new() -> Result<Self> {
        let credential = azure_identity::create_credential().map_err(cosmosdb_auth_error)?;
        Ok(Self {
            credential,
            http: reqwest::Client::new(),
        })
    }

    async fn read_state_document(
        &self,
        account_name: &str,
        database_name: &str,
        repo_key: &str,
    ) -> Result<Option<CosmosDbStateDocument>> {
        let doc_id = cosmosdb_state_document_id(repo_key);
        let response = self
            .http
            .get(cosmosdb_data_url(
                account_name,
                &cosmosdb_doc_path(database_name, &doc_id),
            ))
            .headers(self.data_plane_headers(account_name, repo_key).await?)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error("read Cosmos DB coordinator state", err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| cosmosdb_request_error("read Cosmos DB coordinator state", err))?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(cosmosdb_status_body_error(
                "read Cosmos DB coordinator state",
                status,
                &body,
            ));
        }
        let value = serde_json::from_str::<Value>(&body).map_err(|err| {
            cosmosdb_state_shape_error(repo_key, &format!("invalid document JSON: {err}"))
        })?;
        cosmosdb_state_document_from_json(repo_key, &value).map(Some)
    }

    async fn create_state_document(
        &self,
        account_name: &str,
        database_name: &str,
        repo_key: &str,
        version: u64,
        next_state: &CoordinatorRepoState,
    ) -> Result<bool> {
        let doc_id = cosmosdb_state_document_id(repo_key);
        let body = cosmosdb_state_document_body(repo_key, &doc_id, version, next_state)?;
        let response = self
            .http
            .post(cosmosdb_data_url(
                account_name,
                &cosmosdb_docs_path(database_name),
            ))
            .headers(self.data_plane_headers(account_name, repo_key).await?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error("create Cosmos DB coordinator state", err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| cosmosdb_request_error("create Cosmos DB coordinator state", err))?;
        if status.is_success() {
            return Ok(true);
        }
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(false);
        }
        Err(cosmosdb_status_body_error(
            "create Cosmos DB coordinator state",
            status,
            &body,
        ))
    }

    async fn replace_state_document(
        &self,
        account_name: &str,
        database_name: &str,
        repo_key: &str,
        etag: &str,
        version: u64,
        next_state: &CoordinatorRepoState,
    ) -> Result<bool> {
        let doc_id = cosmosdb_state_document_id(repo_key);
        let body = cosmosdb_state_document_body(repo_key, &doc_id, version, next_state)?;
        let response = self
            .http
            .put(cosmosdb_data_url(
                account_name,
                &cosmosdb_doc_path(database_name, &doc_id),
            ))
            .headers(self.data_plane_headers(account_name, repo_key).await?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::IF_MATCH, etag)
            .json(&body)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error("replace Cosmos DB coordinator state", err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| cosmosdb_request_error("replace Cosmos DB coordinator state", err))?;
        if status.is_success() {
            return Ok(true);
        }
        if matches!(
            status,
            reqwest::StatusCode::PRECONDITION_FAILED
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::CONFLICT
        ) {
            return Ok(false);
        }
        Err(cosmosdb_status_body_error(
            "replace Cosmos DB coordinator state",
            status,
            &body,
        ))
    }

    async fn data_plane_headers(
        &self,
        account_name: &str,
        repo_key: &str,
    ) -> Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            self.aad_authorization().await?.parse().map_err(|err| {
                CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb.auth".into(),
                    origin: format!("failed to build Cosmos DB authorization header: {err}"),
                }
            })?,
        );
        headers.insert(
            reqwest::header::ACCEPT,
            "application/json"
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb".into(),
                    origin: format!("failed to build Cosmos DB accept header: {err}"),
                })?,
        );
        headers.insert(
            "x-ms-date",
            cosmosdb_http_date(SystemTime::now())?
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb.date".into(),
                    origin: format!("failed to build Cosmos DB date header: {err}"),
                })?,
        );
        headers.insert(
            "x-ms-version",
            COSMOSDB_API_VERSION
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb".into(),
                    origin: format!("failed to build Cosmos DB version header: {err}"),
                })?,
        );
        headers.insert(
            "x-ms-consistency-level",
            "Strong"
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb".into(),
                    origin: format!("failed to build Cosmos DB consistency header: {err}"),
                })?,
        );
        headers.insert(
            "x-ms-documentdb-partitionkey",
            cosmosdb_partition_key_header(repo_key)
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb.partition".into(),
                    origin: format!("failed to build Cosmos DB partition key header: {err}"),
                })?,
        );
        headers.insert(
            "x-ms-cosmos-allow-tentative-writes",
            "false"
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb".into(),
                    origin: format!("failed to build Cosmos DB tentative writes header: {err}"),
                })?,
        );
        headers.insert(
            reqwest::header::HOST,
            format!("{account_name}.documents.azure.com")
                .parse()
                .map_err(|err| CoordinationError::Configuration {
                    key: "replication.coordinator.cosmosdb".into(),
                    origin: format!("failed to build Cosmos DB host header: {err}"),
                })?,
        );
        Ok(headers)
    }

    async fn aad_authorization(&self) -> Result<String> {
        let token = self
            .credential
            .get_token(&["https://cosmos.azure.com/.default"])
            .await
            .map_err(cosmosdb_auth_error)?;
        let auth = format!("type=aad&ver=1.0&sig={}", token.token.secret());
        Ok(urlencoding::encode(&auth).into_owned())
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
#[async_trait]
impl VersionedCoordinatorStateStore for AzureCosmosDbWriteCoordinatorClient {
    async fn read_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
    ) -> crate::Result<Option<CoordinatorStateRecord>> {
        async {
            let (account_name, database_name) = cosmosdb_namespace_parts(namespace)?;
            Ok(self
                .read_state_document(account_name, database_name, repo_key)
                .await?
                .map(|document| document.record))
        }
        .await
        .map_err(coordination_error)
    }

    async fn compare_and_swap_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &CoordinatorRepoState,
    ) -> crate::Result<bool> {
        async {
            let (account_name, database_name) = cosmosdb_namespace_parts(namespace)?;
            let current = self
                .read_state_document(account_name, database_name, repo_key)
                .await?;
            let current_version = current.as_ref().map(|document| document.record.version);
            if current_version != expected_version {
                return Ok(false);
            }

            let next_version = expected_version.unwrap_or_default().saturating_add(1);
            match current {
                Some(document) => {
                    self.replace_state_document(
                        account_name,
                        database_name,
                        repo_key,
                        &document.etag,
                        next_version,
                        next_state,
                    )
                    .await
                }
                None => {
                    self.create_state_document(
                        account_name,
                        database_name,
                        repo_key,
                        next_version,
                        next_state,
                    )
                    .await
                }
            }
        }
        .await
        .map_err(coordination_error)
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
struct CosmosDbStateDocument {
    record: CoordinatorStateRecord,
    etag: String,
}

/// Cosmos DB implementation of the managed coordinator control-plane contract.
pub struct CosmosDbCoordinatorBackend<C> {
    client: C,
}

impl<C> CosmosDbCoordinatorBackend<C> {
    #[must_use]
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C> CoordinatorControlPlaneBackend for CosmosDbCoordinatorBackend<C>
where
    C: CosmosDbCoordinatorControlPlaneClient + Send + Sync,
{
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::CosmosDb
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        ensure_cosmosdb_plan(plan)?;
        let mut actions = Vec::new();
        match self.client.describe_account(&plan.name).await? {
            None => {
                self.client
                    .create_account(create_account_request(plan))
                    .await?;
                self.client
                    .create_database_and_containers(&plan.name, planned_database())
                    .await?;
                actions.push("create-account".to_owned());
                actions.push("create-database-and-containers".to_owned());
            }
            Some(account) if account.database.is_none() => {
                self.client
                    .create_database_and_containers(&plan.name, planned_database())
                    .await?;
                actions.push("create-database-and-containers".to_owned());
            }
            Some(_) => {}
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::CosmosDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("Cosmos DB coordinator {} is applied", plan.name),
        })
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        ensure_cosmosdb_plan(plan)?;
        let account = self.client.describe_account(&plan.name).await?;
        let checks = plan
            .requests
            .iter()
            .map(|request| cosmosdb_check(plan, request, account.as_ref()))
            .collect();

        Ok(CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::CosmosDb,
            name: plan.name.clone(),
            url: plan.url.clone(),
            region: plan.region.clone(),
            failover_regions: plan.failover_regions.clone(),
            backend_available: true,
            checked_drift: true,
            checks,
        })
    }

    async fn remove(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        ensure_cosmosdb_plan(plan)?;
        let mut actions = Vec::new();
        if self.client.describe_account(&plan.name).await?.is_some() {
            self.client.delete_account(&plan.name).await?;
            actions.push("remove:create-account".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::CosmosDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("Cosmos DB coordinator {} is removed", plan.name),
        })
    }
}

fn ensure_cosmosdb_plan(plan: &CoordinatorControlPlanePlan) -> Result<()> {
    if plan.provider == ManagedCoordinatorProvider::CosmosDb {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "Cosmos DB coordinator backend cannot manage {} coordinator plan",
            plan.provider.as_str()
        ),
    })
}

fn cosmosdb_check(
    plan: &CoordinatorControlPlanePlan,
    request: &CoordinatorControlPlaneRequest,
    account: Option<&CosmosDbCoordinatorAccount>,
) -> crate::write_coordinator::CoordinatorControlPlaneCheck {
    let action = request
        .action
        .strip_prefix("remove:")
        .unwrap_or(request.action.as_str());
    let (state, message) = match action {
        "create-account" => account_state(plan, account),
        "validate-linearizable-contract" => linearizable_state(plan, account),
        "create-database-and-containers" => database_state(plan, account),
        _ => (
            CoordinatorCheckState::Unsupported,
            format!(
                "Cosmos DB coordinator action {} is unsupported",
                request.action
            ),
        ),
    };
    coordinator_control_plane_check(
        request,
        state,
        message,
        "repair coordinator resources through crab replica coordinator add --apply",
    )
}

fn account_state(
    plan: &CoordinatorControlPlanePlan,
    account: Option<&CosmosDbCoordinatorAccount>,
) -> (CoordinatorCheckState, String) {
    let Some(account) = account else {
        return (
            CoordinatorCheckState::Missing,
            format!("Cosmos DB coordinator account {} is missing", plan.name),
        );
    };
    if account.account_name != plan.name {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB account {} does not match planned coordinator {}",
                account.account_name, plan.name
            ),
        );
    }
    if planned_regions(plan) != account.regions.iter().cloned().collect::<BTreeSet<_>>() {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} regions do not match the plan",
                plan.name
            ),
        );
    }
    if planned_failover_priority_regions(plan) != account.failover_priority_regions {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} failover priority order does not match the planned write and failover order",
                plan.name
            ),
        );
    }
    if !account.automatic_failover {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} must enable fenced failover",
                plan.name
            ),
        );
    }
    if !ownership_tags_match(plan, &account.tags) {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} ownership tags are invalid",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "Cosmos DB coordinator {} account matches the plan",
            plan.name
        ),
    )
}

fn linearizable_state(
    plan: &CoordinatorControlPlanePlan,
    account: Option<&CosmosDbCoordinatorAccount>,
) -> (CoordinatorCheckState, String) {
    let Some(account) = account else {
        return (
            CoordinatorCheckState::Verified,
            format!(
                "Cosmos DB coordinator {} planned account satisfies the strong single-write failover contract",
                plan.name
            ),
        );
    };
    if account.consistency != "Strong"
        || account.write_mode != "single-write-region-with-fenced-failover"
        || account.multi_region_writes_enabled
    {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} must use Strong consistency with single-write fenced failover",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "Cosmos DB coordinator {} satisfies the strong single-write failover contract",
            plan.name
        ),
    )
}

fn database_state(
    plan: &CoordinatorControlPlanePlan,
    account: Option<&CosmosDbCoordinatorAccount>,
) -> (CoordinatorCheckState, String) {
    let Some(account) = account else {
        return (
            CoordinatorCheckState::Missing,
            format!("Cosmos DB coordinator {} containers are missing", plan.name),
        );
    };
    let Some(database) = account.database.as_ref() else {
        return (
            CoordinatorCheckState::Missing,
            format!("Cosmos DB coordinator {} containers are missing", plan.name),
        );
    };
    if database.database_name != COSMOSDB_DATABASE_NAME {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} database name is invalid",
                plan.name
            ),
        );
    }
    let expected = planned_database()
        .containers
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual = database.containers.iter().cloned().collect::<BTreeSet<_>>();
    if expected != actual {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Cosmos DB coordinator {} container set is drifted",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "Cosmos DB coordinator {} containers are verified",
            plan.name
        ),
    )
}

fn create_account_request(plan: &CoordinatorControlPlanePlan) -> CosmosDbCreateCoordinatorAccount {
    CosmosDbCreateCoordinatorAccount {
        account_name: plan.name.clone(),
        regions: planned_regions(plan).into_iter().collect(),
        failover_priority_regions: planned_failover_priority_regions(plan),
        consistency: "Strong".to_owned(),
        write_mode: "single-write-region-with-fenced-failover".to_owned(),
        automatic_failover: true,
        tags: expected_tags(plan),
    }
}

fn planned_database() -> CosmosDbCoordinatorDatabase {
    CosmosDbCoordinatorDatabase {
        database_name: COSMOSDB_DATABASE_NAME.to_owned(),
        containers: vec![
            "repo_epoch".to_owned(),
            "ref_state".to_owned(),
            "push_transaction".to_owned(),
            COSMOSDB_STATE_CONTAINER.to_owned(),
        ],
    }
}

fn planned_regions(plan: &CoordinatorControlPlanePlan) -> BTreeSet<String> {
    std::iter::once(plan.region.clone())
        .chain(plan.failover_regions.iter().cloned())
        .collect()
}

fn planned_failover_priority_regions(plan: &CoordinatorControlPlanePlan) -> Vec<String> {
    let mut regions = vec![plan.region.clone()];
    for region in &plan.failover_regions {
        if !regions.contains(region) {
            regions.push(region.clone());
        }
    }
    regions
}

fn expected_tags(plan: &CoordinatorControlPlanePlan) -> BTreeMap<String, String> {
    BTreeMap::from([
        (TAG_MANAGED.to_owned(), "true".to_owned()),
        (TAG_RESOURCE.to_owned(), "write-coordinator".to_owned()),
        (TAG_COORDINATOR.to_owned(), plan.name.clone()),
    ])
}

fn ownership_tags_match(
    plan: &CoordinatorControlPlanePlan,
    tags: &BTreeMap<String, String>,
) -> bool {
    expected_tags(plan)
        .iter()
        .all(|(key, value)| tags.get(key) == Some(value))
}

/// Azure Resource Manager implementation for Crab-owned Cosmos DB coordinators.
#[cfg(feature = "coordinator-cosmosdb")]
#[derive(Clone)]
pub struct AzureCosmosDbCoordinatorControlPlaneClient {
    credential: Arc<dyn azure_core::auth::TokenCredential>,
    http: reqwest::Client,
    subscription_id: String,
    resource_group_name: String,
    endpoint: String,
}

#[cfg(feature = "coordinator-cosmosdb")]
impl AzureCosmosDbCoordinatorControlPlaneClient {
    pub fn new() -> Result<Self> {
        let subscription_id = std::env::var("AZURE_SUBSCRIPTION_ID").map_err(|_| {
            CoordinationError::Configuration {
                key: "AZURE_SUBSCRIPTION_ID".into(),
                origin: "environment".into(),
            }
        })?;
        let resource_group_name = std::env::var("AZURE_RESOURCE_GROUP").map_err(|_| {
            CoordinationError::Configuration {
                key: "AZURE_RESOURCE_GROUP".into(),
                origin: "environment".into(),
            }
        })?;
        let credential = azure_identity::create_credential().map_err(cosmosdb_auth_error)?;
        Ok(Self {
            credential,
            http: reqwest::Client::new(),
            subscription_id,
            resource_group_name,
            endpoint: "https://management.azure.com".to_owned(),
        })
    }

    fn account_path(&self, account_name: &str) -> String {
        format!(
            "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.DocumentDB/databaseAccounts/{}",
            self.subscription_id, self.resource_group_name, account_name
        )
    }

    fn database_path(&self, account_name: &str, database_name: &str) -> String {
        format!(
            "{}/sqlDatabases/{}",
            self.account_path(account_name),
            database_name
        )
    }

    fn containers_path(&self, account_name: &str, database_name: &str) -> String {
        format!(
            "{}/containers",
            self.database_path(account_name, database_name)
        )
    }

    fn container_path(
        &self,
        account_name: &str,
        database_name: &str,
        container_name: &str,
    ) -> String {
        format!(
            "{}/{}",
            self.containers_path(account_name, database_name),
            container_name
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}?api-version=2024-09-01-preview", self.endpoint, path)
    }

    async fn bearer_token(&self) -> Result<String> {
        self.credential
            .get_token(&["https://management.azure.com/.default"])
            .await
            .map(|token| format!("Bearer {}", token.token.secret()))
            .map_err(cosmosdb_auth_error)
    }

    async fn get_json(&self, path: &str, operation: &str) -> Result<Option<Value>> {
        let response = self
            .http
            .get(self.url(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error(operation, err))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(cosmosdb_status_error(operation, response).await);
        }
        response
            .json::<Value>()
            .await
            .map(Some)
            .map_err(|err| cosmosdb_request_error(operation, err))
    }

    async fn put_json(&self, path: &str, body: Value, operation: &str) -> Result<()> {
        let response = self
            .http
            .put(self.url(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .json(&body)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error(operation, err))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(cosmosdb_status_error(operation, response).await)
    }

    async fn delete_json(&self, path: &str, operation: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.url(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .send()
            .await
            .map_err(|err| cosmosdb_request_error(operation, err))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND || response.status().is_success() {
            return Ok(());
        }
        Err(cosmosdb_status_error(operation, response).await)
    }

    async fn database_state(
        &self,
        account_name: &str,
    ) -> Result<Option<CosmosDbCoordinatorDatabase>> {
        let database_name = planned_database().database_name;
        let Some(database) = self
            .get_json(
                &self.database_path(account_name, &database_name),
                "read Cosmos DB SQL database",
            )
            .await?
        else {
            return Ok(None);
        };
        let database_id = database
            .pointer("/properties/resource/id")
            .and_then(Value::as_str)
            .or_else(|| database.get("name").and_then(Value::as_str))
            .unwrap_or(&database_name)
            .to_owned();
        let containers = self.container_names(account_name, &database_id).await?;
        Ok(Some(CosmosDbCoordinatorDatabase {
            database_name: database_id,
            containers,
        }))
    }

    async fn container_names(
        &self,
        account_name: &str,
        database_name: &str,
    ) -> Result<Vec<String>> {
        let Some(containers) = self
            .get_json(
                &self.containers_path(account_name, database_name),
                "list Cosmos DB SQL containers",
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(containers
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|container| {
                container
                    .pointer("/properties/resource/id")
                    .and_then(Value::as_str)
                    .or_else(|| container.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
            })
            .collect())
    }

    async fn wait_for_account_available(&self, account_name: &str) -> Result<()> {
        for _ in 0..60 {
            if let Some(account) = self
                .get_json(
                    &self.account_path(account_name),
                    "poll Cosmos DB coordinator account",
                )
                .await?
            {
                match account
                    .pointer("/properties/provisioningState")
                    .and_then(Value::as_str)
                {
                    Some("Succeeded") | None => return Ok(()),
                    Some("Failed" | "DeletionFailed") => {
                        return Err(CoordinationError::Configuration {
                            key: "replication.coordinator.cosmosdb".into(),
                            origin: format!(
                                "Cosmos DB coordinator account {account_name} provisioning failed"
                            ),
                        });
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Err(CoordinationError::Configuration {
            key: "replication.coordinator.cosmosdb".into(),
            origin: format!(
                "Cosmos DB coordinator account {account_name} did not become ready before the apply timeout"
            ),
        })
    }

    async fn account_from_json(
        &self,
        account_name: &str,
        account: &Value,
    ) -> Result<CosmosDbCoordinatorAccount> {
        let properties = account.get("properties").unwrap_or(&Value::Null);
        let regions = cosmosdb_regions(properties);
        let mut failover_priority_regions = cosmosdb_failover_priority_regions(properties);
        if failover_priority_regions.is_empty() {
            failover_priority_regions = regions.clone();
        }
        let consistency = properties
            .pointer("/consistencyPolicy/defaultConsistencyLevel")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let multi_region_writes_enabled = properties
            .get("enableMultipleWriteLocations")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let automatic_failover = properties
            .get("enableAutomaticFailover")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let write_mode =
            if consistency == "Strong" && !multi_region_writes_enabled && automatic_failover {
                "single-write-region-with-fenced-failover"
            } else {
                "not-single-write-region-with-fenced-failover"
            }
            .to_owned();
        let account_name = account
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(account_name)
            .to_owned();
        let tags = account
            .get("tags")
            .and_then(Value::as_object)
            .map(|tags| {
                tags.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let database = self.database_state(&account_name).await?;
        Ok(CosmosDbCoordinatorAccount {
            account_name,
            regions,
            failover_priority_regions,
            consistency,
            write_mode,
            multi_region_writes_enabled,
            automatic_failover,
            tags,
            database,
        })
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
#[async_trait]
impl CosmosDbCoordinatorControlPlaneClient for AzureCosmosDbCoordinatorControlPlaneClient {
    async fn describe_account(
        &self,
        account_name: &str,
    ) -> Result<Option<CosmosDbCoordinatorAccount>> {
        let Some(account) = self
            .get_json(
                &self.account_path(account_name),
                "read Cosmos DB coordinator account",
            )
            .await?
        else {
            return Ok(None);
        };
        self.account_from_json(account_name, &account)
            .await
            .map(Some)
    }

    async fn create_account(&self, request: CosmosDbCreateCoordinatorAccount) -> Result<()> {
        self.put_json(
            &self.account_path(&request.account_name),
            cosmosdb_account_body(&request),
            "create Cosmos DB coordinator account",
        )
        .await?;
        self.wait_for_account_available(&request.account_name).await
    }

    async fn create_database_and_containers(
        &self,
        account_name: &str,
        database: CosmosDbCoordinatorDatabase,
    ) -> Result<()> {
        let database_name = database.database_name;
        self.put_json(
            &self.database_path(account_name, &database_name),
            serde_json::json!({
                "properties": {
                    "resource": {
                        "id": database_name.clone(),
                    },
                },
            }),
            "create Cosmos DB coordinator SQL database",
        )
        .await?;

        for container in database.containers {
            self.put_json(
                &self.container_path(account_name, &database_name, &container),
                cosmosdb_container_body(&container),
                "create Cosmos DB coordinator SQL container",
            )
            .await?;
        }
        Ok(())
    }

    async fn delete_account(&self, account_name: &str) -> Result<()> {
        self.delete_json(
            &self.account_path(account_name),
            "delete Cosmos DB coordinator account",
        )
        .await
    }
}

/// Default Azure Cosmos DB backend used by the CLI resolver.
#[cfg(feature = "coordinator-cosmosdb")]
pub struct AzureCosmosDbCoordinatorBackend;

#[cfg(feature = "coordinator-cosmosdb")]
#[async_trait]
impl CoordinatorControlPlaneBackend for AzureCosmosDbCoordinatorBackend {
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::CosmosDb
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        let client = AzureCosmosDbCoordinatorControlPlaneClient::new()?;
        CosmosDbCoordinatorBackend::new(client).apply(plan).await
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        let client = AzureCosmosDbCoordinatorControlPlaneClient::new()?;
        CosmosDbCoordinatorBackend::new(client).status(plan).await
    }

    async fn remove(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        let client = AzureCosmosDbCoordinatorControlPlaneClient::new()?;
        CosmosDbCoordinatorBackend::new(client).remove(plan).await
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_regions(properties: &Value) -> Vec<String> {
    let mut regions = properties
        .get("locations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|location| location.get("locationName").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if regions.is_empty() {
        regions = properties
            .get("failoverPolicies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|policy| policy.get("locationName").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
    }
    regions
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_failover_priority_regions(properties: &Value) -> Vec<String> {
    let mut regions = cosmosdb_ordered_locations(properties, "failoverPolicies");
    if regions.is_empty() {
        regions = cosmosdb_ordered_locations(properties, "locations");
    }
    regions
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_ordered_locations(properties: &Value, field: &str) -> Vec<String> {
    let mut locations = properties
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|location| {
            let name = location.get("locationName").and_then(Value::as_str)?;
            let priority = location
                .get("failoverPriority")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            Some((priority, name.to_owned()))
        })
        .collect::<Vec<_>>();
    locations.sort_by(|(left_priority, left_name), (right_priority, right_name)| {
        left_priority
            .cmp(right_priority)
            .then_with(|| left_name.cmp(right_name))
    });
    locations.into_iter().map(|(_, name)| name).collect()
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_account_body(request: &CosmosDbCreateCoordinatorAccount) -> Value {
    let tags = request
        .tags
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    let locations = request
        .failover_priority_regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            serde_json::json!({
                "locationName": region,
                "failoverPriority": index,
                "isZoneRedundant": false,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "location": request
            .failover_priority_regions
            .first()
            .cloned()
            .unwrap_or_default(),
        "kind": "GlobalDocumentDB",
        "tags": tags,
        "properties": {
            "databaseAccountOfferType": "Standard",
            "consistencyPolicy": {
                "defaultConsistencyLevel": request.consistency,
            },
            "locations": locations,
            "enableAutomaticFailover": request.automatic_failover,
            "enableMultipleWriteLocations": false,
            "disableKeyBasedMetadataWriteAccess": true,
        },
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_container_body(container: &str) -> Value {
    serde_json::json!({
        "properties": {
            "resource": {
                "id": container,
                "partitionKey": {
                    "paths": ["/repo"],
                    "kind": "Hash",
                    "version": 2,
                },
            },
        },
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_namespace_parts(namespace: &str) -> Result<(&str, &str)> {
    let (account_name, database_name) =
        namespace
            .split_once('/')
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.cosmosdb.namespace".into(),
                origin: "Cosmos DB coordinator namespace must be account/database".into(),
            })?;
    if !account_name.trim().is_empty() && !database_name.trim().is_empty() {
        return Ok((account_name, database_name));
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb.namespace".into(),
        origin: "Cosmos DB coordinator namespace must include account and database".into(),
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_docs_path(database_name: &str) -> String {
    format!("dbs/{database_name}/colls/{COSMOSDB_STATE_CONTAINER}/docs")
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_doc_path(database_name: &str, doc_id: &str) -> String {
    format!("{}/{}", cosmosdb_docs_path(database_name), doc_id)
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_data_url(account_name: &str, path: &str) -> String {
    format!("https://{account_name}.documents.azure.com/{path}")
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_state_document_id(repo_key: &str) -> String {
    format!("repo-{}", blake3::hash(repo_key.as_bytes()).to_hex())
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_state_document_body(
    repo_key: &str,
    doc_id: &str,
    version: u64,
    next_state: &CoordinatorRepoState,
) -> Result<Value> {
    let state_json =
        serde_json::to_string(next_state).map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.cosmosdb.state".into(),
            origin: format!("failed to serialize Cosmos DB coordinator state: {err}"),
        })?;
    Ok(serde_json::json!({
        "id": doc_id,
        "repo": repo_key,
        "version": version,
        "state": state_json,
    }))
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_state_document_from_json(
    repo_key: &str,
    value: &Value,
) -> Result<CosmosDbStateDocument> {
    match value.get("repo").and_then(Value::as_str) {
        Some(repo) if repo == repo_key => {}
        Some(repo) => {
            return Err(cosmosdb_state_shape_error(
                repo_key,
                &format!("document belongs to repo {repo}"),
            ));
        }
        None => return Err(cosmosdb_state_shape_error(repo_key, "missing repo")),
    }

    let version = cosmosdb_json_u64(value.get("version"))
        .ok_or_else(|| cosmosdb_state_shape_error(repo_key, "missing or invalid version"))?;
    let state_json = value
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| cosmosdb_state_shape_error(repo_key, "missing state"))?;
    let state = serde_json::from_str::<CoordinatorRepoState>(state_json).map_err(|err| {
        cosmosdb_state_shape_error(repo_key, &format!("invalid serialized state: {err}"))
    })?;
    let etag = value
        .get("_etag")
        .and_then(Value::as_str)
        .or_else(|| value.get("etag").and_then(Value::as_str))
        .filter(|etag| !etag.is_empty())
        .ok_or_else(|| cosmosdb_state_shape_error(repo_key, "missing etag"))?
        .to_owned();
    Ok(CosmosDbStateDocument {
        record: CoordinatorStateRecord { version, state },
        etag,
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_json_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(value)) => value.parse().ok(),
        _ => None,
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_partition_key_header(repo_key: &str) -> String {
    serde_json::json!([repo_key]).to_string()
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_http_date(time: SystemTime) -> Result<String> {
    let duration =
        time.duration_since(UNIX_EPOCH)
            .map_err(|err| CoordinationError::Configuration {
                key: "replication.coordinator.cosmosdb.date".into(),
                origin: format!("system clock is before Unix epoch: {err}"),
            })?;
    let days = duration.as_secs() / 86_400;
    let seconds_of_day = duration.as_secs() % 86_400;
    let days_i64 = i64::try_from(days).map_err(|err| CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb.date".into(),
        origin: format!("Cosmos DB request date is out of range: {err}"),
    })?;
    let (year, month, day) = cosmosdb_civil_from_days(days_i64);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        cosmosdb_weekday_name(days),
        day,
        cosmosdb_month_name(month),
        year,
        hour,
        minute,
        second
    ))
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_weekday_name(days_since_unix_epoch: u64) -> &'static str {
    match (days_since_unix_epoch + 4) % 7 {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        _ => "Sat",
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_month_name(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_state_shape_error(repo_key: &str, detail: &str) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb.state".into(),
        origin: format!("Cosmos DB coordinator state for repo {repo_key} is invalid: {detail}"),
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
async fn cosmosdb_status_error(operation: &str, response: reqwest::Response) -> CoordinationError {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) if !body.trim().is_empty() => body,
        Ok(_) => "empty response body".to_owned(),
        Err(err) => format!("failed to read response body: {err}"),
    };
    CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb".into(),
        origin: format!("Azure Cosmos DB {operation} failed with {status}: {body}"),
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_status_body_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb".into(),
        origin: format!("Azure Cosmos DB {operation} failed with {status}: {body}"),
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_request_error(operation: &str, err: impl fmt::Display) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb".into(),
        origin: format!("Azure Cosmos DB {operation} failed: {err}"),
    }
}

#[cfg(feature = "coordinator-cosmosdb")]
fn cosmosdb_auth_error(err: impl fmt::Display) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.cosmosdb.auth".into(),
        origin: format!("Azure Cosmos DB authentication failed: {err}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;

    use crate::error::CoordinationError;
    use async_trait::async_trait;

    use super::*;
    use crate::write_coordinator::{
        CommitRequest, CoordinatedRefUpdate, CoordinatorRepoState, CoordinatorStateRecord,
        VersionedCoordinatorStateStore, apply_coordinator_control_plane_plan_with_backend,
        commit_uploaded_push_refs, coordinator_control_plane_remove_plan,
        cosmosdb_coordinator_plan, remove_coordinator_control_plane_plan_with_backend,
    };

    #[derive(Default)]
    struct FakeCosmosDbClient {
        account: Mutex<Option<CosmosDbCoordinatorAccount>>,
        created: Mutex<Vec<CosmosDbCreateCoordinatorAccount>>,
        databases: Mutex<Vec<CosmosDbCoordinatorDatabase>>,
        deleted: Mutex<Vec<String>>,
    }

    impl FakeCosmosDbClient {
        fn with_account(account: CosmosDbCoordinatorAccount) -> Self {
            Self {
                account: Mutex::new(Some(account)),
                ..Self::default()
            }
        }
    }

    #[derive(Default)]
    struct FakeStateStore {
        record: Mutex<Option<CoordinatorStateRecord>>,
    }

    impl FakeStateStore {
        fn seed_ref(&self, name: &str, value: &str) {
            let mut state = CoordinatorRepoState::default();
            state.refs.insert(name.to_owned(), value.to_owned());
            self.record
                .lock()
                .unwrap()
                .replace(CoordinatorStateRecord { version: 1, state });
        }
    }

    #[async_trait]
    impl VersionedCoordinatorStateStore for FakeStateStore {
        async fn read_repo_state(
            &self,
            _namespace: &str,
            _repo_key: &str,
        ) -> crate::Result<Option<CoordinatorStateRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }

        async fn compare_and_swap_repo_state(
            &self,
            _namespace: &str,
            _repo_key: &str,
            expected_version: Option<u64>,
            next_state: &CoordinatorRepoState,
        ) -> crate::Result<bool> {
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

    #[async_trait]
    impl CosmosDbCoordinatorControlPlaneClient for FakeCosmosDbClient {
        async fn describe_account(
            &self,
            _account_name: &str,
        ) -> Result<Option<CosmosDbCoordinatorAccount>> {
            Ok(self.account.lock().unwrap().clone())
        }

        async fn create_account(&self, request: CosmosDbCreateCoordinatorAccount) -> Result<()> {
            self.account
                .lock()
                .unwrap()
                .replace(CosmosDbCoordinatorAccount {
                    account_name: request.account_name.clone(),
                    regions: request.regions.clone(),
                    failover_priority_regions: request.failover_priority_regions.clone(),
                    consistency: request.consistency.clone(),
                    write_mode: request.write_mode.clone(),
                    multi_region_writes_enabled: false,
                    automatic_failover: request.automatic_failover,
                    tags: request.tags.clone(),
                    database: None,
                });
            self.created.lock().unwrap().push(request);
            Ok(())
        }

        async fn create_database_and_containers(
            &self,
            _account_name: &str,
            database: CosmosDbCoordinatorDatabase,
        ) -> Result<()> {
            if let Some(account) = self.account.lock().unwrap().as_mut() {
                account.database = Some(database.clone());
            }
            self.databases.lock().unwrap().push(database);
            Ok(())
        }

        async fn delete_account(&self, account_name: &str) -> Result<()> {
            self.account.lock().unwrap().take();
            self.deleted.lock().unwrap().push(account_name.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn cosmosdb_backend_apply_creates_strong_single_write_account() {
        let plan = cosmosdb_coordinator_plan("crab-coordinator", "eastus", &["westus2".to_owned()]);
        let client = FakeCosmosDbClient::default();
        let backend = CosmosDbCoordinatorBackend::new(&client);

        let status = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.created.lock().unwrap().len(), 1);
        assert_eq!(client.databases.lock().unwrap().len(), 1);
        assert_eq!(client.created.lock().unwrap()[0].consistency, "Strong");
        assert_eq!(
            client.created.lock().unwrap()[0].failover_priority_regions,
            vec!["eastus".to_owned(), "westus2".to_owned()]
        );
    }

    #[tokio::test]
    async fn cosmosdb_backend_apply_rejects_multi_region_writes_before_mutation() {
        let plan = cosmosdb_coordinator_plan("crab-coordinator", "eastus", &["westus2".to_owned()]);
        let mut account = verified_account(&plan);
        account.multi_region_writes_enabled = true;
        let client = FakeCosmosDbClient::with_account(account);
        let backend = CosmosDbCoordinatorBackend::new(&client);

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
        assert!(client.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cosmosdb_backend_apply_rejects_failover_order_drift_before_mutation() {
        let plan = cosmosdb_coordinator_plan("crab-coordinator", "eastus", &["westus2".to_owned()]);
        let mut account = verified_account(&plan);
        account.failover_priority_regions = vec!["westus2".to_owned(), "eastus".to_owned()];
        account.database = None;
        let client = FakeCosmosDbClient::with_account(account);
        let backend = CosmosDbCoordinatorBackend::new(&client);

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
        assert!(client.databases.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cosmosdb_backend_remove_requires_owned_verified_account() {
        let apply_plan =
            cosmosdb_coordinator_plan("crab-coordinator", "eastus", &["westus2".to_owned()]);
        let remove_plan = coordinator_control_plane_remove_plan(&apply_plan);
        let client = FakeCosmosDbClient::with_account(verified_account(&apply_plan));
        let backend = CosmosDbCoordinatorBackend::new(&client);

        let status = remove_coordinator_control_plane_plan_with_backend(&remove_plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cosmosdb_write_coordinator_rejects_stale_same_ref() {
        let store = FakeStateStore::default();
        store.seed_ref("refs/heads/main", "a");
        let coordinator =
            CosmosDbWriteCoordinator::new("global", "crab-coordinator", "org/repo", &store);

        let first = commit_uploaded_push_refs(&coordinator, commit_request("op-1", Some("a"), "b"))
            .await
            .unwrap();
        let err = commit_uploaded_push_refs(&coordinator, commit_request("op-2", Some("a"), "c"))
            .await
            .unwrap_err();

        assert_eq!(first.coordinator_epoch, 1);
        assert!(matches!(err, CoordinationError::NonFastForward { .. }));
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn cosmosdb_state_document_body_uses_hashed_id_and_repo_partition() {
        let mut state = CoordinatorRepoState::default();
        state.refs.insert("refs/heads/main".into(), "abc".into());
        let doc_id = cosmosdb_state_document_id("org/repo");

        let body = cosmosdb_state_document_body("org/repo", &doc_id, 7, &state).unwrap();

        assert_eq!(body["id"], doc_id);
        assert_eq!(body["repo"], "org/repo");
        assert_eq!(body["version"], 7);
        assert_eq!(cosmosdb_partition_key_header("org/repo"), "[\"org/repo\"]");
        assert_eq!(doc_id.len(), 69);
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn cosmosdb_state_document_from_json_parses_version_and_etag() {
        let mut state = CoordinatorRepoState::default();
        state.refs.insert("refs/heads/main".into(), "abc".into());
        let state_json = serde_json::to_string(&state).unwrap();
        let value = serde_json::json!({
            "id": cosmosdb_state_document_id("org/repo"),
            "repo": "org/repo",
            "version": "9",
            "state": state_json,
            "_etag": "\"etag-1\"",
        });

        let document = cosmosdb_state_document_from_json("org/repo", &value).unwrap();

        assert_eq!(document.record.version, 9);
        assert_eq!(
            document.record.state.refs.get("refs/heads/main"),
            Some(&"abc".to_owned())
        );
        assert_eq!(document.etag, "\"etag-1\"");
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn cosmosdb_http_date_formats_rfc1123() {
        let date = cosmosdb_http_date(UNIX_EPOCH + std::time::Duration::from_secs(0)).unwrap();

        assert_eq!(date, "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn cosmosdb_failover_priority_regions_sort_provider_policy_order() {
        let properties = serde_json::json!({
            "locations": [
                {"locationName": "eastus", "failoverPriority": 0},
                {"locationName": "westus2", "failoverPriority": 1}
            ],
            "failoverPolicies": [
                {"locationName": "westus2", "failoverPriority": 1},
                {"locationName": "eastus", "failoverPriority": 0}
            ]
        });

        assert_eq!(
            cosmosdb_failover_priority_regions(&properties),
            vec!["eastus".to_owned(), "westus2".to_owned()]
        );
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn cosmosdb_account_body_uses_planned_failover_priority_order() {
        let request = create_account_request(&cosmosdb_coordinator_plan(
            "crab-coordinator",
            "eastus",
            &["westus2".to_owned()],
        ));

        let body = cosmosdb_account_body(&request);

        assert_eq!(body["location"], "eastus");
        assert_eq!(body["properties"]["locations"][0]["locationName"], "eastus");
        assert_eq!(
            body["properties"]["locations"][0]["failoverPriority"],
            serde_json::json!(0)
        );
        assert_eq!(
            body["properties"]["locations"][1]["locationName"],
            "westus2"
        );
        assert_eq!(
            body["properties"]["locations"][1]["failoverPriority"],
            serde_json::json!(1)
        );
    }

    fn verified_account(plan: &CoordinatorControlPlanePlan) -> CosmosDbCoordinatorAccount {
        CosmosDbCoordinatorAccount {
            account_name: plan.name.clone(),
            regions: planned_regions(plan).into_iter().collect(),
            failover_priority_regions: planned_failover_priority_regions(plan),
            consistency: "Strong".to_owned(),
            write_mode: "single-write-region-with-fenced-failover".to_owned(),
            multi_region_writes_enabled: false,
            automatic_failover: true,
            tags: expected_tags(plan),
            database: Some(planned_database()),
        }
    }

    fn commit_request(operation_id: &str, expected: Option<&str>, new: &str) -> CommitRequest {
        CommitRequest {
            operation_id: operation_id.to_owned(),
            writer: "west".to_owned(),
            region: "westus2".to_owned(),
            manifest_generation: 2,
            refs: vec![CoordinatedRefUpdate {
                name: "refs/heads/main".to_owned(),
                expected: expected.map(str::to_owned),
                new: Some(new.to_owned()),
                force: false,
            }],
            uploaded_objects: vec!["packs/pack-1".to_owned()],
            target_regions: vec!["eastus".to_owned()],
        }
    }
}
