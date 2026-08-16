//! Cloud Spanner coordinator control-plane backend.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "coordinator-spanner")]
use std::sync::Arc;
#[cfg(feature = "coordinator-spanner")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
#[cfg(feature = "coordinator-spanner")]
use serde_json::Value;

use crate::error::{CoordinationError, Result};
#[cfg(feature = "coordinator-spanner")]
use crate::write_coordinator::coordination_error;
use crate::write_coordinator::{
    CommitOutcome, CommitRequest, CoordinatorApplyStatus, CoordinatorCheckState,
    CoordinatorControlPlaneBackend, CoordinatorControlPlanePlan, CoordinatorControlPlaneRequest,
    CoordinatorControlPlaneStatus, CoordinatorFenceOutcome, CoordinatorGcSafetySnapshot,
    CoordinatorHealth, CoordinatorRepairSnapshot, ManagedCoordinatorProvider, PushTransactionState,
    VersionedCoordinatorStateStore, VersionedStateWriteCoordinator, WriteCoordinator,
    coordinator_control_plane_check,
};
#[cfg(feature = "coordinator-spanner")]
use crate::write_coordinator::{CoordinatorRepoState, CoordinatorStateRecord};

const LABEL_MANAGED: &str = "crab-managed";
const LABEL_RESOURCE: &str = "crab-resource";
const LABEL_COORDINATOR: &str = "crab-coordinator";
pub const SPANNER_DATABASE_ID: &str = "crab_coordinator";
const SPANNER_STATE_TABLE: &str = "RepoState";

/// Existing Spanner coordinator instance and database state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannerCoordinatorInstance {
    pub instance_id: String,
    pub instance_config_id: String,
    pub regions: Vec<String>,
    pub edition: String,
    pub external_consistency: bool,
    pub serializable_transactions: bool,
    pub strong_reads: bool,
    pub labels: BTreeMap<String, String>,
    pub database: Option<SpannerCoordinatorDatabase>,
}

/// Existing Spanner coordinator database state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannerCoordinatorDatabase {
    pub database_id: String,
    pub tables: Vec<String>,
}

/// Create request for a Spanner coordinator instance/database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannerCreateCoordinator {
    pub instance_id: String,
    pub instance_config_id: String,
    pub regions: Vec<String>,
    pub edition: String,
    pub database_id: String,
    pub tables: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

/// Minimal Spanner control-plane client needed by Crab-owned coordinator setup.
#[async_trait]
pub trait SpannerCoordinatorControlPlaneClient {
    async fn describe_instance(
        &self,
        instance_id: &str,
    ) -> Result<Option<SpannerCoordinatorInstance>>;
    async fn create_instance(&self, request: SpannerCreateCoordinator) -> Result<()>;
    async fn create_database(
        &self,
        instance_id: &str,
        database: SpannerCoordinatorDatabase,
    ) -> Result<()>;
    async fn delete_instance(&self, instance_id: &str) -> Result<()>;
}

#[async_trait]
impl<T> SpannerCoordinatorControlPlaneClient for &T
where
    T: SpannerCoordinatorControlPlaneClient + Send + Sync + ?Sized,
{
    async fn describe_instance(
        &self,
        instance_id: &str,
    ) -> Result<Option<SpannerCoordinatorInstance>> {
        (*self).describe_instance(instance_id).await
    }

    async fn create_instance(&self, request: SpannerCreateCoordinator) -> Result<()> {
        (*self).create_instance(request).await
    }

    async fn create_database(
        &self,
        instance_id: &str,
        database: SpannerCoordinatorDatabase,
    ) -> Result<()> {
        (*self).create_database(instance_id, database).await
    }

    async fn delete_instance(&self, instance_id: &str) -> Result<()> {
        (*self).delete_instance(instance_id).await
    }
}

/// Spanner data-plane client for one externally consistent repo state record.
pub trait SpannerWriteCoordinatorClient: VersionedCoordinatorStateStore {}

impl<T> SpannerWriteCoordinatorClient for T where T: VersionedCoordinatorStateStore {}

/// Spanner implementation of the active-active write coordinator data plane.
pub struct SpannerWriteCoordinator<C> {
    inner: VersionedStateWriteCoordinator<C>,
}

impl<C> SpannerWriteCoordinator<C> {
    #[must_use]
    pub fn new(
        instance_id: impl Into<String>,
        database_id: impl Into<String>,
        repo_key: impl Into<String>,
        client: C,
    ) -> Self {
        let namespace = format!("{}/{}", instance_id.into(), database_id.into());
        Self {
            inner: VersionedStateWriteCoordinator::new("spanner", namespace, repo_key, client),
        }
    }
}

#[async_trait]
impl<C> WriteCoordinator for SpannerWriteCoordinator<C>
where
    C: SpannerWriteCoordinatorClient,
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

/// Spanner implementation of the managed coordinator control-plane contract.
pub struct SpannerCoordinatorBackend<C> {
    client: C,
}

impl<C> SpannerCoordinatorBackend<C> {
    #[must_use]
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C> CoordinatorControlPlaneBackend for SpannerCoordinatorBackend<C>
where
    C: SpannerCoordinatorControlPlaneClient + Send + Sync,
{
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::Spanner
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        ensure_spanner_plan(plan)?;
        let mut actions = Vec::new();
        match self.client.describe_instance(&plan.name).await? {
            None => {
                self.client
                    .create_instance(create_spanner_request(plan))
                    .await?;
                self.client
                    .create_database(&plan.name, planned_database())
                    .await?;
                actions.push("create-instance".to_owned());
                actions.push("create-database".to_owned());
            }
            Some(instance) if instance.database.is_none() => {
                self.client
                    .create_database(&plan.name, planned_database())
                    .await?;
                actions.push("create-database".to_owned());
            }
            Some(_) => {}
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::Spanner,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("Spanner coordinator {} is applied", plan.name),
        })
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        ensure_spanner_plan(plan)?;
        let instance = self.client.describe_instance(&plan.name).await?;
        let checks = plan
            .requests
            .iter()
            .map(|request| spanner_check(plan, request, instance.as_ref()))
            .collect();

