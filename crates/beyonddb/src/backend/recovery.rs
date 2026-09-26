//! Complete published cross-Cell decisions after a request or owner exits.

use crab_cell_runtime::client::{InvocationError, Receipt};
use crab_cell_runtime::identity::CellTarget;
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, mutation_identity};
use crate::{
    CoordinatorDecision, CoordinatorParticipantTarget, CoordinatorPhaseInput,
    CoordinatorPhaseOutcome, Json, ReadCoordinatorParticipant, ReadCoordinatorParticipantInput,
    ReadCrossCellTransaction, ReadCrossCellTransactionInput, ReadPartitionTransaction,
    ReadPartitionTransactionInput, ReadPartitionTransactionOutcome, RecordParticipantResolution,
    ResolvePartitionTransaction, ResolvePartitionTransactionInput,
    ResolvePartitionTransactionOutcome, coordinator_target, data_target,
};

impl CellStorage {
    /// Finish a published decision across its data Cell participants.
    ///
    /// Returns a retryable error while the decision or any participant outcome
    /// is uncertain. The caller must retry; it must not infer an abort.
    pub async fn finish_decided_cross_cell_transaction(
        &self,
        account_id: &str,
        routing_key: &[u8],
        transaction_id: [u8; 16],
    ) -> Result<(), StorageError> {
        let coordinator = coordinator_target(account_id, routing_key)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let read = || ReadCrossCellTransactionInput {
            account_id: account_id.to_owned(),
            transaction_id,
            routing_key: routing_key.to_vec(),
        };
        let status = self
            .client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::Internal("coordinator transaction is missing".into()))?;
        let commit = match status.decision {
            CoordinatorDecision::Begin => {
                return Err(StorageError::Transient(
                    "cross-Cell transaction has no durable decision".into(),
                ));
            }
            CoordinatorDecision::Commit => true,
            CoordinatorDecision::Abort { .. } => false,
        };
        if status.resolved_count == status.participant_count {
            return Ok(());
        }
        for position in 0..status.participant_count {
            let participant = self
                .client
                .query::<ReadCoordinatorParticipant>(
                    &coordinator,
                    None,
                    Json(ReadCoordinatorParticipantInput {
                        account_id: account_id.to_owned(),
                        transaction_id,
                        routing_key: routing_key.to_vec(),
                        position,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0
                .ok_or_else(|| {
                    StorageError::Internal("coordinator participant is missing".into())
                })?;
            let target = match participant.target {
                CoordinatorParticipantTarget::Account => {
                    return Err(StorageError::Unsupported(
                        "account Cell cross-Cell participant resolution".into(),
                    ));
                }
                CoordinatorParticipantTarget::Data {
                    table_id,
                    partition_id,
                    ..
                } => data_target(account_id, &table_id, &partition_id)
                    .map_err(|error| StorageError::Internal(error.to_string()))?,
            };
            let receipt = self
                .resolve_data_participant(&target, &coordinator, transaction_id, commit)
                .await?;
            let recorded = self
                .client
                .command::<RecordParticipantResolution>(
                    &coordinator,
                    mutation_identity()?,
                    Json(CoordinatorPhaseInput {
                        account_id: account_id.to_owned(),
                        transaction_id,
                        routing_key: routing_key.to_vec(),
                        position,
                        participant_cell: *target.cell_id().as_bytes(),
                        sequence: receipt.commit_sequence,
                    }),
                )
                .await;
            match recorded {
                Ok(committed)
                    if matches!(
                        committed.output.0,
                        CoordinatorPhaseOutcome::Recorded | CoordinatorPhaseOutcome::Replay
                    ) => {}
                Ok(_) | Err(InvocationError::Rejected(_)) => {
                    return Err(StorageError::Internal(
                        "coordinator rejected participant resolution".into(),
                    ));
                }
                Err(error) => return Err(cell_error(error)),
            }
        }
        let final_status = self
            .client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::Internal("coordinator transaction disappeared".into()))?;
        if final_status.resolved_count != final_status.participant_count {
            return Err(StorageError::Transient(
                "cross-Cell participant resolution is incomplete".into(),
            ));
        }
        Ok(())
    }

    async fn resolve_data_participant(
        &self,
        target: &CellTarget,
        coordinator: &CellTarget,
        transaction_id: [u8; 16],
        commit: bool,
    ) -> Result<Receipt, StorageError> {
        let input = ReadPartitionTransactionInput {
            transaction_id,
            coordinator_cell: *coordinator.cell_id().as_bytes(),
        };
        let observed = self
            .client
            .query::<ReadPartitionTransaction>(target, None, Json(input.clone()))
            .await
            .map_err(cell_error)?;
        match (commit, observed.output.0) {
            (true, ReadPartitionTransactionOutcome::Committed)
            | (false, ReadPartitionTransactionOutcome::Aborted) => return Ok(observed.receipt),
            (true, ReadPartitionTransactionOutcome::Prepared)
            | (false, ReadPartitionTransactionOutcome::Prepared)
            | (false, ReadPartitionTransactionOutcome::Missing) => {}
            _ => {
                return Err(StorageError::Internal(
                    "participant state contradicts coordinator decision".into(),
                ));
            }
        }
        let result = self
            .client
            .command::<ResolvePartitionTransaction>(
                target,
                mutation_identity()?,
                Json(ResolvePartitionTransactionInput {
                    transaction_id,
                    coordinator_cell: input.coordinator_cell,
                    commit,
                }),
            )
            .await;
        match result {
            Ok(committed)
                if matches!(
                    (commit, &committed.output.0),
                    (true, ResolvePartitionTransactionOutcome::Committed)
                        | (false, ResolvePartitionTransactionOutcome::Aborted)
                ) =>
            {
                Ok(committed.receipt)
            }
            Ok(_) | Err(InvocationError::Rejected(_)) => Err(StorageError::Internal(
                "participant rejected published transaction decision".into(),
            )),
            Err(InvocationError::Pending(_)) => {
                let observed = self
                    .client
                    .query::<ReadPartitionTransaction>(target, None, Json(input))
                    .await
                    .map_err(cell_error)?;
                if matches!(
                    (commit, observed.output.0),
                    (true, ReadPartitionTransactionOutcome::Committed)
                        | (false, ReadPartitionTransactionOutcome::Aborted)
                ) {
                    Ok(observed.receipt)
                } else {
                    Err(StorageError::Transient(
                        "participant resolution outcome remains pending".into(),
                    ))
                }
            }
            Err(error) => Err(cell_error(error)),
        }
    }
}
