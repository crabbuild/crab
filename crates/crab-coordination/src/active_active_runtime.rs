//! Live active-active coordinator Adapters.

use std::sync::Arc;

use crate::active_active::{
    ActiveActiveCoordinatorConfig, ActiveActiveRepairPlan, ActiveActiveReplicationConfig,
    active_active_coordinator_resource, validate_active_active_config,
};
use crate::error::{CoordinationError, Result};
use crate::write_coordinator::{
    CoordinatorFenceOutcome, CoordinatorHealth, ManagedCoordinatorProvider, WriteCoordinator,
};

/// Builds a live write coordinator after provider control-plane admission proof.
pub async fn active_active_write_coordinator_for_repo(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
) -> Result<Arc<dyn WriteCoordinator>> {
    active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::Write,
    )
    .await
}

/// Builds a repair plan from the configured live coordinator.
pub async fn active_active_repair_plan_from_coordinator(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
) -> Result<ActiveActiveRepairPlan> {
    let coordinator = active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::RepairSnapshot,
    )
    .await?;
    let snapshot = coordinator.repair_snapshot().await?;
    crate::active_active::plan_active_active_repair(replication, &snapshot)
}

/// Marks repair-plan regions materialized in the configured live coordinator.
pub async fn mark_active_active_repair_materialized(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
    plan: &ActiveActiveRepairPlan,
) -> Result<()> {
    let coordinator = active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::RepairMaterialization,
    )
    .await?;
    for action in &plan.actions {
        coordinator
            .mark_region_materialized(&action.operation_id, &action.region)
            .await?;
    }
    Ok(())
}

/// Fences active-active writes through the configured live coordinator.
pub async fn fence_active_active_writes(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
    reason: Option<String>,
) -> Result<CoordinatorFenceOutcome> {
    let coordinator = active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::Failover { action: "fence" },
    )
    .await?;
    coordinator.fence_writes(reason).await
}

/// Resumes active-active writes through the configured live coordinator.
pub async fn resume_active_active_writes(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
) -> Result<CoordinatorFenceOutcome> {
    let coordinator = active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::Failover { action: "resume" },
    )
    .await?;
    coordinator.resume_writes().await
}

/// Reads live coordinator health after provider control-plane admission proof.
pub async fn active_active_coordinator_health(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
) -> Result<CoordinatorHealth> {
    let coordinator = active_active_coordinator_for_operation(
        replication,
        repo_prefix,
        ActiveActiveRuntimeOperation::Health,
    )
    .await?;
    coordinator.health().await
}

async fn active_active_coordinator_for_operation(
    replication: &ActiveActiveReplicationConfig,
    repo_prefix: &str,
    operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    if !replication.is_active_active() {
        return Err(CoordinationError::Configuration {
            key: "replication.mode".into(),
            origin: operation.invalid_mode_origin(),
        });
    }
    validate_active_active_config(replication)?;

    let coordinator =
        replication
            .coordinator
            .as_ref()
            .ok_or_else(|| CoordinationError::Configuration {
                key: "replication.coordinator".into(),
                origin: "active-active writes require a managed coordinator".into(),
            })?;
    let target = active_active_coordinator_resource(&coordinator.url)?;
    match target.provider {
        ManagedCoordinatorProvider::DynamoDb => {
            dynamodb_active_active_coordinator(coordinator, &target.name, repo_prefix, operation)
                .await
        }
        ManagedCoordinatorProvider::Spanner => {
            spanner_active_active_coordinator(coordinator, &target.name, repo_prefix, operation)
                .await
        }
        ManagedCoordinatorProvider::CosmosDb => {
            cosmosdb_active_active_coordinator(coordinator, &target.name, repo_prefix, operation)
                .await
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ActiveActiveRuntimeOperation {
    Write,
    RepairSnapshot,
    RepairMaterialization,
    Failover { action: &'static str },
    Health,
}

impl ActiveActiveRuntimeOperation {
    fn invalid_mode_origin(self) -> String {
        match self {
            Self::Write => {
                "active-active write coordinator is only valid in active-active mode".to_owned()
            }
            Self::RepairSnapshot | Self::RepairMaterialization => {
                "coordinator-backed repair is only valid in active-active mode".to_owned()
            }
            Self::Failover { .. } => {
                "active-active failover is only valid in active-active mode".to_owned()
            }
            Self::Health => {
                "active-active failover status is only valid in active-active mode".to_owned()
            }
        }
    }

    #[cfg_attr(
        all(
            feature = "coordinator-dynamodb",
            feature = "coordinator-spanner",
            feature = "coordinator-cosmosdb"
        ),
        expect(
            dead_code,
            reason = "provider-missing diagnostics compile only when a provider feature is disabled"
        )
    )]
    fn missing_feature_origin(
        self,
        provider: ManagedCoordinatorProvider,
        resource_name: &str,
        feature: &str,
    ) -> String {
        let provider = provider_display_name(provider);
        match self {
            Self::Write => format!(
                "{provider} active-active push coordinator {resource_name} requires the {feature} feature; writes fail closed"
            ),
            Self::RepairSnapshot => format!(
                "{provider} active-active repair snapshot {resource_name} requires the {feature} feature; repair fails closed"
            ),
            Self::RepairMaterialization => format!(
                "{provider} active-active repair {resource_name} requires the {feature} feature; repair fails closed"
            ),
            Self::Failover { action } => format!(
                "{provider} active-active failover {action} for {resource_name} requires the {feature} feature; writes fail closed"
            ),
            Self::Health => format!(
                "{provider} active-active failover status for {resource_name} requires the {feature} feature; writes fail closed"
            ),
        }
    }
}

fn provider_display_name(provider: ManagedCoordinatorProvider) -> &'static str {
    match provider {
        ManagedCoordinatorProvider::DynamoDb => "DynamoDB",
        ManagedCoordinatorProvider::Spanner => "Spanner",
        ManagedCoordinatorProvider::CosmosDb => "Cosmos DB",
    }
}

#[cfg(feature = "coordinator-dynamodb")]
async fn dynamodb_active_active_coordinator(
    coordinator: &ActiveActiveCoordinatorConfig,
    table_name: &str,
    repo_prefix: &str,
    _operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    use crate::dynamodb_coordinator::{
        AwsDynamoDbCoordinatorBackend, AwsDynamoDbWriteCoordinatorClient, DynamoDbWriteCoordinator,
    };
    use crate::write_coordinator::{
        dynamodb_coordinator_plan, inspect_coordinator_control_plane_plan_with_backend,
    };

    let plan = dynamodb_coordinator_plan(
        table_name,
        &coordinator.region,
        &coordinator.failover_regions,
    );
    let status =
        inspect_coordinator_control_plane_plan_with_backend(&plan, &AwsDynamoDbCoordinatorBackend)
            .await?;
    crate::write_coordinator::validate_coordinator_write_admission(&status)?;

    let client = AwsDynamoDbWriteCoordinatorClient::for_region(&coordinator.region).await;
    let coordinator = DynamoDbWriteCoordinator::new(table_name.to_owned(), repo_prefix, client);
    Ok(Arc::new(coordinator))
}

#[cfg(not(feature = "coordinator-dynamodb"))]
async fn dynamodb_active_active_coordinator(
    _coordinator: &ActiveActiveCoordinatorConfig,
    table_name: &str,
    _repo_prefix: &str,
    operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: operation.missing_feature_origin(
            ManagedCoordinatorProvider::DynamoDb,
            table_name,
            "coordinator-dynamodb",
        ),
    })
}

