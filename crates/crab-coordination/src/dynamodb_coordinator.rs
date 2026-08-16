//! DynamoDB coordinator control-plane backend.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{CoordinationError, Result};
use crate::write_coordinator::{
    CommitOutcome, CommitRequest, CoordinatorApplyStatus, CoordinatorCheckState,
    CoordinatorControlPlaneBackend, CoordinatorControlPlanePlan, CoordinatorControlPlaneRequest,
    CoordinatorControlPlaneStatus, CoordinatorFenceOutcome, CoordinatorGcSafetySnapshot,
    CoordinatorHealth, CoordinatorRepairSnapshot, CoordinatorRepoState, CoordinatorStateRecord,
    CoordinatorTransactionRecord, ManagedCoordinatorProvider, PushTransactionState,
    VersionedCoordinatorStateStore, VersionedStateWriteCoordinator, WriteCoordinator,
    coordination_error, coordinator_control_plane_check,
};
use async_trait::async_trait;

const OWNERSHIP_MANAGED: &str = "crab:managed";
const OWNERSHIP_RESOURCE: &str = "crab:resource";
const OWNERSHIP_COORDINATOR: &str = "crab:coordinator";
#[cfg(feature = "coordinator-dynamodb")]
const STATE_SORT_KEY: &str = "state";
const MAX_REPO_STATE_BYTES: usize = 350 * 1024;
const DEFAULT_CAS_ATTEMPTS: usize = 16;
const DEFAULT_COMPLETED_OPERATION_RECORDS: usize = 512;

/// Existing DynamoDB coordinator table state returned by a control-plane client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoDbCoordinatorTable {
    pub table_name: String,
    pub regions: Vec<String>,
    pub witness_regions: Vec<String>,
    pub billing_mode: String,
    pub consistency_mode: String,
    pub same_account: bool,
    pub tags: BTreeMap<String, String>,
}

/// Create request for a DynamoDB MRSC coordinator table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoDbCreateCoordinatorTable {
    pub table_name: String,
    pub regions: Vec<String>,
    pub witness_regions: Vec<String>,
    pub billing_mode: String,
    pub consistency_mode: String,
    pub tags: BTreeMap<String, String>,
}

/// Minimal DynamoDB control-plane client needed by Crab-owned coordinator setup.
pub trait DynamoDbCoordinatorControlPlaneClient {
    fn describe_table(&self, table_name: &str) -> Result<Option<DynamoDbCoordinatorTable>>;
    fn create_global_table(&self, request: DynamoDbCreateCoordinatorTable) -> Result<()>;
    fn tag_table(&self, table_name: &str, tags: &BTreeMap<String, String>) -> Result<()>;
    fn delete_table(&self, table_name: &str) -> Result<()>;
}

impl<T> DynamoDbCoordinatorControlPlaneClient for &T
where
    T: DynamoDbCoordinatorControlPlaneClient + ?Sized,
{
    fn describe_table(&self, table_name: &str) -> Result<Option<DynamoDbCoordinatorTable>> {
        (*self).describe_table(table_name)
    }

    fn create_global_table(&self, request: DynamoDbCreateCoordinatorTable) -> Result<()> {
        (*self).create_global_table(request)
    }

    fn tag_table(&self, table_name: &str, tags: &BTreeMap<String, String>) -> Result<()> {
        (*self).tag_table(table_name, tags)
    }

    fn delete_table(&self, table_name: &str) -> Result<()> {
        (*self).delete_table(table_name)
    }
}

/// One DynamoDB repo authority item read from the coordinator table.
pub type DynamoDbCoordinatorStateRecord = CoordinatorStateRecord;

/// Serialized active-active repository authority state stored in DynamoDB.
pub type DynamoDbRepoState = CoordinatorRepoState;

/// Serialized push transaction state stored inside the single repo authority item.
pub type DynamoDbTransactionRecord = CoordinatorTransactionRecord;

/// DynamoDB data-plane client for one serialized repo authority item.
#[async_trait]
pub trait DynamoDbWriteCoordinatorClient {
    async fn read_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
    ) -> Result<Option<DynamoDbCoordinatorStateRecord>>;

    async fn compare_and_swap_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &DynamoDbRepoState,
    ) -> Result<bool>;
}

#[async_trait]
impl<T> DynamoDbWriteCoordinatorClient for &T
where
    T: DynamoDbWriteCoordinatorClient + Sync + ?Sized,
{
    async fn read_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
    ) -> Result<Option<DynamoDbCoordinatorStateRecord>> {
        (*self).read_repo_state(table_name, repo_key).await
    }

    async fn compare_and_swap_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &DynamoDbRepoState,
    ) -> Result<bool> {
        (*self)
            .compare_and_swap_repo_state(table_name, repo_key, expected_version, next_state)
            .await
    }
}

/// DynamoDB single-item CAS implementation of the active-active write coordinator.
pub struct DynamoDbWriteCoordinator<C> {
    inner: VersionedStateWriteCoordinator<DynamoDbStateStore<C>>,
}

impl<C> DynamoDbWriteCoordinator<C> {
    #[must_use]
    pub fn new(table_name: impl Into<String>, repo_key: impl Into<String>, client: C) -> Self {
        let table_name = table_name.into();
        let repo_key = repo_key.into();
        let state_store = DynamoDbStateStore { client };
        Self {
            inner: VersionedStateWriteCoordinator::new(
                "dynamodb",
                table_name,
                repo_key,
                state_store,
            )
            .with_max_cas_attempts(DEFAULT_CAS_ATTEMPTS)
            .with_max_state_bytes(MAX_REPO_STATE_BYTES)
            .with_max_completed_operations(DEFAULT_COMPLETED_OPERATION_RECORDS),
        }
    }

    #[must_use]
    pub fn with_max_cas_attempts(mut self, max_cas_attempts: usize) -> Self {
        self.inner = self.inner.with_max_cas_attempts(max_cas_attempts);
        self
    }

    #[must_use]
    pub fn with_max_completed_operations(mut self, max_completed_operations: usize) -> Self {
        self.inner = self
            .inner
            .with_max_completed_operations(max_completed_operations);
        self
    }
}

struct DynamoDbStateStore<C> {
    client: C,
}

#[async_trait]
impl<C> WriteCoordinator for DynamoDbWriteCoordinator<C>
where
    C: DynamoDbWriteCoordinatorClient + Send + Sync,
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

