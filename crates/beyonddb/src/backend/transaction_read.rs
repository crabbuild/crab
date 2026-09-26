//! Serializable cross-Cell reads from immutable participant snapshots.

use extenddb_core::types::{Item, TableKeyInfo};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error};
use crate::{
    CoordinatorDecision, CoordinatorParticipantTarget, GetItemInput, Json,
    ReadAccountTransactionResult, ReadCoordinatorParticipant, ReadCoordinatorParticipantInput,
    ReadCrossCellTransaction, ReadPartitionTransactionResult, ReadTransactionInput,
    ReadTransactionResultInput, TransactionFailure, TransactionOperation, TransactionReadResult,
    account_target, coordinator_target, data_target,
};

impl CellStorage {
    pub(super) async fn cross_cell_read(
        &self,
        account_id: &str,
        inputs: Vec<GetItemInput>,
        routing: Vec<(TableKeyInfo, Item)>,
    ) -> Result<Vec<Option<Item>>, StorageError> {
        let count = inputs.len();
        let operations = inputs.into_iter().map(TransactionOperation::Read).collect();
        let admitted = self
            .admit_transaction(account_id, None, operations, routing)
            .await?;
        match admitted.decision {
            CoordinatorDecision::Commit => {}
            CoordinatorDecision::Abort { index, reason } => {
                return Err(super::data::transaction_canceled(
                    usize::from(index.unwrap_or(0)),
                    reason.unwrap_or(TransactionFailure::Conflict),
                    count,
                    &[],
                ));
            }
            CoordinatorDecision::Begin => {
                return Err(StorageError::Transient(
                    "transactional read is undecided".into(),
                ));
            }
        }
        let identity = admitted.identity;
        let coordinator = coordinator_target(account_id, &identity.routing_key)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let status = self
            .client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(identity.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::Internal("read transaction disappeared".into()))?;
        let mut items = vec![None; count];
        for position in 0..status.participant_count {
            let input = ReadCoordinatorParticipantInput {
                account_id: account_id.into(),
                transaction_id: identity.transaction_id,
                routing_key: identity.routing_key.clone(),
                position,
            };
            let participant = self
                .client
                .query::<ReadCoordinatorParticipant>(&coordinator, None, Json(input))
                .await
                .map_err(cell_error)?
                .output
                .0
                .ok_or_else(|| StorageError::Internal("read participant disappeared".into()))?;
            let target = match &participant.target {
                CoordinatorParticipantTarget::Account => account_target(account_id),
                CoordinatorParticipantTarget::Data {
                    table_id,
                    partition_id,
                    ..
                } => data_target(account_id, table_id, partition_id),
            }
            .map_err(|error| StorageError::Internal(error.to_string()))?;
            for (position, operation) in participant.operations.into_iter().enumerate() {
                if !matches!(operation.operation, TransactionOperation::Read(_)) {
                    return Err(StorageError::Internal(
                        "read transaction contains a write".into(),
                    ));
                }
                let input = Json(ReadTransactionResultInput {
                    transaction: ReadTransactionInput {
                        transaction_id: identity.transaction_id,
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                    },
                    position: u8::try_from(position)
                        .map_err(|_| StorageError::Internal("invalid read position".into()))?,
                });
                let image = match participant.target {
                    CoordinatorParticipantTarget::Account => {
                        self.client
                            .query::<ReadAccountTransactionResult>(&target, None, input)
                            .await
                    }
                    CoordinatorParticipantTarget::Data { .. } => {
                        self.client
                            .query::<ReadPartitionTransactionResult>(&target, None, input)
                            .await
                    }
                }
                .map_err(cell_error)?
                .output
                .0;
                let TransactionReadResult::Item(image) = image else {
                    return Err(StorageError::Internal(
                        "committed read image is missing".into(),
                    ));
                };
                let slot = items.get_mut(usize::from(operation.index)).ok_or_else(|| {
                    StorageError::Internal("invalid transaction read index".into())
                })?;
                if slot.replace(image).is_some() {
                    return Err(StorageError::Internal(
                        "duplicate transaction read index".into(),
                    ));
                }
            }
        }
        let items = items
            .into_iter()
            .map(|item| {
                item.ok_or_else(|| {
                    StorageError::Internal("transaction read result is incomplete".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_read_size(items)
    }
}

pub(super) fn validate_read_size(
    items: Vec<Option<Item>>,
) -> Result<Vec<Option<Item>>, StorageError> {
    let size: usize = items
        .iter()
        .flatten()
        .map(extenddb_core::types::item_size_bytes)
        .sum();
    if size > 4 * 1024 * 1024 {
        return Err(StorageError::Validation(
            "transaction read exceeds 4 MiB".into(),
        ));
    }
    Ok(items)
}