        Ok(CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::Spanner,
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
        ensure_spanner_plan(plan)?;
        let mut actions = Vec::new();
        if self.client.describe_instance(&plan.name).await?.is_some() {
            self.client.delete_instance(&plan.name).await?;
            actions.push("remove:create-instance".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::Spanner,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("Spanner coordinator {} is removed", plan.name),
        })
    }
}

#[cfg(feature = "coordinator-spanner")]
#[derive(Clone)]
pub struct GoogleSpannerCoordinatorControlPlaneClient {
    http: reqwest::Client,
    token_source: Arc<dyn google_cloud_token::TokenSource>,
    project_id: String,
    base_url: String,
}

#[cfg(feature = "coordinator-spanner")]
#[derive(Clone)]
pub struct GoogleSpannerWriteCoordinatorClient {
    http: reqwest::Client,
    token_source: Arc<dyn google_cloud_token::TokenSource>,
    project_id: String,
    base_url: String,
}

#[cfg(feature = "coordinator-spanner")]
impl GoogleSpannerWriteCoordinatorClient {
    pub async fn new() -> Result<Self> {
        let config = google_cloud_storage::client::ClientConfig::default()
            .with_auth()
            .await
            .map_err(spanner_auth_error)?;
        let project_id =
            config
                .project_id
                .clone()
                .ok_or_else(|| CoordinationError::Configuration {
                    key: "replication.coordinator.spanner.project".into(),
                    origin: "Google Cloud authentication did not resolve a project ID for Spanner"
                        .into(),
                })?;
        let token_source = config
            .token_source_provider
            .as_ref()
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.spanner.auth".into(),
                origin: "Google Cloud authentication did not provide a token source".into(),
            })?
            .token_source();
        Ok(Self {
            http: reqwest::Client::new(),
            token_source,
            project_id,
            base_url: "https://spanner.googleapis.com/v1".to_owned(),
        })
    }

    async fn create_session(&self, database_path: &str) -> Result<String> {
        let body = serde_json::json!({ "session": {} });
        let session = self
            .post_json(
                &format!("{database_path}/sessions"),
                &body,
                "create Spanner coordinator session",
            )
            .await?;
        spanner_json_str(&session, &["name"])
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.spanner.session".into(),
                origin: "Spanner session creation response did not include a session name".into(),
            })
    }

    async fn delete_session(&self, session_path: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.endpoint(session_path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .send()
            .await
            .map_err(|err| spanner_request_error("delete Spanner coordinator session", err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| spanner_request_error("delete Spanner coordinator session", err))?;
        if status == reqwest::StatusCode::NOT_FOUND || status.is_success() {
            return Ok(());
        }
        Err(spanner_status_error(
            "delete Spanner coordinator session",
            status,
            &body,
        ))
    }

    async fn begin_read_write_transaction(&self, session_path: &str) -> Result<String> {
        let body = serde_json::json!({ "options": { "readWrite": {} } });
        let transaction = self
            .post_json(
                &format!("{session_path}:beginTransaction"),
                &body,
                "begin Spanner coordinator transaction",
            )
            .await?;
        spanner_json_str(&transaction, &["id"])
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.spanner.transaction".into(),
                origin: "Spanner beginTransaction response did not include a transaction ID".into(),
            })
    }

    async fn execute_repo_state_query(
        &self,
        session_path: &str,
        repo_key: &str,
        transaction_id: Option<&str>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "sql": format!("SELECT Version, State FROM {SPANNER_STATE_TABLE} WHERE Repo = @repo"),
            "params": { "repo": repo_key },
            "paramTypes": { "repo": { "code": "STRING" } },
        });
        if let Some(transaction_id) = transaction_id {
            body["transaction"] = serde_json::json!({ "id": transaction_id });
        }
        self.post_json(
            &format!("{session_path}:executeSql"),
            &body,
            "query Spanner coordinator state",
        )
        .await
    }

    async fn commit_repo_state(
        &self,
        session_path: &str,
        transaction_id: &str,
        repo_key: &str,
        version: u64,
        next_state: &CoordinatorRepoState,
    ) -> Result<bool> {
        let state_json =
            serde_json::to_string(next_state).map_err(|err| CoordinationError::Configuration {
                key: "replication.coordinator.spanner".into(),
                origin: format!("failed to serialize Spanner coordinator state: {err}"),
            })?;
        let body = spanner_repo_state_commit_body(
            transaction_id,
            repo_key,
            version,
            &state_json,
            spanner_now_ms()?,
        );
        let response = self
            .http
            .post(self.endpoint(&format!("{session_path}:commit")))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .json(&body)
            .send()
            .await
            .map_err(|err| spanner_request_error("commit Spanner coordinator state", err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| spanner_request_error("commit Spanner coordinator state", err))?;
        if status.is_success() {
            return Ok(true);
        }
        if spanner_is_aborted_response(status, &body) {
            return Ok(false);
        }
        Err(spanner_status_error(
            "commit Spanner coordinator state",
            status,
            &body,
        ))
    }

    async fn rollback_transaction(&self, session_path: &str, transaction_id: &str) -> Result<()> {
        let body = serde_json::json!({ "transactionId": transaction_id });
        self.post_json(
            &format!("{session_path}:rollback"),
            &body,
            "rollback Spanner coordinator transaction",
        )
        .await?;
        Ok(())
    }

    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        operation: &str,
    ) -> Result<serde_json::Value> {
        let response = self
            .http
            .post(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .json(body)
            .send()
            .await
            .map_err(|err| spanner_request_error(operation, err))?;
        spanner_response_json(response, operation).await
    }

    async fn bearer_token(&self) -> Result<String> {
        self.token_source
            .token()
            .await
            .map_err(|err| spanner_auth_error(format!("Spanner token failed: {err}")))
    }

    fn database_path(&self, instance_id: &str, database_id: &str) -> String {
        format!(
            "projects/{}/instances/{instance_id}/databases/{database_id}",
            self.project_id
        )
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

#[cfg(feature = "coordinator-spanner")]
#[async_trait]
impl VersionedCoordinatorStateStore for GoogleSpannerWriteCoordinatorClient {
    async fn read_repo_state(
        &self,
        namespace: &str,
        repo_key: &str,
    ) -> crate::Result<Option<CoordinatorStateRecord>> {
        async {
            let (instance_id, database_id) = spanner_namespace_parts(namespace)?;
            let database_path = self.database_path(instance_id, database_id);
            let session = self.create_session(&database_path).await?;
            let result = self
                .execute_repo_state_query(&session, repo_key, None)
                .await
                .and_then(|result| spanner_repo_state_from_result_set(repo_key, &result));
            let _ = self.delete_session(&session).await;
            result
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
            let (instance_id, database_id) = spanner_namespace_parts(namespace)?;
            let database_path = self.database_path(instance_id, database_id);
            let session = self.create_session(&database_path).await?;
            let transaction_id = match self.begin_read_write_transaction(&session).await {
                Ok(transaction_id) => transaction_id,
                Err(err) => {
                    let _ = self.delete_session(&session).await;
                    return Err(err);
                }
            };

            let current = self
                .execute_repo_state_query(&session, repo_key, Some(&transaction_id))
                .await
                .and_then(|result| spanner_repo_state_from_result_set(repo_key, &result));
            let current = match current {
                Ok(current) => current,
                Err(err) => {
                    let _ = self.rollback_transaction(&session, &transaction_id).await;
                    let _ = self.delete_session(&session).await;
                    return Err(err);
                }
            };
            let current_version = current.as_ref().map(|record| record.version);
            if current_version != expected_version {
                let _ = self.rollback_transaction(&session, &transaction_id).await;
                let _ = self.delete_session(&session).await;
                return Ok(false);
            }

            let next_version = expected_version.unwrap_or_default().saturating_add(1);
            let result = self
                .commit_repo_state(
                    &session,
                    &transaction_id,
                    repo_key,
                    next_version,
                    next_state,
                )
                .await;
            if matches!(result, Ok(false) | Err(_)) {
                let _ = self.rollback_transaction(&session, &transaction_id).await;
            }
            let _ = self.delete_session(&session).await;
            result
        }
        .await
        .map_err(coordination_error)
    }
}

#[cfg(feature = "coordinator-spanner")]
impl GoogleSpannerCoordinatorControlPlaneClient {
    pub async fn new() -> Result<Self> {
        let config = google_cloud_storage::client::ClientConfig::default()
            .with_auth()
            .await
            .map_err(spanner_auth_error)?;
        let project_id =
            config
                .project_id
                .clone()
                .ok_or_else(|| CoordinationError::Configuration {
                    key: "replication.coordinator.spanner.project".into(),
                    origin: "Google Cloud authentication did not resolve a project ID for Spanner"
                        .into(),
                })?;
        let token_source = config
            .token_source_provider
            .as_ref()
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.spanner.auth".into(),
                origin: "Google Cloud authentication did not provide a token source".into(),
            })?
            .token_source();
        Ok(Self {
            http: reqwest::Client::new(),
            token_source,
            project_id,
            base_url: "https://spanner.googleapis.com/v1".to_owned(),
        })
    }

    async fn read_database(
        &self,
        instance_id: &str,
        database_id: &str,
    ) -> Result<Option<SpannerCoordinatorDatabase>> {
        let database_path = self.database_path(instance_id, database_id);
        if self
            .get_optional_json(&database_path, "read Spanner coordinator database")
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let ddl = self
            .get_optional_json(
                &format!("{database_path}/ddl"),
                "read Spanner coordinator database DDL",
            )
            .await?;
        let tables = ddl
            .as_ref()
            .map(spanner_tables_from_ddl)
            .unwrap_or_default();
        Ok(Some(SpannerCoordinatorDatabase {
            database_id: database_id.to_owned(),
            tables,
        }))
    }

    async fn wait_for_instance_ready(&self, instance_id: &str) -> Result<()> {
        for _ in 0..60 {
            if let Some(instance) = self
                .get_optional_json(
                    &self.instance_path(instance_id),
                    "poll Spanner coordinator instance",
                )
                .await?
            {
                match spanner_json_str(&instance, &["state"]) {
                    Some("READY") => return Ok(()),
                    Some("FAILED") => {
                        return Err(CoordinationError::Configuration {
                            key: "replication.coordinator.spanner.instance".into(),
                            origin: format!(
                                "Spanner coordinator instance {instance_id} entered FAILED state"
                            ),
                        });
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Err(CoordinationError::Configuration {
            key: "replication.coordinator.spanner.instance".into(),
            origin: format!("timed out waiting for Spanner coordinator instance {instance_id}"),
        })
    }

    async fn wait_for_database_schema(&self, instance_id: &str, database_id: &str) -> Result<()> {
        let expected = planned_database()
            .tables
            .into_iter()
            .collect::<BTreeSet<_>>();
        for _ in 0..60 {
            if let Some(database) = self.read_database(instance_id, database_id).await?
                && database.tables.iter().cloned().collect::<BTreeSet<_>>() == expected
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Err(CoordinationError::Configuration {
            key: "replication.coordinator.spanner.database".into(),
            origin: format!(
                "timed out waiting for Spanner coordinator database {database_id} schema"
            ),
        })
    }

    async fn get_optional_json(
        &self,
        path: &str,
        operation: &str,
    ) -> Result<Option<serde_json::Value>> {
        let response = self
            .http
            .get(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .send()
            .await
            .map_err(|err| spanner_request_error(operation, err))?;
        spanner_optional_response_json(response, operation).await
    }

    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        operation: &str,
    ) -> Result<serde_json::Value> {
        let response = self
            .http
            .post(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .json(body)
            .send()
            .await
            .map_err(|err| spanner_request_error(operation, err))?;
        spanner_response_json(response, operation).await
    }

    async fn delete_json(&self, path: &str, operation: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.bearer_token().await?)
            .send()
            .await
            .map_err(|err| spanner_request_error(operation, err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| spanner_request_error(operation, err))?;
        if status == reqwest::StatusCode::NOT_FOUND || status.is_success() {
            return Ok(());
        }
        Err(spanner_status_error(operation, status, &body))
    }

    async fn bearer_token(&self) -> Result<String> {
        self.token_source
            .token()
            .await
            .map_err(|err| spanner_auth_error(format!("Spanner token failed: {err}")))
    }

    fn instance_path(&self, instance_id: &str) -> String {
        format!("projects/{}/instances/{instance_id}", self.project_id)
    }

    fn database_path(&self, instance_id: &str, database_id: &str) -> String {
        format!(
            "projects/{}/instances/{instance_id}/databases/{database_id}",
            self.project_id
        )
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

#[cfg(feature = "coordinator-spanner")]
#[async_trait]
impl SpannerCoordinatorControlPlaneClient for GoogleSpannerCoordinatorControlPlaneClient {
    async fn describe_instance(
        &self,
        instance_id: &str,
    ) -> Result<Option<SpannerCoordinatorInstance>> {
        let Some(instance) = self
            .get_optional_json(
                &self.instance_path(instance_id),
                "read Spanner coordinator instance",
            )
            .await?
        else {
            return Ok(None);
        };
        let config = match spanner_json_str(&instance, &["config"]) {
            Some(config_path) => {
                self.get_optional_json(config_path, "read Spanner coordinator instance config")
                    .await?
            }
            None => None,
        };
        let database = self.read_database(instance_id, SPANNER_DATABASE_ID).await?;
        Ok(Some(spanner_instance_from_json(
            &instance,
            config.as_ref(),
            database,
        )?))
    }

    async fn create_instance(&self, request: SpannerCreateCoordinator) -> Result<()> {
        let instance_id = request.instance_id.clone();
        let body = serde_json::json!({
            "instanceId": instance_id,
            "instance": {
                "config": format!(
                    "projects/{}/instanceConfigs/{}",
                    self.project_id, request.instance_config_id
                ),
                "displayName": request.instance_id,
                "processingUnits": 100,
                "labels": request.labels,
                "edition": request.edition,
            },
        });
        self.post_json(
            &format!("projects/{}/instances", self.project_id),
            &body,
            "create Spanner coordinator instance",
        )
        .await?;
        self.wait_for_instance_ready(&instance_id).await
    }

    async fn create_database(
        &self,
        instance_id: &str,
        database: SpannerCoordinatorDatabase,
    ) -> Result<()> {
        let body = serde_json::json!({
            "createStatement": format!("CREATE DATABASE `{}`", database.database_id),
            "extraStatements": spanner_database_ddl_statements(),
            "databaseDialect": "GOOGLE_STANDARD_SQL",
        });
        self.post_json(
            &format!(
                "projects/{}/instances/{instance_id}/databases",
                self.project_id
            ),
            &body,
            "create Spanner coordinator database",
        )
        .await?;
        self.wait_for_database_schema(instance_id, &database.database_id)
            .await
    }

    async fn delete_instance(&self, instance_id: &str) -> Result<()> {
        self.delete_json(
            &self.instance_path(instance_id),
            "delete Spanner coordinator instance",
        )
        .await
    }
}

#[cfg(feature = "coordinator-spanner")]
pub struct GoogleSpannerCoordinatorBackend;

#[cfg(feature = "coordinator-spanner")]
#[async_trait]
impl CoordinatorControlPlaneBackend for GoogleSpannerCoordinatorBackend {
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::Spanner
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        let client = GoogleSpannerCoordinatorControlPlaneClient::new().await?;
        SpannerCoordinatorBackend::new(client).apply(plan).await
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        let client = GoogleSpannerCoordinatorControlPlaneClient::new().await?;
        SpannerCoordinatorBackend::new(client).status(plan).await
    }

    async fn remove(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        let client = GoogleSpannerCoordinatorControlPlaneClient::new().await?;
        SpannerCoordinatorBackend::new(client).remove(plan).await
    }
}

fn ensure_spanner_plan(plan: &CoordinatorControlPlanePlan) -> Result<()> {
    if plan.provider == ManagedCoordinatorProvider::Spanner {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "Spanner coordinator backend cannot manage {} coordinator plan",
            plan.provider.as_str()
        ),
    })
}

fn spanner_check(
    plan: &CoordinatorControlPlanePlan,
    request: &CoordinatorControlPlaneRequest,
    instance: Option<&SpannerCoordinatorInstance>,
) -> crate::write_coordinator::CoordinatorControlPlaneCheck {
    let action = request
        .action
        .strip_prefix("remove:")
        .unwrap_or(request.action.as_str());
    let (state, message) = match action {
        "create-instance" => instance_state(plan, instance),
        "validate-linearizable-contract" => linearizable_state(plan, instance),
        "create-database" => database_state(plan, instance),
        _ => (
            CoordinatorCheckState::Unsupported,
            format!(
                "Spanner coordinator action {} is unsupported",
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

fn instance_state(
    plan: &CoordinatorControlPlanePlan,
    instance: Option<&SpannerCoordinatorInstance>,
) -> (CoordinatorCheckState, String) {
    let Some(instance) = instance else {
        return (
            CoordinatorCheckState::Missing,
            format!("Spanner coordinator instance {} is missing", plan.name),
        );
    };
    if instance.instance_id != plan.name {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner instance {} does not match planned coordinator {}",
                instance.instance_id, plan.name
            ),
        );
    }
    if instance.instance_config_id != planned_instance_config_id(plan) {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner coordinator {} instance config ID does not match the plan",
                plan.name
            ),
        );
    }
    if planned_regions(plan) != instance.regions.iter().cloned().collect::<BTreeSet<_>>() {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner coordinator {} regions do not match the plan",
                plan.name
            ),
        );
    }
    if instance.edition != "ENTERPRISE_PLUS" {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner coordinator {} must use ENTERPRISE_PLUS edition",
                plan.name
            ),
        );
    }
    if !ownership_labels_match(plan, &instance.labels) {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner coordinator {} ownership labels are invalid",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "Spanner coordinator {} instance matches the plan",
            plan.name
        ),
    )
}