#[async_trait]
impl<C> VersionedCoordinatorStateStore for DynamoDbStateStore<C>
where
    C: DynamoDbWriteCoordinatorClient + Send + Sync,
{
    async fn read_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
    ) -> crate::Result<Option<CoordinatorStateRecord>> {
        self.client
            .read_repo_state(table_name, repo_key)
            .await
            .map_err(coordination_error)
    }

    async fn compare_and_swap_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &CoordinatorRepoState,
    ) -> crate::Result<bool> {
        self.client
            .compare_and_swap_repo_state(table_name, repo_key, expected_version, next_state)
            .await
            .map_err(coordination_error)
    }
}

#[cfg(feature = "coordinator-dynamodb")]
#[derive(Debug, Clone)]
pub struct AwsDynamoDbWriteCoordinatorClient {
    client: aws_sdk_dynamodb::Client,
}

#[cfg(feature = "coordinator-dynamodb")]
impl AwsDynamoDbWriteCoordinatorClient {
    #[must_use]
    pub fn new(client: aws_sdk_dynamodb::Client) -> Self {
        Self { client }
    }

    pub async fn for_region(region: &str) -> Self {
        Self {
            client: dynamodb_sdk_client(region).await,
        }
    }
}

#[cfg(feature = "coordinator-dynamodb")]
#[async_trait]
impl DynamoDbWriteCoordinatorClient for AwsDynamoDbWriteCoordinatorClient {
    async fn read_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
    ) -> Result<Option<DynamoDbCoordinatorStateRecord>> {
        use aws_sdk_dynamodb::types::AttributeValue;

        let output = self
            .client
            .get_item()
            .table_name(table_name)
            .key("pk", AttributeValue::S(repo_key.to_owned()))
            .key("sk", AttributeValue::S(STATE_SORT_KEY.to_owned()))
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| dynamodb_sdk_error("read coordinator state", err))?;
        let Some(item) = output.item else {
            return Ok(None);
        };
        let version = item
            .get("version")
            .and_then(|value| value.as_n().ok())
            .ok_or_else(|| dynamodb_state_shape_error(repo_key, "missing version"))?
            .parse::<u64>()
            .map_err(|err| {
                dynamodb_state_shape_error(repo_key, &format!("invalid version: {err}"))
            })?;
        let state_json = item
            .get("state")
            .and_then(|value| value.as_s().ok())
            .ok_or_else(|| dynamodb_state_shape_error(repo_key, "missing state"))?;
        let state = serde_json::from_str::<DynamoDbRepoState>(state_json).map_err(|err| {
            dynamodb_state_shape_error(repo_key, &format!("invalid serialized state: {err}"))
        })?;
        Ok(Some(DynamoDbCoordinatorStateRecord { version, state }))
    }

    async fn compare_and_swap_repo_state(
        &self,
        table_name: &str,
        repo_key: &str,
        expected_version: Option<u64>,
        next_state: &DynamoDbRepoState,
    ) -> Result<bool> {
        use aws_sdk_dynamodb::types::AttributeValue;

        ensure_dynamodb_state_size(next_state)?;
        let next_version = expected_version.unwrap_or_default().saturating_add(1);
        let state_json =
            serde_json::to_string(next_state).map_err(|err| CoordinationError::Configuration {
                key: "replication.coordinator.dynamodb".into(),
                origin: format!("failed to serialize DynamoDB coordinator state: {err}"),
            })?;
        let mut put = self
            .client
            .put_item()
            .table_name(table_name)
            .item("pk", AttributeValue::S(repo_key.to_owned()))
            .item("sk", AttributeValue::S(STATE_SORT_KEY.to_owned()))
            .item("version", AttributeValue::N(next_version.to_string()))
            .item("state", AttributeValue::S(state_json));

        put = if let Some(version) = expected_version {
            put.condition_expression("#version = :expected_version")
                .expression_attribute_names("#version", "version")
                .expression_attribute_values(
                    ":expected_version",
                    AttributeValue::N(version.to_string()),
                )
        } else {
            put.condition_expression("attribute_not_exists(#pk)")
                .expression_attribute_names("#pk", "pk")
        };

        match put.send().await {
            Ok(_) => Ok(true),
            Err(err) => {
                if matches!(
                    err.as_service_error(),
                    Some(err) if err.is_conditional_check_failed_exception()
                ) {
                    return Ok(false);
                }
                Err(dynamodb_sdk_error("write coordinator state", err))
            }
        }
    }
}

#[cfg(feature = "coordinator-dynamodb")]
fn ensure_dynamodb_state_size(state: &DynamoDbRepoState) -> Result<()> {
    let state_bytes = dynamodb_state_size_bytes(state)?;
    if state_bytes <= MAX_REPO_STATE_BYTES {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator.dynamodb.state_size".into(),
        origin: format!(
            "DynamoDB coordinator repo state is {} bytes; Crab's single-item CAS limit is {} bytes",
            state_bytes, MAX_REPO_STATE_BYTES
        ),
    })
}

#[cfg(feature = "coordinator-dynamodb")]
fn dynamodb_state_size_bytes(state: &DynamoDbRepoState) -> Result<usize> {
    serde_json::to_vec(state)
        .map(|bytes| bytes.len())
        .map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb".into(),
            origin: format!("failed to serialize DynamoDB coordinator state: {err}"),
        })
}

#[cfg(feature = "coordinator-dynamodb")]
fn dynamodb_state_shape_error(repo_key: &str, reason: &str) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.dynamodb".into(),
        origin: format!("DynamoDB coordinator state for {repo_key} is invalid: {reason}"),
    }
}

/// DynamoDB implementation of the managed coordinator control-plane contract.
pub struct DynamoDbCoordinatorBackend<C> {
    client: C,
}

impl<C> DynamoDbCoordinatorBackend<C> {
    #[must_use]
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

/// DynamoDB SDK-backed coordinator control-plane backend.
#[cfg(feature = "coordinator-dynamodb")]
#[derive(Debug, Clone, Copy, Default)]
pub struct AwsDynamoDbCoordinatorBackend;

#[cfg(feature = "coordinator-dynamodb")]
#[async_trait]
impl CoordinatorControlPlaneBackend for AwsDynamoDbCoordinatorBackend {
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::DynamoDb
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        ensure_dynamodb_plan(plan)?;
        let client = dynamodb_sdk_client(&plan.region).await;
        let mut actions = Vec::new();
        if describe_sdk_table(&client, plan).await?.is_none() {
            create_sdk_table(&client, plan).await?;
            actions.push("create-global-table".to_owned());
            tag_sdk_table(&client, plan).await?;
            actions.push("put-table-tags".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("DynamoDB coordinator {} is applied", plan.name),
        })
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        ensure_dynamodb_plan(plan)?;
        let client = dynamodb_sdk_client(&plan.region).await;
        let table = describe_sdk_table(&client, plan).await?;
        let checks = plan
            .requests
            .iter()
            .map(|request| dynamodb_check(plan, request, table.as_ref()))
            .collect();

