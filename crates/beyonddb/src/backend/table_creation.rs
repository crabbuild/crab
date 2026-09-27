//! Complete cataloged table creation through the same request and recovery path.

use crab_cell_runtime::client::{CellClient, InvocationError};
use extenddb_storage::error::StorageError;

use super::{InitialPartitionProvisioner, cell_error, mutation_identity};
use crate::{
    ActivateTableRoute, ActivateTableRouteOutcome, Json, RoutePageInput, RoutePageOutcome,
    TableRecord, TableRoute, account_target,
};

pub(crate) async fn publish_initial_routes(
    provisioner: &dyn InitialPartitionProvisioner,
    client: &CellClient,
    account_id: &str,
    record: &TableRecord,
) -> Result<(), StorageError> {
    let target =
        account_target(account_id).map_err(|error| StorageError::Internal(error.to_string()))?;
    let page = crate::read_route_page(
        client,
        &target,
        RoutePageInput {
            table_id: record.id.clone(),
            start_hash: None,
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await?;
    if matches!(page, RoutePageOutcome::Page { .. }) {
        return Ok(());
    }
    for index in &record.global_secondary_indexes {
        // An earlier attempt may have published this index before base admission
        // failed. Preserve that directory and its independently installed owners.
        let published = client
            .query::<crate::ReadRouteDirectory>(&target, None, Json(index.id.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if published.is_some() {
            continue;
        }
        let partitions: Vec<_> = provisioner
            .provision_global_index(client, account_id, record, index)
            .await?
            .into_iter()
            .map(|range| crate::RoutePagePartition {
                partition_id: range.partition_id,
                lower: range.lower.unwrap_or([0; 16]),
                upper: range.upper,
                epoch: range.epoch,
            })
            .collect();
        let receipt = provisioner
            .provision_route_directory(client, account_id, &index.id, partitions.clone())
            .await?;
        client
            .command::<crate::ActivateGlobalIndexRoute>(
                &target,
                mutation_identity()?,
                Json(crate::GlobalIndexRoute {
                    table: record.clone(),
                    index: index.clone(),
                    partitions,
                    receipt,
                }),
            )
            .await
            .map_err(cell_error)?;
    }
    let partitions = provisioner.provision(client, account_id, record).await?;
    let route = TableRoute {
        table_id: record.id.clone(),
        epoch: 1,
        partitions,
    };
    let ranges = route
        .partitions
        .iter()
        .map(|part| crate::RoutePagePartition {
            partition_id: part.partition_id,
            lower: part.lower.unwrap_or([0; 16]),
            upper: part.upper,
            epoch: part.epoch,
        })
        .collect();
    let receipt = provisioner
        .provision_route_directory(client, account_id, &record.id, ranges)
        .await?;
    match client
        .command::<ActivateTableRoute>(
            &target,
            mutation_identity()?,
            Json(crate::TableRoutePublication { route, receipt }),
        )
        .await
    {
        Ok(committed) if committed.output.0 == ActivateTableRouteOutcome::Activated => {}
        Ok(_) => {
            return Err(StorageError::Internal(
                "unexpected successful route activation".into(),
            ));
        }
        Err(InvocationError::Rejected(committed)) => {
            return Err(match committed.output.0 {
                ActivateTableRouteOutcome::TableNotFound => {
                    StorageError::TableNotFound(record.table_name.clone())
                }
                ActivateTableRouteOutcome::AlreadyActive => {
                    StorageError::Transient("table route changed during creation".into())
                }
                ActivateTableRouteOutcome::TransactionConflict => {
                    StorageError::Transient("table has prepared transactions".into())
                }
                ActivateTableRouteOutcome::IndexesNotReady => {
                    StorageError::Transient("global index routes are not ready".into())
                }
                ActivateTableRouteOutcome::TableNotEmpty => {
                    StorageError::TableNotActive(record.table_name.clone())
                }
                ActivateTableRouteOutcome::InvalidRoute => {
                    StorageError::Internal("provisioner returned an invalid table route".into())
                }
                ActivateTableRouteOutcome::Activated => {
                    StorageError::Internal("unexpected rejected route activation".into())
                }
            });
        }
        Err(error) => return Err(cell_error(error)),
    }
    Ok(())
}