#[cfg(feature = "coordinator-spanner")]
async fn spanner_active_active_coordinator(
    coordinator: &ActiveActiveCoordinatorConfig,
    instance_id: &str,
    repo_prefix: &str,
    _operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    use crate::spanner_coordinator::{
        GoogleSpannerCoordinatorBackend, GoogleSpannerWriteCoordinatorClient, SPANNER_DATABASE_ID,
        SpannerWriteCoordinator,
    };
    use crate::write_coordinator::{
        inspect_coordinator_control_plane_plan_with_backend, spanner_coordinator_plan,
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
    .await?;
    crate::write_coordinator::validate_coordinator_write_admission(&status)?;

    let client = GoogleSpannerWriteCoordinatorClient::new().await?;
    let coordinator = SpannerWriteCoordinator::new(
        instance_id.to_owned(),
        SPANNER_DATABASE_ID,
        repo_prefix,
        client,
    );
    Ok(Arc::new(coordinator))
}

#[cfg(not(feature = "coordinator-spanner"))]
async fn spanner_active_active_coordinator(
    _coordinator: &ActiveActiveCoordinatorConfig,
    instance_id: &str,
    _repo_prefix: &str,
    operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: operation.missing_feature_origin(
            ManagedCoordinatorProvider::Spanner,
            instance_id,
            "coordinator-spanner",
        ),
    })
}

#[cfg(feature = "coordinator-cosmosdb")]
async fn cosmosdb_active_active_coordinator(
    coordinator: &ActiveActiveCoordinatorConfig,
    account_name: &str,
    repo_prefix: &str,
    _operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    use crate::cosmosdb_coordinator::{
        AzureCosmosDbCoordinatorBackend, AzureCosmosDbWriteCoordinatorClient,
        COSMOSDB_DATABASE_NAME, CosmosDbWriteCoordinator,
    };
    use crate::write_coordinator::{
        cosmosdb_coordinator_plan, inspect_coordinator_control_plane_plan_with_backend,
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
    .await?;
    crate::write_coordinator::validate_coordinator_write_admission(&status)?;

    let client = AzureCosmosDbWriteCoordinatorClient::new()?;
    let coordinator = CosmosDbWriteCoordinator::new(
        account_name.to_owned(),
        COSMOSDB_DATABASE_NAME,
        repo_prefix,
        client,
    );
    Ok(Arc::new(coordinator))
}

#[cfg(not(feature = "coordinator-cosmosdb"))]
async fn cosmosdb_active_active_coordinator(
    _coordinator: &ActiveActiveCoordinatorConfig,
    account_name: &str,
    _repo_prefix: &str,
    operation: ActiveActiveRuntimeOperation,
) -> Result<Arc<dyn WriteCoordinator>> {
    Err(CoordinationError::Configuration {
        key: "replication.coordinator".into(),
        origin: operation.missing_feature_origin(
            ManagedCoordinatorProvider::CosmosDb,
            account_name,
            "coordinator-cosmosdb",
        ),
    })
}