        Ok(CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
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
        ensure_dynamodb_plan(plan)?;
        let client = dynamodb_sdk_client(&plan.region).await;
        let mut actions = Vec::new();
        if describe_sdk_table(&client, plan).await?.is_some() {
            client
                .delete_table()
                .table_name(&plan.name)
                .send()
                .await
                .map_err(|err| dynamodb_sdk_error("delete coordinator table", err))?;
            actions.push("remove:create-global-table".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("DynamoDB coordinator {} is removed", plan.name),
        })
    }
}

#[cfg(feature = "coordinator-dynamodb")]
async fn dynamodb_sdk_client(region: &str) -> aws_sdk_dynamodb::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;
    aws_sdk_dynamodb::Client::new(&config)
}

#[cfg(feature = "coordinator-dynamodb")]
async fn describe_sdk_table(
    client: &aws_sdk_dynamodb::Client,
    plan: &CoordinatorControlPlanePlan,
) -> Result<Option<DynamoDbCoordinatorTable>> {
    let response = client.describe_table().table_name(&plan.name).send().await;
    let output = match response {
        Ok(output) => output,
        Err(err) => {
            if matches!(
                err.as_service_error(),
                Some(err) if err.is_resource_not_found_exception()
            ) {
                return Ok(None);
            }
            return Err(dynamodb_sdk_error("describe coordinator table", err));
        }
    };
    let Some(description) = output.table() else {
        return Ok(None);
    };
    let tags = match description.table_arn() {
        Some(arn) => list_sdk_tags(client, arn).await?,
        None => BTreeMap::new(),
    };
    Ok(Some(table_from_sdk_description(plan, description, tags)))
}

#[cfg(feature = "coordinator-dynamodb")]
async fn create_sdk_table(
    client: &aws_sdk_dynamodb::Client,
    plan: &CoordinatorControlPlanePlan,
) -> Result<()> {
    use aws_sdk_dynamodb::client::Waiters;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, BillingMode, CreateGlobalTableWitnessGroupMemberAction,
        CreateReplicationGroupMemberAction, GlobalTableWitnessGroupUpdate, KeySchemaElement,
        KeyType, MultiRegionConsistency, ReplicationGroupUpdate, ScalarAttributeType,
    };
    let topology = dynamodb_mrsc_topology(plan)?;

    let partition = AttributeDefinition::builder()
        .attribute_name("pk")
        .attribute_type(ScalarAttributeType::S)
        .build()
        .map_err(dynamodb_build_error)?;
    let sort = AttributeDefinition::builder()
        .attribute_name("sk")
        .attribute_type(ScalarAttributeType::S)
        .build()
        .map_err(dynamodb_build_error)?;
    let partition_key = KeySchemaElement::builder()
        .attribute_name("pk")
        .key_type(KeyType::Hash)
        .build()
        .map_err(dynamodb_build_error)?;
    let sort_key = KeySchemaElement::builder()
        .attribute_name("sk")
        .key_type(KeyType::Range)
        .build()
        .map_err(dynamodb_build_error)?;

    let mut create = client
        .create_table()
        .table_name(&plan.name)
        .attribute_definitions(partition)
        .attribute_definitions(sort)
        .key_schema(partition_key)
        .key_schema(sort_key)
        .billing_mode(BillingMode::PayPerRequest);
    for tag in sdk_tags(plan)? {
        create = create.tags(tag);
    }
    create
        .send()
        .await
        .map_err(|err| dynamodb_sdk_error("create coordinator table", err))?;

    client
        .wait_until_table_exists()
        .table_name(&plan.name)
        .wait(std::time::Duration::from_secs(600))
        .await
        .map_err(|err| CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb".into(),
            origin: format!(
                "timed out waiting for coordinator table {}: {err}",
                plan.name
            ),
        })?;

    let mut update = client
        .update_table()
        .table_name(&plan.name)
        .multi_region_consistency(MultiRegionConsistency::Strong);
    let mut has_replica_updates = false;
    for region in topology
        .table_regions
        .iter()
        .filter(|region| *region != &plan.region)
    {
        let create = CreateReplicationGroupMemberAction::builder()
            .region_name(region)
            .build()
            .map_err(dynamodb_build_error)?;
        update = update.replica_updates(ReplicationGroupUpdate::builder().create(create).build());
        has_replica_updates = true;
    }
    for region in &topology.witness_regions {
        let create = CreateGlobalTableWitnessGroupMemberAction::builder()
            .region_name(region)
            .build()
            .map_err(dynamodb_build_error)?;
        update = update.global_table_witness_updates(
            GlobalTableWitnessGroupUpdate::builder()
                .create(create)
                .build(),
        );
        has_replica_updates = true;
    }
    if has_replica_updates {
        update
            .send()
            .await
            .map_err(|err| dynamodb_sdk_error("create coordinator replicas", err))?;
    }
    Ok(())
}

#[cfg(feature = "coordinator-dynamodb")]
async fn tag_sdk_table(
    client: &aws_sdk_dynamodb::Client,
    plan: &CoordinatorControlPlanePlan,
) -> Result<()> {
    let Some(table) = client
        .describe_table()
        .table_name(&plan.name)
        .send()
        .await
        .map_err(|err| dynamodb_sdk_error("describe coordinator table for tagging", err))?
        .table()
        .cloned()
    else {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb".into(),
            origin: format!(
                "DynamoDB coordinator table {} was not returned after create",
                plan.name
            ),
        });
    };
    let arn = table
        .table_arn()
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb".into(),
            origin: format!(
                "DynamoDB coordinator table {} has no ARN for tagging",
                plan.name
            ),
        })?;
    let mut tag = client.tag_resource().resource_arn(arn);
    for sdk_tag in sdk_tags(plan)? {
        tag = tag.tags(sdk_tag);
    }
    tag.send()
        .await
        .map_err(|err| dynamodb_sdk_error("tag coordinator table", err))?;
    Ok(())
}

