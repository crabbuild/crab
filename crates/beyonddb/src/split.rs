//! Recoverable host orchestration of one planned data-range split.

use crab_cell_runtime::client::{CellClient, InvocationError, ReadPolicy};
use extenddb_storage::error::StorageError;

use crate::backend::{cell_error, mutation_identity};
use crate::{
    ActivateImportedPartition, ActivateImportedPartitionInput, ActivateImportedPartitionOutcome,
    DirectoryPartitionInput, FinishDirectoryTransfer, ImportPartitionItem, ImportSummary,
    InstallPartition, InstallPartitionOutcome, Json, OpenPartition, OpenPartitionOutcome,
    PartitionExport, PartitionImportInput, PartitionImportOutcome, PartitionInstall,
    PartitionScanInput, PartitionScanOutcome, PartitionSeal, PartitionSpec, PartitionState,
    ReadDirectoryTransfer, ReadPartitionState, SealPartition, SealPartitionOutcome, SplitPlan,
    SplitRouteState, account_target, data_key_hash, data_target,
};

/// Runs a durable split using already admitted account and data Cells.
///
/// The client must reach the account, sealed source, and both child Cells.
/// Repeating `resume` after owner loss is safe because every mutation is
/// idempotent and route publication compares the recorded predecessor.
pub struct CellSplitController {
    client: CellClient,
}

impl CellSplitController {
    /// Bind the routed Cell client used for all split participants.
    pub fn new(client: CellClient) -> Self {
        Self {
            client: client.with_read_policy(ReadPolicy::CurrentOwner),
        }
    }