fn linearizable_state(
    plan: &CoordinatorControlPlanePlan,
    instance: Option<&SpannerCoordinatorInstance>,
) -> (CoordinatorCheckState, String) {
    let Some(instance) = instance else {
        return (
            CoordinatorCheckState::Verified,
            format!(
                "Spanner coordinator {} planned instance satisfies the external-consistency transaction contract",
                plan.name
            ),
        );
    };
    if !instance.external_consistency
        || !instance.serializable_transactions
        || !instance.strong_reads
    {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "Spanner coordinator {} must use external consistency, serializable transactions, and strong reads",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "Spanner coordinator {} satisfies the external-consistency transaction contract",
            plan.name
        ),
    )
}

fn database_state(
    plan: &CoordinatorControlPlanePlan,
    instance: Option<&SpannerCoordinatorInstance>,
) -> (CoordinatorCheckState, String) {
    let Some(instance) = instance else {
        return (
            CoordinatorCheckState::Missing,
            format!("Spanner coordinator {} database is missing", plan.name),
        );
    };
    let Some(database) = instance.database.as_ref() else {
        return (
            CoordinatorCheckState::Missing,
            format!("Spanner coordinator {} database is missing", plan.name),
        );
    };
    if database.database_id != SPANNER_DATABASE_ID {
        return (
            CoordinatorCheckState::Drifted,
            format!("Spanner coordinator {} database ID is invalid", plan.name),
        );
    }
    let expected_tables = planned_database()
        .tables
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual_tables = database.tables.iter().cloned().collect::<BTreeSet<_>>();
    if expected_tables != actual_tables {
        return (
            CoordinatorCheckState::Drifted,
            format!("Spanner coordinator {} schema is drifted", plan.name),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!("Spanner coordinator {} schema is verified", plan.name),
    )
}

fn create_spanner_request(plan: &CoordinatorControlPlanePlan) -> SpannerCreateCoordinator {
    let database = planned_database();
    SpannerCreateCoordinator {
        instance_id: plan.name.clone(),
        instance_config_id: planned_instance_config_id(plan),
        regions: planned_region_list(plan),
        edition: "ENTERPRISE_PLUS".to_owned(),
        database_id: database.database_id,
        tables: database.tables,
        labels: expected_labels(plan),
    }
}

fn planned_database() -> SpannerCoordinatorDatabase {
    SpannerCoordinatorDatabase {
        database_id: SPANNER_DATABASE_ID.to_owned(),
        tables: vec![
            "RepoEpoch".to_owned(),
            "RefState".to_owned(),
            "PushTransaction".to_owned(),
            SPANNER_STATE_TABLE.to_owned(),
        ],
    }
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_database_ddl_statements() -> Vec<String> {
    vec![
        "CREATE TABLE RepoEpoch (Repo STRING(MAX) NOT NULL, Epoch INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo)".to_owned(),
        "CREATE TABLE RefState (Repo STRING(MAX) NOT NULL, Ref STRING(MAX) NOT NULL, Oid STRING(MAX), Epoch INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo, Ref)".to_owned(),
        "CREATE TABLE PushTransaction (Repo STRING(MAX) NOT NULL, OperationId STRING(MAX) NOT NULL, State STRING(MAX) NOT NULL, Writer STRING(MAX) NOT NULL, Region STRING(MAX) NOT NULL, ManifestGeneration INT64 NOT NULL, UpdatedAt TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (Repo, OperationId)".to_owned(),
        format!("CREATE TABLE {SPANNER_STATE_TABLE} (Repo STRING(MAX) NOT NULL, Version INT64 NOT NULL, State STRING(MAX) NOT NULL, UpdatedAtMs INT64 NOT NULL) PRIMARY KEY (Repo)"),
    ]
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_namespace_parts(namespace: &str) -> Result<(&str, &str)> {
    let (instance_id, database_id) =
        namespace
            .split_once('/')
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator.spanner.namespace".into(),
                origin: "Spanner coordinator namespace must be instance/database".into(),
            })?;
    if !instance_id.trim().is_empty() && !database_id.trim().is_empty() {
        return Ok((instance_id, database_id));
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator.spanner.namespace".into(),
        origin: "Spanner coordinator namespace must include instance and database".into(),
    })
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_repo_state_from_result_set(
    repo_key: &str,
    result: &Value,
) -> Result<Option<CoordinatorStateRecord>> {
    let Some(rows) = result.get("rows").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(row) = rows.first().and_then(Value::as_array) else {
        return Ok(None);
    };
    let version = row
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| spanner_state_shape_error(repo_key, "missing version"))?
        .parse::<u64>()
        .map_err(|err| spanner_state_shape_error(repo_key, &format!("invalid version: {err}")))?;
    let state_json = row
        .get(1)
        .and_then(Value::as_str)
        .ok_or_else(|| spanner_state_shape_error(repo_key, "missing state"))?;
    let state = serde_json::from_str::<CoordinatorRepoState>(state_json).map_err(|err| {
        spanner_state_shape_error(repo_key, &format!("invalid serialized state: {err}"))
    })?;
    Ok(Some(CoordinatorStateRecord { version, state }))
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_repo_state_commit_body(
    transaction_id: &str,
    repo_key: &str,
    version: u64,
    state_json: &str,
    updated_at_ms: u128,
) -> Value {
    serde_json::json!({
        "transactionId": transaction_id,
        "mutations": [
            {
                "insertOrUpdate": {
                    "table": SPANNER_STATE_TABLE,
                    "columns": ["Repo", "Version", "State", "UpdatedAtMs"],
                    "values": [[
                        repo_key,
                        version.to_string(),
                        state_json,
                        updated_at_ms.to_string(),
                    ]]
                }
            }
        ]
    })
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_now_ms() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.spanner.time".into(),
            origin: format!("system clock is before Unix epoch: {err}"),
        })
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_is_aborted_response(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::CONFLICT
        || body.contains("\"ABORTED\"")
        || body.contains("\"code\":10")
        || body.contains("\"code\": 10")
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_state_shape_error(repo_key: &str, detail: &str) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.spanner.state".into(),
        origin: format!("Spanner coordinator state for repo {repo_key} is invalid: {detail}"),
    }
}