#[cfg(feature = "coordinator-dynamodb")]
async fn list_sdk_tags(
    client: &aws_sdk_dynamodb::Client,
    arn: &str,
) -> Result<BTreeMap<String, String>> {
    let mut tags = BTreeMap::new();
    let mut next_token = None;
    loop {
        let mut request = client.list_tags_of_resource().resource_arn(arn);
        if let Some(token) = next_token.take() {
            request = request.next_token(token);
        }
        let output = request
            .send()
            .await
            .map_err(|err| dynamodb_sdk_error("list coordinator table tags", err))?;
        for tag in output.tags() {
            tags.insert(tag.key().to_owned(), tag.value().to_owned());
        }
        next_token = output.next_token().map(str::to_owned);
        if next_token.is_none() {
            return Ok(tags);
        }
    }
}

#[cfg(feature = "coordinator-dynamodb")]
fn table_from_sdk_description(
    plan: &CoordinatorControlPlanePlan,
    description: &aws_sdk_dynamodb::types::TableDescription,
    tags: BTreeMap<String, String>,
) -> DynamoDbCoordinatorTable {
    let mut regions = BTreeSet::from([plan.region.clone()]);
    let mut witness_regions = BTreeSet::new();
    let mut replica_accounts = Vec::new();
    for replica in description.replicas() {
        if let Some(region) = replica.region_name() {
            regions.insert(region.to_owned());
        }
        if let Some(account) = replica.replica_arn().and_then(arn_account_id) {
            replica_accounts.push(account);
        }
    }
    for witness in description.global_table_witnesses() {
        if let Some(region) = witness.region_name() {
            witness_regions.insert(region.to_owned());
        }
    }
    let table_account = description.table_arn().and_then(arn_account_id);
    let same_account = table_account.is_some_and(|account| {
        replica_accounts
            .iter()
            .all(|replica_account| *replica_account == account)
    });
    let billing_mode = description
        .billing_mode_summary()
        .and_then(|summary| summary.billing_mode())
        .map_or("UNKNOWN", |mode| mode.as_str())
        .to_owned();
    let consistency_mode = description
        .multi_region_consistency()
        .map_or("UNKNOWN", |mode| match mode.as_str() {
            "STRONG" => "MRSC",
            "EVENTUAL" => "MREC",
            other => other,
        })
        .to_owned();

    DynamoDbCoordinatorTable {
        table_name: description
            .table_name()
            .unwrap_or(plan.name.as_str())
            .to_owned(),
        regions: regions.into_iter().collect(),
        witness_regions: witness_regions.into_iter().collect(),
        billing_mode,
        consistency_mode,
        same_account,
        tags,
    }
}

#[cfg(feature = "coordinator-dynamodb")]
fn sdk_tags(plan: &CoordinatorControlPlanePlan) -> Result<Vec<aws_sdk_dynamodb::types::Tag>> {
    expected_ownership_tags(plan)
        .into_iter()
        .map(|(key, value)| {
            aws_sdk_dynamodb::types::Tag::builder()
                .key(key)
                .value(value)
                .build()
                .map_err(dynamodb_build_error)
        })
        .collect()
}

#[cfg(feature = "coordinator-dynamodb")]
fn arn_account_id(arn: &str) -> Option<&str> {
    arn.split(':').nth(4).filter(|account| !account.is_empty())
}

#[cfg(feature = "coordinator-dynamodb")]
fn dynamodb_build_error(err: aws_sdk_dynamodb::error::BuildError) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.dynamodb".into(),
        origin: format!("failed to build DynamoDB coordinator request: {err}"),
    }
}

#[cfg(feature = "coordinator-dynamodb")]
fn dynamodb_sdk_error(
    action: &str,
    err: impl std::error::Error + Send + Sync + 'static,
) -> CoordinationError {
    CoordinationError::Configuration {
        key: "replication.coordinator.dynamodb".into(),
        origin: format!("DynamoDB {action} failed: {err}"),
    }
}

#[async_trait]
impl<C> CoordinatorControlPlaneBackend for DynamoDbCoordinatorBackend<C>
where
    C: DynamoDbCoordinatorControlPlaneClient + Send + Sync,
{
    fn provider(&self) -> ManagedCoordinatorProvider {
        ManagedCoordinatorProvider::DynamoDb
    }

    async fn apply(&self, plan: &CoordinatorControlPlanePlan) -> Result<CoordinatorApplyStatus> {
        ensure_dynamodb_plan(plan)?;
        let mut actions = Vec::new();
        if self.client.describe_table(&plan.name)?.is_none() {
            self.client
                .create_global_table(create_table_request(plan)?)?;
            self.client
                .tag_table(&plan.name, &expected_ownership_tags(plan))?;
            actions.push("create-global-table".to_owned());
            actions.push("put-table-tags".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("DynamoDB coordinator {} is applied", plan.name),
        })
    }

    async fn status(
        &self,
        plan: &CoordinatorControlPlanePlan,
    ) -> Result<CoordinatorControlPlaneStatus> {
        ensure_dynamodb_plan(plan)?;
        let table = self.client.describe_table(&plan.name)?;
        let checks = plan
            .requests
            .iter()
            .map(|request| dynamodb_check(plan, request, table.as_ref()))
            .collect();

        Ok(CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
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
        ensure_dynamodb_plan(plan)?;
        let mut actions = Vec::new();
        if self.client.describe_table(&plan.name)?.is_some() {
            self.client.delete_table(&plan.name)?;
            actions.push("remove:create-global-table".to_owned());
        }

        Ok(CoordinatorApplyStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            applied: !actions.is_empty(),
            checked_drift: true,
            actions,
            message: format!("DynamoDB coordinator {} is removed", plan.name),
        })
    }
}

fn ensure_dynamodb_plan(plan: &CoordinatorControlPlanePlan) -> Result<()> {
    if plan.provider == ManagedCoordinatorProvider::DynamoDb {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: format!(
            "DynamoDB coordinator backend cannot manage {} coordinator plan",
            plan.provider.as_str()
        ),
    })
}