    /// Finish a previously planned split and open its replacement ranges.
    pub async fn resume(&self, account_id: &str, plan: &SplitPlan) -> Result<(), StorageError> {
        let (source, children, seal) = split_contract(plan)?;
        let account = account_target(account_id).map_err(cell_identity)?;
        let source_target = data_target(account_id, &source.table.id, &source.partition_id)
            .map_err(cell_identity)?;
        let child_targets = [
            data_target(account_id, &children[0].table.id, &children[0].partition_id)
                .map_err(cell_identity)?,
            data_target(account_id, &children[1].table.id, &children[1].partition_id)
                .map_err(cell_identity)?,
        ];
        let directory = crate::route_directory_target(
            &self.client,
            &account,
            &source.table.id,
            source.lower.unwrap_or([0; 16]),
        )
        .await?;
        let current_plan = self
            .client
            .query::<ReadDirectoryTransfer>(
                &directory,
                None,
                Json(DirectoryPartitionInput {
                    table_id: source.table.id.clone(),
                    partition_id: source.partition_id,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0
            .map(SplitPlan::try_from)
            .transpose()
            .map_err(cell_identity)?;
        let route_state =
            crate::split_route_state(&self.client, account_id, &plan.clone().into()).await?;
        let published_before = route_state == SplitRouteState::After;
        // Finish removes intent only after both durable opens. A completed
        // replay must not reacquire a historical source or consume a slot.
        if current_plan.is_none() && published_before {
            return Ok(());
        }
        if current_plan.as_ref() != Some(plan)
            || !matches!(
                route_state,
                SplitRouteState::Before | SplitRouteState::After
            )
        {
            return Err(split_state("split plan or published route changed"));
        }
        for (spec, target) in children.iter().zip(&child_targets) {
            let installed = self
                .client
                .command::<InstallPartition>(
                    target,
                    mutation_identity()?,
                    Json(PartitionInstall::Importing {
                        spec: spec.clone(),
                        source: seal.clone(),
                    }),
                )
                .await
                .map_err(cell_error)?;
            if installed.output.0 != InstallPartitionOutcome::Installed {
                return Err(split_state("child install did not commit"));
            }
        }
        let sealed = match self
            .client
            .command::<SealPartition>(&source_target, mutation_identity()?, Json(seal.clone()))
            .await
        {
            Ok(result) => result,
            // Prepared intents and projection journals must settle before copying.
            // They defer this source without terminating the serving capacity task.
            Err(InvocationError::Rejected(result))
                if matches!(
                    result.output.0,
                    SealPartitionOutcome::InFlightTransaction
                        | SealPartitionOutcome::PendingIndexChanges
                ) =>
            {
                return Err(split_state("split source has unresolved work"));
            }
            Err(error) => return Err(cell_error(error)),
        };
        if sealed.output.0 != SealPartitionOutcome::Sealed {
            return Err(split_state("source seal did not commit"));
        }
        let source_status = self
            .client
            .query::<ReadPartitionState>(&source_target, Some(sealed.receipt), Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if !source_status.is_some_and(|status| {
            status.spec == source && status.state == PartitionState::Sealed(seal.clone())
        }) {
            return Err(split_state("source seal differs from split plan"));
        }
        // Installation contracts remain immutable while index policy changes.
        // Read policy only after sealing; otherwise a concurrent schema change
        // could disappear when the children become the serving ranges.
        let indexes = self
            .client
            .query::<crate::ReadPartitionIndexes>(&source_target, Some(sealed.receipt), Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        for target in &child_targets {
            let inherited = self
                .client
                .command::<crate::InheritPartitionIndexes>(
                    target,
                    mutation_identity()?,
                    Json(crate::InheritPartitionIndexesInput {
                        source: seal.clone(),
                        state: indexes.clone(),
                    }),
                )
                .await
                .map_err(cell_error)?;
            if !inherited.output.0 {
                return Err(split_state("child index policy differs from sealed source"));
            }
        }
        let mut expected = [ImportSummary::default(), ImportSummary::default()];
        let mut cursor = None;
        loop {
            let page = self
                .client
                .query::<PartitionExport>(
                    &source_target,
                    Some(sealed.receipt),
                    Json(PartitionScanInput {
                        index_name: None,
                        table_id: source.table.id.clone(),
                        epoch: source.epoch,
                        limit: Some(100),
                        exclusive_start_key: cursor,
                    }),
                )
                .await
                .map_err(cell_error)?;
            let PartitionScanOutcome::Page {
                items,
                last_evaluated_key,
            } = page.output.0
            else {
                return Err(split_state("sealed source export failed"));
            };
            for item in items {
                let hash = data_key_hash(&source.table.id, &item, &source.table.key_schema)
                    .map_err(cell_identity)?;
                let index = usize::from(hash >= seal.boundary);
                expected[index]
                    .include(&item, &source.table.key_schema)
                    .map_err(cell_identity)?;
                let copied = self
                    .client
                    .command::<ImportPartitionItem>(
                        &child_targets[index],
                        mutation_identity()?,
                        Json(PartitionImportInput {
                            table_id: source.table.id.clone(),
                            epoch: children[index].epoch,
                            item,
                        }),
                    )
                    .await;
                match copied {
                    Ok(result) if result.output.0 == PartitionImportOutcome::Imported => {}
                    Err(InvocationError::Rejected(result))
                        if result.output.0 == PartitionImportOutcome::NotImporting => {}
                    Err(error) => return Err(cell_error(error)),
                    Ok(_) => return Err(split_state("child import did not commit")),
                }
            }
            let Some(next) = last_evaluated_key else {
                break;
            };
            cursor = Some(next);
        }
        for (index, target) in child_targets.iter().enumerate() {
            let activated = self
                .client
                .command::<ActivateImportedPartition>(
                    target,
                    mutation_identity()?,
                    Json(ActivateImportedPartitionInput {
                        source: seal.clone(),
                        expected: expected[index].clone(),
                    }),
                )
                .await
                .map_err(cell_error)?;
            if activated.output.0 != ActivateImportedPartitionOutcome::Activated {
                return Err(split_state("child activation did not commit"));
            }
            let status = self
                .client
                .query::<ReadPartitionState>(target, Some(activated.receipt), Json(()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            if !status.is_some_and(|status| {
                if status.spec != children[index] {
                    return false;
                }
                match status.state {
                    PartitionState::Activated { source, summary } => {
                        source == seal && summary == expected[index]
                    }
                    PartitionState::Opened { source, summary } if published_before => {
                        source == seal && summary == expected[index]
                    }
                    _ => false,
                }
            }) {
                return Err(split_state("child activation differs from source export"));
            }
        }
        crate::publish_directory_transfer(
            &self.client,
            account_id,
            &directory,
            &plan.clone().into(),
        )
        .await?;
        for (index, target) in child_targets.iter().enumerate() {
            let opened = self
                .client
                .command::<OpenPartition>(
                    target,
                    mutation_identity()?,
                    Json(ActivateImportedPartitionInput {
                        source: seal.clone(),
                        expected: expected[index].clone(),
                    }),
                )
                .await
                .map_err(cell_error)?;
            if opened.output.0 != OpenPartitionOutcome::Opened {
                return Err(split_state("child open did not commit"));
            }
            let state = self
                .client
                .query::<ReadPartitionState>(target, Some(opened.receipt), Json(()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            if !state.is_some_and(|status| {
                status.spec == children[index]
                    && status.state
                        == PartitionState::Opened {
                            source: seal.clone(),
                            summary: expected[index].clone(),
                        }
            }) {
                return Err(split_state("child open differs from source export"));
            }
        }
        // The published children retain discovery until both are durably open.
        // Removing the plan earlier strands them after controller loss.
        match self
            .client
            .command::<FinishDirectoryTransfer>(
                &directory,
                mutation_identity()?,
                Json(plan.clone().into()),
            )
            .await
        {
            Ok(result) if result.output.0 => Ok(()),
            Ok(_) | Err(InvocationError::Rejected(_)) => {
                Err(split_state("split completion state changed"))
            }
            Err(error) => Err(cell_error(error)),
        }
    }
}

pub(crate) fn split_contract(
    plan: &SplitPlan,
) -> Result<(PartitionSpec, [PartitionSpec; 2], PartitionSeal), StorageError> {
    let source = plan.source.clone();
    if !plan.valid_for(&source.table) {
        return Err(split_state("split route is invalid"));
    }
    let children = plan.children.clone();
    let boundary = children[0]
        .upper
        .ok_or_else(|| split_state("split boundary is absent"))?;
    let seal = PartitionSeal {
        table_id: source.table.id.clone(),
        source_partition_id: source.partition_id,
        epoch: source.epoch,
        next_epoch: plan
            .next_epoch()
            .ok_or_else(|| split_state("split epoch exhausted"))?,
        source_lower: source.lower,
        source_upper: source.upper,
        boundary,
        left_partition_id: children[0].partition_id,
        right_partition_id: children[1].partition_id,
    };
    Ok((source, children, seal))
}

fn split_state(message: &str) -> StorageError {
    StorageError::Transient(message.into())
}

fn cell_identity(error: crab_cell_runtime::Error) -> StorageError {
    StorageError::Internal(error.to_string())
}
