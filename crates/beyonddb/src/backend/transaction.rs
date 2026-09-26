//! Resume read and write transactions from immutable coordinator records.

use crab_cell_runtime::client::{InvocationError, Receipt};
use crab_cell_runtime::identity::CellTarget;
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, mutation_identity};
use crate::{
    CoordinatorDecision, CoordinatorParticipantTarget, CoordinatorPhaseInput,
    CoordinatorPhaseOutcome, CrossCellTransactionStatus, DecideCrossCellTransaction,
    DecideCrossCellTransactionInput, DecideCrossCellTransactionOutcome, Json,
    ParticipantTransactionState, PrepareAccountTransaction, PrepareAccountTransactionInput,
    PreparePartitionTransaction, PreparePartitionTransactionInput, PrepareTransactionOutcome,
    ReadCoordinatorParticipant, ReadCoordinatorParticipantInput, ReadCrossCellTransaction,
    ReadCrossCellTransactionInput, ReadTransactionInput, ReadUnresolvedCoordinatorParticipants,
    RecordParticipantPrepare, TransactionFailure, account_target, coordinator_target, data_target,
};

impl CellStorage {
    /// Drive an admitted transaction using its durable participant payloads.
    ///
    /// The coordinator must already contain a published BEGIN. Returns its
    /// terminal decision only after every participant resolution is published.
    /// Unresolved transport ambiguity leaves work pending and returns a retryable error.
    pub async fn resume_cross_cell_transaction(
        &self,
        account_id: &str,
        routing_key: &[u8],
        transaction_id: [u8; 16],
    ) -> Result<CoordinatorDecision, StorageError> {
        let coordinator = coordinator_target(account_id, routing_key)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let read = ReadCrossCellTransactionInput {
            account_id: account_id.to_owned(),
            transaction_id,
            routing_key: routing_key.to_vec(),
        };
        if self.transaction_status(&coordinator, &read).await?.decision
            != CoordinatorDecision::Begin
        {
            return self.finish_transaction(&coordinator, &read).await;
        }
        let participants = self
            .client
            .query::<ReadUnresolvedCoordinatorParticipants>(&coordinator, None, Json(read.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        for participant in participants {
            let payload = self
                .client
                .query::<ReadCoordinatorParticipant>(
                    &coordinator,
                    None,
                    Json(ReadCoordinatorParticipantInput {
                        account_id: read.account_id.clone(),
                        transaction_id,
                        routing_key: read.routing_key.clone(),
                        position: participant.position,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0
                .ok_or_else(|| {
                    StorageError::Internal("transaction participant payload is missing".into())
                })?;
            let operations = payload
                .operations
                .iter()
                .map(|operation| operation.operation.clone())
                .collect();
            let coordinator_cell = *coordinator.cell_id().as_bytes();
            let (target, input) = match &payload.target {
                CoordinatorParticipantTarget::Account => (
                    account_target(account_id),
                    ParticipantPrepare::Account(PrepareAccountTransactionInput {
                        transaction_id,
                        coordinator_cell,
                        coordinator_key: read.routing_key.clone(),
                        operations,
                    }),
                ),
                CoordinatorParticipantTarget::Data {
                    table_id,
                    partition_id,
                    epoch,
                } => (
                    data_target(account_id, table_id, partition_id),
                    ParticipantPrepare::Data(PreparePartitionTransactionInput {
                        table_id: table_id.clone(),
                        epoch: *epoch,
                        transaction_id,
                        coordinator_cell,
                        coordinator_key: read.routing_key.clone(),
                        operations,
                    }),
                ),
            };
            let target = target.map_err(|error| StorageError::Internal(error.to_string()))?;
            let (outcome, receipt) = self.prepare_transaction_participant(&target, input).await?;
            let rejection = match outcome {
                PrepareTransactionOutcome::Prepared | PrepareTransactionOutcome::Replay => None,
                PrepareTransactionOutcome::Rejected { index, reason } => Some((index, reason)),
                PrepareTransactionOutcome::NotInstalled
                | PrepareTransactionOutcome::StaleRoute
                | PrepareTransactionOutcome::Sealed
                | PrepareTransactionOutcome::NotReady
                | PrepareTransactionOutcome::WrongPartition => {
                    Some((0, TransactionFailure::Conflict))
                }
                PrepareTransactionOutcome::Committed | PrepareTransactionOutcome::Aborted => {
                    // A concurrent driver/recovery owner may have finished. Only
                    // the coordinator can decide which terminal outcome to return.
                    return self.finish_transaction(&coordinator, &read).await;
                }
                PrepareTransactionOutcome::Mismatch => {
                    return Err(StorageError::Internal(
                        "participant transaction identity mismatch".into(),
                    ));
                }
            };
            if let Some((index, reason)) = rejection {
                let operation = payload.operations.get(index).ok_or_else(|| {
                    StorageError::Internal("participant returned invalid operation index".into())
                })?;
                let decision = CoordinatorDecision::Abort {
                    index: Some(operation.index),
                    reason: Some(reason),
                };
                return self.decide_transaction(&coordinator, &read, decision).await;
            }
            let recorded = self
                .client
                .command::<RecordParticipantPrepare>(
                    &coordinator,
                    mutation_identity()?,
                    Json(CoordinatorPhaseInput {
                        account_id: read.account_id.clone(),
                        transaction_id,
                        routing_key: read.routing_key.clone(),
                        position: participant.position,
                        participant_cell: *target.cell_id().as_bytes(),
                        sequence: receipt.commit_sequence,
                    }),
                )
                .await;
            match recorded {
                Ok(result)
                    if matches!(
                        result.output.0,
                        CoordinatorPhaseOutcome::Recorded | CoordinatorPhaseOutcome::Replay
                    ) => {}
                Err(InvocationError::Rejected(result))
                    if result.output.0 == CoordinatorPhaseOutcome::WrongDecision =>
                {
                    return self.finish_transaction(&coordinator, &read).await;
                }
                Ok(_) | Err(InvocationError::Rejected(_)) => {
                    return Err(StorageError::Internal(
                        "coordinator refused prepare evidence".into(),
                    ));
                }
                Err(error) => return Err(cell_error(error)),
            }
        }
        self.decide_transaction(&coordinator, &read, CoordinatorDecision::Commit)
            .await
    }

    async fn transaction_status(
        &self,
        coordinator: &CellTarget,
        read: &ReadCrossCellTransactionInput,
    ) -> Result<CrossCellTransactionStatus, StorageError> {
        self.client
            .query::<ReadCrossCellTransaction>(coordinator, None, Json(read.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::Internal("coordinator transaction is missing".into()))
    }

    async fn finish_transaction(
        &self,
        coordinator: &CellTarget,
        read: &ReadCrossCellTransactionInput,
    ) -> Result<CoordinatorDecision, StorageError> {
        let status = self.transaction_status(coordinator, read).await?;
        if status.decision == CoordinatorDecision::Begin {
            return Err(StorageError::Transient(
                "transaction decision remains pending".into(),
            ));
        }
        self.finish_decided_cross_cell_transaction(
            &read.account_id,
            &read.routing_key,
            read.transaction_id,
        )
        .await?;
        Ok(status.decision)
    }

    async fn decide_transaction(
        &self,
        coordinator: &CellTarget,
        read: &ReadCrossCellTransactionInput,
        decision: CoordinatorDecision,
    ) -> Result<CoordinatorDecision, StorageError> {
        let result = self
            .client
            .command::<DecideCrossCellTransaction>(
                coordinator,
                mutation_identity()?,
                Json(DecideCrossCellTransactionInput {
                    account_id: read.account_id.clone(),
                    transaction_id: read.transaction_id,
                    routing_key: read.routing_key.clone(),
                    decision,
                }),
            )
            .await;
        match result {
            Ok(result)
                if matches!(
                    result.output.0,
                    DecideCrossCellTransactionOutcome::Decided(_)
                ) => {}
            Err(InvocationError::Rejected(result))
                if result.output.0 == DecideCrossCellTransactionOutcome::DecisionConflict => {}
            // A lost reply may conceal a commit; read the authoritative state.
            Err(InvocationError::Pending(_)) => {}
            Ok(_) | Err(InvocationError::Rejected(_)) => {
                return Err(StorageError::Internal(
                    "coordinator refused transaction decision".into(),
                ));
            }
            Err(error) => return Err(cell_error(error)),
        }
        self.finish_transaction(coordinator, read).await
    }

    async fn prepare_transaction_participant(
        &self,
        target: &CellTarget,
        input: ParticipantPrepare,
    ) -> Result<(PrepareTransactionOutcome, Receipt), StorageError> {
        let read = match &input {
            ParticipantPrepare::Account(input) => ReadTransactionInput {
                transaction_id: input.transaction_id,
                coordinator_cell: input.coordinator_cell,
            },
            ParticipantPrepare::Data(input) => ReadTransactionInput {
                transaction_id: input.transaction_id,
                coordinator_cell: input.coordinator_cell,
            },
        };
        let prior = self.participant_state(target, read.clone()).await?;
        match prior.output.0 {
            ParticipantTransactionState::Committed => {
                return Ok((PrepareTransactionOutcome::Committed, prior.receipt));
            }
            ParticipantTransactionState::Aborted => {
                return Ok((PrepareTransactionOutcome::Aborted, prior.receipt));
            }
            ParticipantTransactionState::CoordinatorMismatch => {
                return Ok((PrepareTransactionOutcome::Mismatch, prior.receipt));
            }
            // Re-submit prepared payloads to verify their immutable digest. The
            // application record supplies idempotency beyond the runtime ledger.
            ParticipantTransactionState::Prepared | ParticipantTransactionState::Missing => {}
        }
        let identity = mutation_identity()?;
        let result = match input {
            ParticipantPrepare::Account(input) => {
                self.client
                    .command::<PrepareAccountTransaction>(target, identity, Json(input))
                    .await
            }
            ParticipantPrepare::Data(input) => {
                self.client
                    .command::<PreparePartitionTransaction>(target, identity, Json(input))
                    .await
            }
        };
        match result {
            Ok(result) => Ok((result.output.0, result.receipt)),
            Err(InvocationError::Rejected(result)) => Ok((result.output.0, result.receipt)),
            Err(InvocationError::Pending(_)) => {
                let observed = self.participant_state(target, read).await?;
                let outcome = match observed.output.0 {
                    ParticipantTransactionState::Prepared => PrepareTransactionOutcome::Replay,
                    ParticipantTransactionState::Committed => PrepareTransactionOutcome::Committed,
                    ParticipantTransactionState::Aborted => PrepareTransactionOutcome::Aborted,
                    ParticipantTransactionState::CoordinatorMismatch => {
                        PrepareTransactionOutcome::Mismatch
                    }
                    ParticipantTransactionState::Missing => {
                        return Err(StorageError::Transient(
                            "participant prepare outcome remains pending".into(),
                        ));
                    }
                };
                Ok((outcome, observed.receipt))
            }
            Err(error) => Err(cell_error(error)),
        }
    }
}

enum ParticipantPrepare {
    Account(PrepareAccountTransactionInput),
    Data(PreparePartitionTransactionInput),
}