fn dynamodb_check(
    plan: &CoordinatorControlPlanePlan,
    request: &CoordinatorControlPlaneRequest,
    table: Option<&DynamoDbCoordinatorTable>,
) -> crate::write_coordinator::CoordinatorControlPlaneCheck {
    let action = request
        .action
        .strip_prefix("remove:")
        .unwrap_or(request.action.as_str());
    let (state, message) = match action {
        "create-global-table" => table_existence_state(plan, table),
        "validate-linearizable-contract" => linearizable_contract_state(plan, table),
        "put-table-tags" => ownership_state(plan, table),
        _ => (
            CoordinatorCheckState::Unsupported,
            format!(
                "DynamoDB coordinator action {} is unsupported",
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

fn table_existence_state(
    plan: &CoordinatorControlPlanePlan,
    table: Option<&DynamoDbCoordinatorTable>,
) -> (CoordinatorCheckState, String) {
    let topology = match dynamodb_mrsc_topology(plan) {
        Ok(topology) => topology,
        Err(err) => {
            return (
                CoordinatorCheckState::Drifted,
                format!(
                    "DynamoDB coordinator {} has invalid MRSC topology: {err}",
                    plan.name
                ),
            );
        }
    };
    let Some(table) = table else {
        return (
            CoordinatorCheckState::Missing,
            format!("DynamoDB MRSC global table {} is missing", plan.name),
        );
    };

    if table.table_name != plan.name {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB table {} does not match planned coordinator {}",
                table.table_name, plan.name
            ),
        );
    }
    if topology.table_regions != table.regions.iter().cloned().collect::<BTreeSet<_>>() {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} table replicas do not match planned MRSC topology",
                plan.name
            ),
        );
    }
    if topology.witness_regions
        != table
            .witness_regions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
    {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} witnesses do not match planned MRSC topology",
                plan.name
            ),
        );
    }
    if table.billing_mode != "PAY_PER_REQUEST" {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} billing mode must be PAY_PER_REQUEST",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!("DynamoDB coordinator {} table matches the plan", plan.name),
    )
}

fn linearizable_contract_state(
    plan: &CoordinatorControlPlanePlan,
    table: Option<&DynamoDbCoordinatorTable>,
) -> (CoordinatorCheckState, String) {
    if let Err(err) = dynamodb_mrsc_topology(plan) {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} has invalid MRSC topology: {err}",
                plan.name
            ),
        );
    }
    let Some(table) = table else {
        return (
            CoordinatorCheckState::Verified,
            format!(
                "DynamoDB coordinator {} planned table satisfies the MRSC conditional-write contract",
                plan.name
            ),
        );
    };
    if table.consistency_mode != "MRSC" {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} must use multi-Region strong consistency",
                plan.name
            ),
        );
    }
    if !table.same_account {
        return (
            CoordinatorCheckState::Drifted,
            format!(
                "DynamoDB coordinator {} must be a same-account MRSC global table",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Verified,
        format!(
            "DynamoDB coordinator {} satisfies the MRSC conditional-write contract",
            plan.name
        ),
    )
}

fn ownership_state(
    plan: &CoordinatorControlPlanePlan,
    table: Option<&DynamoDbCoordinatorTable>,
) -> (CoordinatorCheckState, String) {
    let Some(table) = table else {
        return (
            CoordinatorCheckState::Missing,
            format!(
                "DynamoDB coordinator {} ownership tags are missing",
                plan.name
            ),
        );
    };
    let expected = expected_ownership_tags(plan);
    if expected
        .iter()
        .all(|(key, value)| table.tags.get(key) == Some(value))
    {
        return (
            CoordinatorCheckState::Verified,
            format!(
                "DynamoDB coordinator {} ownership tags are verified",
                plan.name
            ),
        );
    }
    (
        CoordinatorCheckState::Drifted,
        format!(
            "DynamoDB coordinator {} is not tagged as a Crab-managed coordinator",
            plan.name
        ),
    )
}

fn create_table_request(
    plan: &CoordinatorControlPlanePlan,
) -> Result<DynamoDbCreateCoordinatorTable> {
    let topology = dynamodb_mrsc_topology(plan)?;
    Ok(DynamoDbCreateCoordinatorTable {
        table_name: plan.name.clone(),
        regions: topology.table_regions.into_iter().collect(),
        witness_regions: topology.witness_regions.into_iter().collect(),
        billing_mode: "PAY_PER_REQUEST".to_owned(),
        consistency_mode: "MRSC".to_owned(),
        tags: expected_ownership_tags(plan),
    })
}

fn planned_regions(plan: &CoordinatorControlPlanePlan) -> BTreeSet<String> {
    std::iter::once(plan.region.clone())
        .chain(plan.failover_regions.iter().cloned())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DynamoDbMrscTopology {
    table_regions: BTreeSet<String>,
    witness_regions: BTreeSet<String>,
}

fn dynamodb_mrsc_topology(plan: &CoordinatorControlPlanePlan) -> Result<DynamoDbMrscTopology> {
    let table_regions = planned_regions(plan);
    if table_regions.len() != plan.failover_regions.len().saturating_add(1) {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb.failover_regions".into(),
            origin: "DynamoDB MRSC coordinator regions must be distinct".into(),
        });
    }
    if !(1..=2).contains(&plan.failover_regions.len()) {
        return Err(CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb.failover_regions".into(),
            origin: "DynamoDB MRSC requires one failover region plus a witness or two full failover replicas".into(),
        });
    }

    let supported_set = dynamodb_mrsc_region_set(&table_regions)?;
    let mut witness_regions = BTreeSet::new();
    if table_regions.len() == 2 {
        let Some(witness) = supported_set
            .iter()
            .find(|region| !table_regions.contains(**region))
        else {
            return Err(CoordinationError::Configuration {
                key: "replication.coordinator.dynamodb.failover_regions".into(),
                origin: "DynamoDB MRSC witness region could not be derived from the configured region set".into(),
            });
        };
        witness_regions.insert((*witness).to_owned());
    }

    Ok(DynamoDbMrscTopology {
        table_regions,
        witness_regions,
    })
}

fn dynamodb_mrsc_region_set(regions: &BTreeSet<String>) -> Result<&'static [&'static str]> {
    const REGION_SETS: &[&[&str]] = &[
        &["us-east-1", "us-east-2", "us-west-2"],
        &["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1"],
        &["ap-northeast-1", "ap-northeast-2", "ap-northeast-3"],
    ];

    REGION_SETS
        .iter()
        .copied()
        .find(|region_set| {
            regions
                .iter()
                .all(|region| region_set.contains(&region.as_str()))
        })
        .ok_or_else(|| CoordinationError::Configuration {
            key: "replication.coordinator.dynamodb.failover_regions".into(),
            origin: "DynamoDB MRSC regions must come from one supported AWS MRSC region set".into(),
        })
}

