//! Recoverable host orchestration of one planned data-range split.

use crab_cell_runtime::client::{CellClient, InvocationError};
use extenddb_storage::error::StorageError;

use crate::backend::{cell_error, mutation_identity};
use crate::{
    ActivateImportedPartition, ActivateImportedPartitionInput, ActivateImportedPartitionOutcome,
    CommitSplit, CommitSplitOutcome, ImportPartitionItem, ImportSummary, InstallPartition,
    InstallPartitionOutcome, Json, OpenPartition, OpenPartitionOutcome, PartitionExport,
    PartitionImportInput, PartitionImportOutcome, PartitionInstall, PartitionScanInput,
    PartitionScanOutcome, PartitionSeal, PartitionSpec, PartitionState, ReadPartitionState,
    ReadSplitPlan, ReadSplitRoute, SealPartition, SealPartitionOutcome, SplitPlan, SplitRouteState,
    account_target, data_key_hash, data_target,
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
        Self { client }
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
        let current_plan = self
            .client
            .query::<ReadSplitPlan>(&account, None, Json(source.table.id.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let route_state = self
            .client
            .query::<ReadSplitRoute>(&account, None, Json(plan.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let published_before = current_plan.is_none() && route_state == SplitRouteState::After;
        if !((current_plan.as_ref() == Some(plan) && route_state == SplitRouteState::Before)
            || published_before)
        {
            return Err(StorageError::Transient(
                "split plan or published route changed".into(),
            ));
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
        let sealed = self
            .client
            .command::<SealPartition>(&source_target, mutation_identity()?, Json(seal.clone()))
            .await
            .map_err(cell_error)?;
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
        let published = self
            .client
            .command::<CommitSplit>(&account, mutation_identity()?, Json(plan.clone()))
            .await
            .map_err(cell_error)?;
        if published.output.0 != CommitSplitOutcome::Committed {
            return Err(split_state("route switch did not commit"));
        }
        let visible = self
            .client
            .query::<ReadSplitRoute>(&account, Some(published.receipt), Json(plan.clone()))
            .await
            .map_err(cell_error)?;
        if visible.output.0 != SplitRouteState::After {
            return Err(split_state("published route differs from split plan"));
        }
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
        }
        Ok(())
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