fn planned_regions(plan: &CoordinatorControlPlanePlan) -> BTreeSet<String> {
    std::iter::once(plan.region.clone())
        .chain(plan.failover_regions.iter().cloned())
        .collect()
}

fn planned_instance_config_id(plan: &CoordinatorControlPlanePlan) -> String {
    plan.region.clone()
}

fn planned_region_list(plan: &CoordinatorControlPlanePlan) -> Vec<String> {
    let mut regions = vec![plan.region.clone()];
    for region in &plan.failover_regions {
        if !regions.contains(region) {
            regions.push(region.clone());
        }
    }
    regions
}

fn expected_labels(plan: &CoordinatorControlPlanePlan) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_MANAGED.to_owned(), "true".to_owned()),
        (LABEL_RESOURCE.to_owned(), "write-coordinator".to_owned()),
        (LABEL_COORDINATOR.to_owned(), plan.name.clone()),
    ])
}

fn ownership_labels_match(
    plan: &CoordinatorControlPlanePlan,
    labels: &BTreeMap<String, String>,
) -> bool {
    expected_labels(plan)
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_instance_from_json(
    instance: &Value,
    config: Option<&Value>,
    database: Option<SpannerCoordinatorDatabase>,
) -> Result<SpannerCoordinatorInstance> {
    let name =
        spanner_json_str(instance, &["name"]).ok_or_else(|| CoordinationError::Configuration {
            key: "replication.coordinator.spanner.instance".into(),
            origin: "Spanner instance response did not include a name".into(),
        })?;
    let mut regions = BTreeSet::new();
    let mut instance_config_id = String::new();
    if let Some(config_path) = spanner_json_str(instance, &["config"]) {
        let config_id = spanner_last_segment(config_path);
        if !config_id.is_empty() {
            instance_config_id = config_id.to_owned();
            regions.insert(config_id.to_owned());
        }
    }
    if let Some(config) = config {
        regions.extend(spanner_instance_config_locations(config));
    }
    let ready = spanner_json_str(instance, &["state"]) == Some("READY");
    Ok(SpannerCoordinatorInstance {
        instance_id: spanner_last_segment(name).to_owned(),
        instance_config_id,
        regions: regions.into_iter().collect(),
        edition: spanner_json_str(instance, &["edition"])
            .unwrap_or_default()
            .to_owned(),
        external_consistency: ready,
        serializable_transactions: ready,
        strong_reads: ready,
        labels: spanner_json_string_map(instance, "labels"),
        database,
    })
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_instance_config_locations(config: &Value) -> BTreeSet<String> {
    config
        .get("replicas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|replica| spanner_json_str(replica, &["location"]))
        .map(str::to_owned)
        .collect()
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_tables_from_ddl(ddl: &Value) -> Vec<String> {
    ddl.get("statements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(spanner_table_name_from_ddl)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_table_name_from_ddl(statement: &str) -> Option<String> {
    let statement = statement.trim_start();
    let rest = statement.strip_prefix("CREATE TABLE ")?;
    let table = rest.split_whitespace().next()?.trim_matches('`');
    if table.is_empty() {
        None
    } else {
        Some(table.to_owned())
    }
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_json_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_json_string_map(value: &Value, key: &str) -> BTreeMap<String, String> {
    value
        .get(key)
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_last_segment(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

#[cfg(feature = "coordinator-spanner")]
async fn spanner_response_json(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| spanner_request_error(operation, err))?;
    if !status.is_success() {
        return Err(spanner_status_error(operation, status, &body));
    }
    serde_json::from_str(&body).map_err(|err| CoordinationError::Configuration {
        key: "replication.coordinator.spanner".into(),
        origin: format!("Spanner {operation} returned invalid JSON: {err}"),
    })
}

#[cfg(feature = "coordinator-spanner")]
async fn spanner_optional_response_json(
    response: reqwest::Response,
    operation: &str,
) -> Result<Option<serde_json::Value>> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| spanner_request_error(operation, err))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(spanner_status_error(operation, status, &body));
    }
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.spanner".into(),
            origin: format!("Spanner {operation} returned invalid JSON: {err}"),
        })
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_status_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.spanner".into(),
        origin: format!(
            "Spanner {operation} failed with HTTP {status}: {}",
            spanner_error_excerpt(body)
        ),
    }
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_request_error(operation: &str, err: impl std::fmt::Display) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.spanner".into(),
        origin: format!("Spanner {operation} request failed: {err}"),
    }
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_auth_error(err: impl std::fmt::Display) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.spanner.auth".into(),
        origin: format!("Spanner authentication failed: {err}"),
    }
}