fn expected_ownership_tags(plan: &CoordinatorControlPlanePlan) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNERSHIP_MANAGED.to_owned(), "true".to_owned()),
        (
            OWNERSHIP_RESOURCE.to_owned(),
            "write-coordinator".to_owned(),
        ),
        (OWNERSHIP_COORDINATOR.to_owned(), plan.name.clone()),
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;
    #[cfg(feature = "coordinator-dynamodb")]
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::error::CoordinationError;

    use super::*;
    use crate::write_coordinator::{
        apply_coordinator_control_plane_plan_with_backend, commit_uploaded_push,
        coordinator_control_plane_remove_plan, dynamodb_coordinator_plan,
        remove_coordinator_control_plane_plan_with_backend,
    };

    #[derive(Default)]
    struct FakeDynamoDbClient {
        table: Mutex<Option<DynamoDbCoordinatorTable>>,
        created: Mutex<Vec<DynamoDbCreateCoordinatorTable>>,
        tagged: Mutex<Vec<(String, BTreeMap<String, String>)>>,
        deleted: Mutex<Vec<String>>,
    }

    impl FakeDynamoDbClient {
        fn with_table(table: DynamoDbCoordinatorTable) -> Self {
            Self {
                table: Mutex::new(Some(table)),
                ..Self::default()
            }
        }

        fn created_count(&self) -> usize {
            self.created.lock().unwrap().len()
        }

        fn deleted_count(&self) -> usize {
            self.deleted.lock().unwrap().len()
        }
    }

    impl DynamoDbCoordinatorControlPlaneClient for FakeDynamoDbClient {
        fn describe_table(&self, _table_name: &str) -> Result<Option<DynamoDbCoordinatorTable>> {
            Ok(self.table.lock().unwrap().clone())
        }

        fn create_global_table(&self, request: DynamoDbCreateCoordinatorTable) -> Result<()> {
            self.table
                .lock()
                .unwrap()
                .replace(DynamoDbCoordinatorTable {
                    table_name: request.table_name.clone(),
                    regions: request.regions.clone(),
                    witness_regions: request.witness_regions.clone(),
                    billing_mode: request.billing_mode.clone(),
                    consistency_mode: request.consistency_mode.clone(),
                    same_account: true,
                    tags: request.tags.clone(),
                });
            self.created.lock().unwrap().push(request);
            Ok(())
        }

        fn tag_table(&self, table_name: &str, tags: &BTreeMap<String, String>) -> Result<()> {
            self.tagged
                .lock()
                .unwrap()
                .push((table_name.to_owned(), tags.clone()));
            Ok(())
        }

        fn delete_table(&self, table_name: &str) -> Result<()> {
            self.table.lock().unwrap().take();
            self.deleted.lock().unwrap().push(table_name.to_owned());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDynamoDbWriteClient {
        state: Mutex<Option<DynamoDbCoordinatorStateRecord>>,
        failed_cas: Mutex<usize>,
        writes: Mutex<usize>,
    }

    impl FakeDynamoDbWriteClient {
        fn with_state(state: DynamoDbRepoState) -> Self {
            Self {
                state: Mutex::new(Some(DynamoDbCoordinatorStateRecord { version: 1, state })),
                ..Self::default()
            }
        }

        fn fail_next_cas(&self, count: usize) {
            *self.failed_cas.lock().unwrap() = count;
        }

        fn fence_epoch(&self) {
            let mut state = self.state.lock().unwrap();
            let mut repo_state = state
                .as_ref()
                .map_or_else(DynamoDbRepoState::default, |record| record.state.clone());
            repo_state.epoch = repo_state.epoch.saturating_add(1);
            let next_version = state
                .as_ref()
                .map_or(1, |record| record.version.saturating_add(1));
            state.replace(DynamoDbCoordinatorStateRecord {
                version: next_version,
                state: repo_state,
            });
        }

        fn state(&self) -> DynamoDbRepoState {
            self.state
                .lock()
                .unwrap()
                .as_ref()
                .map_or_else(DynamoDbRepoState::default, |record| record.state.clone())
        }

        fn write_count(&self) -> usize {
            *self.writes.lock().unwrap()
        }
    }

    #[async_trait]
    impl DynamoDbWriteCoordinatorClient for FakeDynamoDbWriteClient {
        async fn read_repo_state(
            &self,
            _table_name: &str,
            _repo_key: &str,
        ) -> Result<Option<DynamoDbCoordinatorStateRecord>> {
            Ok(self.state.lock().unwrap().clone())
        }

        async fn compare_and_swap_repo_state(
            &self,
            _table_name: &str,
            _repo_key: &str,
            expected_version: Option<u64>,
            next_state: &DynamoDbRepoState,
        ) -> Result<bool> {
            let mut failed_cas = self.failed_cas.lock().unwrap();
            if *failed_cas > 0 {
                *failed_cas -= 1;
                return Ok(false);
            }
            drop(failed_cas);

            let mut state = self.state.lock().unwrap();
            let current_version = state.as_ref().map(|record| record.version);
            if current_version != expected_version {
                return Ok(false);
            }
            let next_version = expected_version.unwrap_or_default().saturating_add(1);
            *state = Some(DynamoDbCoordinatorStateRecord {
                version: next_version,
                state: next_state.clone(),
            });
            *self.writes.lock().unwrap() += 1;
            Ok(true)
        }
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_commits_refs_through_single_state_cas() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);

        let outcome = commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap();

        assert_eq!(outcome.state, PushTransactionState::Materialized);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("b".to_owned())
        );
        let state = client.state();
        assert!(!state.transactions.contains_key("op-1"));
        assert_eq!(
            state.completed_operations["op-1"].state,
            PushTransactionState::Materialized
        );
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_replays_operation_id_idempotently() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);
        let request = commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]);

        let first = commit_uploaded_push(&coordinator, request.clone())
            .await
            .unwrap();
        let second = commit_uploaded_push(&coordinator, request).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("b".to_owned())
        );
    }

    #[tokio::test]
    async fn dynamodb_completed_operation_cache_is_bounded_but_replays_recent_operation() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client)
            .with_max_completed_operations(1);

        commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap();
        let second_request = commit_request("op-2", Some("b"), Some("c"), &["xorb/2"]);
        let first = commit_uploaded_push(&coordinator, second_request.clone())
            .await
            .unwrap();
        let state = client.state();

        assert_eq!(state.completed_operations.len(), 1);
        assert!(!state.transactions.contains_key("op-2"));
        assert!(!state.completed_operations.contains_key("op-1"));
        assert!(state.completed_operations.contains_key("op-2"));
        assert_eq!(
            commit_uploaded_push(&coordinator, second_request)
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            coordinator
                .mark_region_materialized("op-2", "us-east-1")
                .await
                .unwrap(),
            PushTransactionState::Materialized
        );
    }

    #[tokio::test]
    async fn dynamodb_health_reports_state_summary() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client)
            .with_max_completed_operations(2);

        commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap();
        coordinator
            .begin(commit_request("op-2", Some("b"), Some("c"), &["xorb/2"]))
            .await
            .unwrap();

        let health = coordinator.health().await.unwrap();
        let summary = health.state_summary.unwrap();

        assert_eq!(summary.transaction_count, 1);
        assert_eq!(summary.completed_operation_count, 1);
        assert_eq!(summary.max_completed_operations, 2);
        assert!(summary.state_bytes > 0);
        assert_eq!(summary.max_state_bytes, Some(MAX_REPO_STATE_BYTES));
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_epoch_fences_pending_transaction_commit() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);
        let request = commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]);

        coordinator.begin(request.clone()).await.unwrap();
        client.fence_epoch();
        let err = coordinator.commit(request).await.unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("a".to_owned())
        );
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_epoch_keeps_committed_replay_idempotent() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);
        let request = commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]);

        let first = coordinator.commit(request.clone()).await.unwrap();
        client.fence_epoch();
        let second = coordinator.commit(request).await.unwrap();

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_failover_fence_blocks_writes_until_resume() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);

        let fence = coordinator
            .fence_writes(Some("operator fenced region-a".to_owned()))
            .await
            .unwrap();
        let err = commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap_err();
        let resume = coordinator.resume_writes().await.unwrap();
        let outcome = commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap();

        assert_eq!(fence.previous_epoch, 1);
        assert_eq!(fence.coordinator_epoch, 2);
        assert!(!fence.healthy);
        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert_eq!(resume.coordinator_epoch, 2);
        assert!(resume.healthy);
        assert_eq!(outcome.coordinator_epoch, 2);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("b".to_owned())
        );
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_rejects_stale_ref_update() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);

        let err = commit_uploaded_push(
            &coordinator,
            commit_request("op-1", Some("stale"), Some("b"), &["xorb/1"]),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CoordinationError::NonFastForward { .. }));
        let state = client.state();
        assert!(!state.transactions.contains_key("op-1"));
        assert_eq!(
            state.completed_operations["op-1"].state,
            PushTransactionState::Aborted
        );
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_retries_cas_conflict() {
        let client = FakeDynamoDbWriteClient::default();
        client.fail_next_cas(1);
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);

        let state = coordinator
            .begin(commit_request("op-1", None, Some("b"), &[]))
            .await
            .unwrap();

        assert_eq!(state, PushTransactionState::Pending);
        assert_eq!(client.write_count(), 1);
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_reports_repair_gaps() {
        let client = FakeDynamoDbWriteClient::with_state(state_with_ref("refs/heads/main", "a"));
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);
        let mut request = commit_request("op-1", Some("a"), Some("b"), &["xorb/1"]);
        request.target_regions = vec!["us-east-1".to_owned(), "us-west-2".to_owned()];

        commit_uploaded_push(&coordinator, request).await.unwrap();
        let snapshot = coordinator.repair_snapshot().await.unwrap();

        assert_eq!(snapshot.materialization_gaps.len(), 1);
        assert_eq!(snapshot.materialization_gaps[0].region, "us-west-2");
        assert_eq!(
            snapshot.materialization_gaps[0].uploaded_objects,
            vec!["xorb/1"]
        );
    }

    #[tokio::test]
    async fn dynamodb_write_coordinator_protects_unmaterialized_objects_from_gc() {
        let client = FakeDynamoDbWriteClient::default();
        let coordinator = DynamoDbWriteCoordinator::new("crab-coordinator", "org/repo", &client);
        let request = commit_request("op-1", None, Some("b"), &["xorb/1"]);

        coordinator.begin(request).await.unwrap();
        coordinator.mark_objects_uploaded("op-1").await.unwrap();
        let snapshot = coordinator.gc_safety_snapshot().await.unwrap();

        assert_eq!(
            snapshot.protected_keys(),
            std::collections::HashSet::from(["xorb/1".to_owned()])
        );
    }

    #[cfg(feature = "coordinator-dynamodb")]
    #[tokio::test]
    #[ignore = "requires DynamoDB Local at CRAB_DYNAMODB_LOCAL_ENDPOINT or http://127.0.0.1:8000"]
    async fn dynamodb_local_exercises_sdk_single_item_cas() {
        let endpoint = std::env::var("CRAB_DYNAMODB_LOCAL_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:8000".to_owned());
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .load()
            .await;
        let client = aws_sdk_dynamodb::Client::new(&config);
        let table_name = unique_local_table_name();
        create_local_table(&client, &table_name).await;

        let coordinator = DynamoDbWriteCoordinator::new(
            table_name.clone(),
            "local/repo",
            AwsDynamoDbWriteCoordinatorClient::new(client.clone()),
        );

        let first = commit_uploaded_push(
            &coordinator,
            commit_request("op-local-1", None, Some("a"), &["xorb/local-1"]),
        )
        .await
        .unwrap();
        let replay = commit_uploaded_push(
            &coordinator,
            commit_request("op-local-1", None, Some("a"), &["xorb/local-1"]),
        )
        .await
        .unwrap();
        let stale = commit_uploaded_push(
            &coordinator,
            commit_request("op-local-2", None, Some("b"), &["xorb/local-2"]),
        )
        .await
        .unwrap_err();
        let fence = coordinator
            .fence_writes(Some("local verification fence".to_owned()))
            .await
            .unwrap();
        let fenced_write = commit_uploaded_push(
            &coordinator,
            commit_request("op-local-3", Some("a"), Some("c"), &["xorb/local-3"]),
        )
        .await
        .unwrap_err();
        let resume = coordinator.resume_writes().await.unwrap();
        let second = commit_uploaded_push(
            &coordinator,
            commit_request("op-local-3", Some("a"), Some("c"), &["xorb/local-3"]),
        )
        .await
        .unwrap();
        let health = coordinator.health().await.unwrap();

        assert_eq!(first.state, PushTransactionState::Materialized);
        assert_eq!(first, replay);
        assert!(matches!(stale, CoordinationError::NonFastForward { .. }));
        assert!(!fence.healthy);
        assert!(matches!(
            fenced_write,
            CoordinationError::Configuration { .. }
        ));
        assert!(resume.healthy);
        assert_eq!(second.coordinator_epoch, resume.coordinator_epoch);
        assert_eq!(
            coordinator.ref_value("refs/heads/main").await.unwrap(),
            Some("c".to_owned())
        );
        assert!(health.healthy);
        assert!(health.linearizable);

        let _ = client.delete_table().table_name(table_name).send().await;
    }

    #[tokio::test]
    async fn dynamodb_backend_apply_creates_missing_mrsc_table() {
        let plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);
        let client = FakeDynamoDbClient::default();
        let backend = DynamoDbCoordinatorBackend::new(&client);

        let status = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.created_count(), 1);
        assert_eq!(
            client.created.lock().unwrap()[0].regions,
            vec!["us-east-1".to_owned(), "us-west-2".to_owned()]
        );
        assert_eq!(
            client.created.lock().unwrap()[0].witness_regions,
            vec!["us-east-2".to_owned()]
        );
        assert_eq!(client.tagged.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dynamodb_backend_apply_rejects_invalid_mrsc_region_set_before_mutation() {
        let plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["eu-west-1".to_owned()]);
        let client = FakeDynamoDbClient::default();
        let backend = DynamoDbCoordinatorBackend::new(&client);
        let status = backend.status(&plan).await.unwrap();

        assert!(status.checks.iter().any(|check| {
            check.state == CoordinatorCheckState::Drifted
                && check.message.contains("supported AWS MRSC region set")
        }));

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert_eq!(client.created_count(), 0);
    }

    #[tokio::test]
    async fn dynamodb_backend_apply_rejects_drift_before_mutation() {
        let plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);
        let mut table = verified_table(&plan);
        table.consistency_mode = "MREC".to_owned();
        let client = FakeDynamoDbClient::with_table(table);
        let backend = DynamoDbCoordinatorBackend::new(&client);

        let err = apply_coordinator_control_plane_plan_with_backend(&plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert!(err.to_string().contains("drifted"));
        assert_eq!(client.created_count(), 0);
    }

    #[tokio::test]
    async fn dynamodb_backend_remove_requires_owned_verified_table() {
        let apply_plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);
        let remove_plan = coordinator_control_plane_remove_plan(&apply_plan);
        let client = FakeDynamoDbClient::with_table(verified_table(&apply_plan));
        let backend = DynamoDbCoordinatorBackend::new(&client);

        let status = remove_coordinator_control_plane_plan_with_backend(&remove_plan, &backend)
            .await
            .unwrap();

        assert!(status.applied);
        assert_eq!(client.deleted_count(), 1);
    }

    #[tokio::test]
    async fn dynamodb_backend_remove_rejects_unowned_table() {
        let apply_plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);
        let remove_plan = coordinator_control_plane_remove_plan(&apply_plan);
        let mut table = verified_table(&apply_plan);
        table.tags.clear();
        let client = FakeDynamoDbClient::with_table(table);
        let backend = DynamoDbCoordinatorBackend::new(&client);

        let err = remove_coordinator_control_plane_plan_with_backend(&remove_plan, &backend)
            .await
            .unwrap_err();

        assert!(matches!(err, CoordinationError::Configuration { .. }));
        assert_eq!(client.deleted_count(), 0);
    }

    fn verified_table(plan: &CoordinatorControlPlanePlan) -> DynamoDbCoordinatorTable {
        let topology = dynamodb_mrsc_topology(plan).unwrap();
        DynamoDbCoordinatorTable {
            table_name: plan.name.clone(),
            regions: topology.table_regions.into_iter().collect(),
            witness_regions: topology.witness_regions.into_iter().collect(),
            billing_mode: "PAY_PER_REQUEST".to_owned(),
            consistency_mode: "MRSC".to_owned(),
            same_account: true,
            tags: expected_ownership_tags(plan),
        }
    }

    fn state_with_ref(name: &str, value: &str) -> DynamoDbRepoState {
        let mut state = DynamoDbRepoState::default();
        state.refs.insert(name.to_owned(), value.to_owned());
        state
    }

    fn commit_request(
        operation_id: &str,
        expected: Option<&str>,
        new: Option<&str>,
        uploaded_objects: &[&str],
    ) -> CommitRequest {
        CommitRequest {
            operation_id: operation_id.to_owned(),
            writer: "east".to_owned(),
            region: "us-east-1".to_owned(),
            manifest_generation: 7,
            refs: vec![crate::write_coordinator::CoordinatedRefUpdate {
                name: "refs/heads/main".to_owned(),
                expected: expected.map(str::to_owned),
                new: new.map(str::to_owned),
                force: false,
            }],
            uploaded_objects: uploaded_objects
                .iter()
                .map(|object| (*object).to_owned())
                .collect(),
            target_regions: Vec::new(),
        }
    }

    #[cfg(feature = "coordinator-dynamodb")]
    fn unique_local_table_name() -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        format!("crab-active-active-local-{timestamp}")
    }

    #[cfg(feature = "coordinator-dynamodb")]
    async fn create_local_table(client: &aws_sdk_dynamodb::Client, table_name: &str) {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };

        let pk = AttributeDefinition::builder()
            .attribute_name("pk")
            .attribute_type(ScalarAttributeType::S)
            .build()
            .expect("pk attribute definition should build");
        let sk = AttributeDefinition::builder()
            .attribute_name("sk")
            .attribute_type(ScalarAttributeType::S)
            .build()
            .expect("sk attribute definition should build");
        let hash_key = KeySchemaElement::builder()
            .attribute_name("pk")
            .key_type(KeyType::Hash)
            .build()
            .expect("pk key schema should build");
        let range_key = KeySchemaElement::builder()
            .attribute_name("sk")
            .key_type(KeyType::Range)
            .build()
            .expect("sk key schema should build");

        client
            .create_table()
            .table_name(table_name)
            .attribute_definitions(pk)
            .attribute_definitions(sk)
            .key_schema(hash_key)
            .key_schema(range_key)
            .billing_mode(BillingMode::PayPerRequest)
            .send()
            .await
            .expect("DynamoDB Local coordinator table should be created");
    }
}
