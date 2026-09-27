//! Recoverable index range transfer and directory cutover through routed Cells.

use crab_cell_runtime::{
    cell::catalog::CellCatalog,
    client::{CellClient, InvocationError, ReadPolicy},
};
use extenddb_storage::{BoxedFuture, error::StorageError};

use super::{CellInitialPartitionProvisioner, provision_error, split_boundary};
use crate::backend::{cell_error, mutation_identity};
use crate::{
    ActivateGlobalIndexImport, BeginDirectoryTransfer, DirectoryPartitionInput,
    ExportGlobalIndexEntries, FinishDirectoryTransfer, GlobalIndexApplyOutcome, GlobalIndexExport,
    GlobalIndexFingerprint, GlobalIndexImport, GlobalIndexImportComplete, GlobalIndexSplitPlan,
    GlobalIndexState, ImportGlobalIndexEntry, Json, OpenGlobalIndexImport, PrepareGlobalIndexSplit,
    PublishDirectoryTransfer, ReadDirectoryRange, ReadDirectoryTransfer, ReadGlobalIndexPartition,
    ReadGlobalIndexState, SplitRouteState, account_target, data_key_hash, global_index_target,
};

fn changed() -> StorageError {
    StorageError::Transient("global-index split state changed; retry".into())
}

fn committed(
    result: Result<crab_cell_runtime::client::Committed<Json<bool>>, InvocationError<Json<bool>>>,
) -> Result<(), StorageError> {
    match result {
        Ok(result) if result.output.0 => Ok(()),
        Ok(_) | Err(InvocationError::Rejected(_)) => Err(changed()),
        Err(error) => Err(cell_error(error)),
    }
}