#[cfg(feature = "coordinator-spanner")]
fn spanner_error_excerpt(body: &str) -> String {
    let excerpt = body.chars().take(512).collect::<String>();
    if body.chars().count() > 512 {
        format!("{excerpt}...")
    } else {
        excerpt
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
        remove_coordinator_control_plane_plan_with_backend, spanner_coordinator_plan,
    };

    #[derive(Default)]
    struct FakeSpannerClient {
        instance: Mutex<Option<SpannerCoordinatorInstance>>,
        created: Mutex<Vec<SpannerCreateCoordinator>>,
        databases: Mutex<Vec<SpannerCoordinatorDatabase>>,
        deleted: Mutex<Vec<String>>,
    }

    impl FakeSpannerClient {
        fn with_instance(instance: SpannerCoordinatorInstance) -> Self {
            Self {
                instance: Mutex::new(Some(instance)),
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
    impl SpannerCoordinatorControlPlaneClient for FakeSpannerClient {
        async fn describe_instance(
            &self,
            _instance_id: &str,
        ) -> Result<Option<SpannerCoordinatorInstance>> {
            Ok(self.instance.lock().unwrap().clone())
        }

        async fn create_instance(&self, request: SpannerCreateCoordinator) -> Result<()> {
            self.instance
                .lock()
                .unwrap()
                .replace(SpannerCoordinatorInstance {
                    instance_id: request.instance_id.clone(),
                    instance_config_id: request.instance_config_id.clone(),
                    regions: request.regions.clone(),
                    edition: request.edition.clone(),
                    external_consistency: true,
                    serializable_transactions: true,
                    strong_reads: true,
                    labels: request.labels.clone(),
                    database: None,
                });
            self.created.lock().unwrap().push(request);
            Ok(())
        }

        async fn create_database(
            &self,
            _instance_id: &str,
            database: SpannerCoordinatorDatabase,
        ) -> Result<()> {
            if let Some(instance) = self.instance.lock().unwrap().as_mut() {
                instance.database = Some(database.clone());
            }
            self.databases.lock().unwrap().push(database);
            Ok(())
        }

        async fn delete_instance(&self, instance_id: &str) -> Result<()> {
            self.instance.lock().unwrap().take();
            self.deleted.lock().unwrap().push(instance_id.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn spanner_backend_apply_creates_missing_external_consistency_instance() {
        let plan = spanner_coordinator_plan("crab-coordinator", "nam3", &["eur3".to_owned()]);
        let client = FakeSpannerClient::default();
        let backend = SpannerCoordinatorBackend::new(&client);

        let status = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.created.lock().unwrap().len(), 1);
        assert_eq!(client.created.lock().unwrap()[0].instance_config_id, "nam3");
        assert_eq!(
            client.created.lock().unwrap()[0].database_id,
            "crab_coordinator"
        );
    }

    #[tokio::test]
    async fn spanner_backend_apply_rejects_non_external_consistency_before_mutation() {
        let plan = spanner_coordinator_plan("crab-coordinator", "nam3", &["eur3".to_owned()]);
        let mut instance = verified_instance(&plan);
        instance.external_consistency = false;
        let client = FakeSpannerClient::with_instance(instance);
        let backend = SpannerCoordinatorBackend::new(&client);

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
        assert!(client.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn spanner_backend_apply_rejects_instance_config_drift_before_mutation() {
        let plan = spanner_coordinator_plan("crab-coordinator", "nam3", &["eur3".to_owned()]);
        let mut instance = verified_instance(&plan);
        instance.instance_config_id = "eur3".to_owned();
        instance.regions = planned_regions(&plan).into_iter().collect();
        let client = FakeSpannerClient::with_instance(instance);
        let backend = SpannerCoordinatorBackend::new(&client);

        let status = backend.status(&plan).await.unwrap();
        let check = status
            .checks
            .iter()
            .find(|check| check.managed_resource_id.ends_with("-instance"))
            .unwrap();
        assert_eq!(check.state, CoordinatorCheckState::Drifted);
        assert!(check.message.contains("instance config ID"));

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
        assert!(client.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn spanner_backend_remove_requires_owned_verified_instance() {
        let apply_plan = spanner_coordinator_plan("crab-coordinator", "nam3", &["eur3".to_owned()]);
        let remove_plan = coordinator_control_plane_remove_plan(&apply_plan);
        let client = FakeSpannerClient::with_instance(verified_instance(&apply_plan));
        let backend = SpannerCoordinatorBackend::new(&client);

        let status = remove_coordinator_control_plane_plan_with_backend(&remove_plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.deleted.lock().unwrap().len(), 1);
    }

    #[cfg(feature = "coordinator-spanner")]
    #[test]
    fn spanner_rest_instance_mapping_uses_config_replicas_and_ready_contract() {
        let plan = spanner_coordinator_plan(
            "crab-coordinator",
            "nam3",
            &["us-east1".to_owned(), "us-west1".to_owned()],
        );
        let instance = serde_json::json!({
            "name": "projects/test-project/instances/crab-coordinator",
            "config": "projects/test-project/instanceConfigs/nam3",
            "state": "READY",
            "edition": "ENTERPRISE_PLUS",
            "labels": expected_labels(&plan),
        });
        let config = serde_json::json!({
            "replicas": [
                {"location": "us-east1"},
                {"location": "us-west1"}
            ]
        });
        let database = SpannerCoordinatorDatabase {
            database_id: SPANNER_DATABASE_ID.to_owned(),
            tables: vec![
                "PushTransaction".to_owned(),
                "RepoEpoch".to_owned(),
                "RepoState".to_owned(),
                "RefState".to_owned(),
            ],
        };

        let mapped = spanner_instance_from_json(&instance, Some(&config), Some(database))
            .expect("map REST instance");

        assert_eq!(mapped.instance_id, "crab-coordinator");
        assert_eq!(mapped.instance_config_id, "nam3");
        assert_eq!(
            mapped.regions,
            vec![
                "nam3".to_owned(),
                "us-east1".to_owned(),
                "us-west1".to_owned()
            ]
        );
        assert!(mapped.external_consistency);
        assert!(mapped.serializable_transactions);
        assert!(mapped.strong_reads);
        assert_eq!(
            mapped.database.as_ref().unwrap().database_id,
            SPANNER_DATABASE_ID
        );
    }

    #[cfg(feature = "coordinator-spanner")]
    #[test]
    fn spanner_result_set_to_repo_state_parses_versioned_state() {
        let state = CoordinatorRepoState {
            epoch: 7,
            healthy: true,
            fence_reason: None,
            refs: BTreeMap::from([("refs/heads/main".to_owned(), "abc".to_owned())]),
            transactions: BTreeMap::new(),
            ..CoordinatorRepoState::default()
        };
        let state_json = serde_json::to_string(&state).unwrap();
        let result = serde_json::json!({
            "rows": [[
                "3",
                state_json
            ]]
        });

        let record = spanner_repo_state_from_result_set("org/repo", &result)
            .unwrap()
            .unwrap();

        assert_eq!(record.version, 3);
        assert_eq!(record.state, state);
    }

    #[cfg(feature = "coordinator-spanner")]
    #[test]
    fn spanner_state_mutation_body_uses_insert_or_update_shape() {
        let body = spanner_repo_state_commit_body("tx-1", "org/repo", 4, "{\"epoch\":1}", 123);

        assert_eq!(body["transactionId"], "tx-1");
        assert_eq!(
            body["mutations"][0]["insertOrUpdate"]["table"],
            SPANNER_STATE_TABLE
        );
        assert_eq!(
            body["mutations"][0]["insertOrUpdate"]["columns"],
            serde_json::json!(["Repo", "Version", "State", "UpdatedAtMs"])
        );
        assert_eq!(
            body["mutations"][0]["insertOrUpdate"]["values"][0],
            serde_json::json!(["org/repo", "4", "{\"epoch\":1}", "123"])
        );
    }

    #[tokio::test]
    async fn spanner_write_coordinator_rejects_stale_same_ref() {
        let store = FakeStateStore::default();
        store.seed_ref("refs/heads/main", "a");
        let coordinator =
            SpannerWriteCoordinator::new("global", SPANNER_DATABASE_ID, "org/repo", &store);

        let first = commit_uploaded_push_refs(&coordinator, commit_request("op-1", Some("a"), "b"))
            .await
            .unwrap();
        let err = commit_uploaded_push_refs(&coordinator, commit_request("op-2", Some("a"), "c"))
            .await
            .unwrap_err();

        assert_eq!(first.coordinator_epoch, 1);
        assert!(matches!(err, CoordinationError::NonFastForward { .. }));
    }

    fn verified_instance(plan: &CoordinatorControlPlanePlan) -> SpannerCoordinatorInstance {
        SpannerCoordinatorInstance {
            instance_id: plan.name.clone(),
            instance_config_id: planned_instance_config_id(plan),
            regions: planned_regions(plan).into_iter().collect(),
            edition: "ENTERPRISE_PLUS".to_owned(),
            external_consistency: true,
            serializable_transactions: true,
            strong_reads: true,
            labels: expected_labels(plan),
            database: Some(planned_database()),
        }
    }

    fn commit_request(operation_id: &str, expected: Option<&str>, new: &str) -> CommitRequest {
        CommitRequest {
            operation_id: operation_id.to_owned(),
            writer: "west".to_owned(),
            region: "us-west1".to_owned(),
            manifest_generation: 2,
            refs: vec![CoordinatedRefUpdate {
                name: "refs/heads/main".to_owned(),
                expected: expected.map(str::to_owned),
                new: Some(new.to_owned()),
                force: false,
            }],
            uploaded_objects: vec!["packs/pack-1".to_owned()],
            target_regions: vec!["us-east1".to_owned()],
        }
    }
}