impl CellInitialPartitionProvisioner {
    /// Resume a pending source/child transfer or split an index range over its page budget.
    pub async fn split_global_index_if_over_database_bytes(
        &self,
        account_id: &str,
        client: CellClient,
        index_id: &str,
        partition_id: [u8; 16],
        lower: [u8; 16],
        max_database_bytes: u64,
    ) -> Result<Option<GlobalIndexSplitPlan>, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let client = client.with_read_policy(ReadPolicy::CurrentOwner);
        let account = account_target(account_id).map_err(provision_error)?;
        let directory = crate::route_directory_target(&client, &account, index_id, lower).await?;
        let pending = client
            .query::<ReadDirectoryTransfer>(
                &directory,
                None,
                Json(DirectoryPartitionInput {
                    table_id: index_id.to_owned(),
                    partition_id,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        if let Some(plan) = pending {
            let plan = GlobalIndexSplitPlan::try_from(plan).map_err(provision_error)?;
            self.resume_global_index_split(account_id, client, &plan)
                .await?;
            return Ok(Some(plan));
        }
        let target =
            global_index_target(account_id, index_id, &partition_id).map_err(provision_error)?;
        let bytes = client
            .query::<crate::GlobalIndexUsage>(&target, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if bytes <= max_database_bytes {
            return Ok(None);
        }
        self.split_global_index_partition(account_id, client, index_id, partition_id, lower)
            .await
            .map(Some)
    }

    /// Plan or resume a split using the current index range and durable participant reservation.
    pub fn split_global_index_partition<'a>(
        &'a self,
        account_id: &'a str,
        client: CellClient,
        index_id: &'a str,
        partition_id: [u8; 16],
        lower: [u8; 16],
    ) -> BoxedFuture<'a, Result<GlobalIndexSplitPlan, StorageError>> {
        Box::pin(async move {
            let client = client.with_read_policy(ReadPolicy::CurrentOwner);
            let account = account_target(account_id).map_err(provision_error)?;
            let directory =
                crate::route_directory_target(&client, &account, index_id, lower).await?;
            let input = DirectoryPartitionInput {
                table_id: index_id.to_owned(),
                partition_id,
            };
            if let Some(plan) = client
                .query::<ReadDirectoryTransfer>(&directory, None, Json(input.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0
            {
                let plan = GlobalIndexSplitPlan::try_from(plan).map_err(provision_error)?;
                self.resume_global_index_split(account_id, client, &plan)
                    .await?;
                return Ok(plan);
            }
            let published = client
                .query::<ReadDirectoryRange>(&directory, None, Json(input))
                .await
                .map_err(cell_error)?
                .output
                .0;
            let target = global_index_target(account_id, index_id, &partition_id)
                .map_err(provision_error)?;
            let Some(published) = published else {
                let state = client
                    .query::<ReadGlobalIndexState>(&target, None, Json(()))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                let Some(GlobalIndexState::Sealed(plan)) = state else {
                    return Err(changed());
                };
                self.resume_global_index_split(account_id, client, &plan)
                    .await?;
                return Ok(plan);
            };
            let source = client
                .query::<ReadGlobalIndexPartition>(&target, None, Json(()))
                .await
                .map_err(cell_error)?
                .output
                .0
                .ok_or_else(changed)?;
            if source.index.id != index_id
                || source.partition_id != partition_id
                || source.lower.unwrap_or([0; 16]) != published.lower
                || source.upper != published.upper
                || source.epoch != published.epoch
            {
                return Err(changed());
            }
            let directory = self
                .split_ready_directory(&client, account_id, index_id, lower)
                .await?;
            let boundary = split_boundary(source.lower, source.upper)?;
            let epoch = published.epoch.checked_add(1).ok_or_else(|| {
                StorageError::LimitExceeded("index directory epoch exhausted".into())
            })?;
            let mut children = [source.clone(), source.clone()];
            children[0].partition_id = *uuid::Uuid::now_v7().as_bytes();
            children[0].upper = Some(boundary);
            children[0].epoch = epoch;
            children[1].partition_id = *uuid::Uuid::now_v7().as_bytes();
            children[1].lower = Some(boundary);
            children[1].epoch = epoch;
            let plan = GlobalIndexSplitPlan {
                source,
                children,
                expected_epoch: published.epoch,
            };
            committed(
                client
                    .command::<BeginDirectoryTransfer>(
                        &directory,
                        mutation_identity()?,
                        Json(plan.clone().into()),
                    )
                    .await,
            )?;
            self.resume_global_index_split(account_id, client, &plan)
                .await?;
            Ok(plan)
        })
    }

    /// Resume a recorded index split through copying, publication, and verified opening.
    pub fn resume_global_index_split<'a>(
        &'a self,
        account_id: &'a str,
        client: CellClient,
        plan: &'a GlobalIndexSplitPlan,
    ) -> BoxedFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            if !plan.valid() {
                return Err(changed());
            }
            let client = client.with_read_policy(ReadPolicy::CurrentOwner);
            let account = account_target(account_id).map_err(provision_error)?;
            let index = &plan.source.index;
            let directory = crate::route_directory_target(
                &client,
                &account,
                &index.id,
                plan.source.lower.unwrap_or([0; 16]),
            )
            .await?;
            let pending = client
                .query::<ReadDirectoryTransfer>(
                    &directory,
                    None,
                    Json(DirectoryPartitionInput {
                        table_id: index.id.clone(),
                        partition_id: plan.source.partition_id,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0
                .map(GlobalIndexSplitPlan::try_from)
                .transpose()
                .map_err(provision_error)?;
            let state = crate::split_route_state(&client, account_id, &plan.clone().into()).await?;
            // Finish is durable proof that the exact replacement ranges opened.
            // Completed replay needs no historical source or admission capacity.
            if pending.is_none() && state == SplitRouteState::After {
                return Ok(());
            }
            if pending.as_ref() != Some(plan)
                || !matches!(state, SplitRouteState::Before | SplitRouteState::After)
            {
                return Err(changed());
            }
            let source = global_index_target(account_id, &index.id, &plan.source.partition_id)
                .map_err(provision_error)?;
            let children = [
                global_index_target(account_id, &index.id, &plan.children[0].partition_id)
                    .map_err(provision_error)?,
                global_index_target(account_id, &index.id, &plan.children[1].partition_id)
                    .map_err(provision_error)?,
            ];
            if CellCatalog::new(self.layout.clone(), source.tenant())
                .lookup(source.cell_id())
                .await
                .map_err(provision_error)?
                .is_none()
            {
                return Err(StorageError::Transient(
                    "index split source is not cataloged".into(),
                ));
            }
            for target in children.iter().chain([&source]) {
                self.reclaim_retired_ranges(&client, &account, Some(source.cell_id()))
                    .await?;
                self.provision_range(target, &client).await?;
                committed(
                    client
                        .command::<PrepareGlobalIndexSplit>(
                            target,
                            mutation_identity()?,
                            Json(plan.clone()),
                        )
                        .await,
                )?;
            }
            let mut expected = [
                GlobalIndexFingerprint::default(),
                GlobalIndexFingerprint::default(),
            ];
            let mut after = None;
            loop {
                let page = client
                    .query::<ExportGlobalIndexEntries>(
                        &source,
                        None,
                        Json(GlobalIndexExport {
                            plan: plan.clone(),
                            after,
                        }),
                    )
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .ok_or_else(changed)?;
                for entry in page.entries {
                    let hash =
                        data_key_hash(&index.id, &entry.key, &index.specification.key_schema)
                            .map_err(provision_error)?;
                    let child = usize::from(hash >= plan.children[1].lower.ok_or_else(changed)?);
                    expected[child]
                        .include(&entry, &plan.children[child])
                        .map_err(provision_error)?;
                    match client
                        .command::<ImportGlobalIndexEntry>(
                            &children[child],
                            mutation_identity()?,
                            Json(GlobalIndexImport {
                                plan: plan.clone(),
                                entry,
                            }),
                        )
                        .await
                    {
                        Ok(result)
                            if matches!(
                                result.output.0,
                                GlobalIndexApplyOutcome::Applied | GlobalIndexApplyOutcome::Replay
                            ) => {}
                        // A concurrent resumer can finish this immutable import.
                        // Activation below must still match the complete source fingerprint.
                        Err(InvocationError::Rejected(result))
                            if result.output.0 == GlobalIndexApplyOutcome::StaleRoute => {}
                        Err(error) => return Err(cell_error(error)),
                        Ok(_) => return Err(changed()),
                    }
                }
                let Some(next) = page.next else {
                    break;
                };
                after = Some(next);
            }
            for (i, target) in children.iter().enumerate() {
                committed(
                    client
                        .command::<ActivateGlobalIndexImport>(
                            target,
                            mutation_identity()?,
                            Json(GlobalIndexImportComplete {
                                plan: plan.clone(),
                                expected: expected[i].clone(),
                            }),
                        )
                        .await,
                )?;
            }
            committed(
                client
                    .command::<PublishDirectoryTransfer>(
                        &directory,
                        mutation_identity()?,
                        Json(plan.clone().into()),
                    )
                    .await,
            )?;
            if crate::split_route_state(&client, account_id, &plan.clone().into()).await?
                != SplitRouteState::After
            {
                return Err(changed());
            }
            for (i, target) in children.iter().enumerate() {
                committed(
                    client
                        .command::<OpenGlobalIndexImport>(
                            target,
                            mutation_identity()?,
                            Json(GlobalIndexImportComplete {
                                plan: plan.clone(),
                                expected: expected[i].clone(),
                            }),
                        )
                        .await,
                )?;
                let state = client
                    .query::<ReadGlobalIndexState>(target, None, Json(()))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                if state
                    != Some(GlobalIndexState::Opened {
                        plan: plan.clone(),
                        summary: expected[i].clone(),
                    })
                {
                    return Err(changed());
                }
            }
            // Retain the participant lookup through publication and both opens.
            // A restarted sweep can discover the unfinished plan through a child.
            committed(
                client
                    .command::<FinishDirectoryTransfer>(
                        &directory,
                        mutation_identity()?,
                        Json(plan.clone().into()),
                    )
                    .await,
            )
        })
    }
}
