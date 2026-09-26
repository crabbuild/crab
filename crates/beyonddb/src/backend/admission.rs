//! Public transaction admission through one coordinator authority for every destination.

use std::collections::BTreeMap;

use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::identity::CellTarget;
use extenddb_core::types::{Item, TableKeyInfo};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, mutation_identity};
use crate::{
    BeginCrossCellTransaction, BeginCrossCellTransactionInput, BeginCrossCellTransactionOutcome,
    CoordinatorDecision, CoordinatorParticipant, CoordinatorParticipantTarget,
    IndexedTransactionOperation, Json, ReadCoordinatorToken, ReadCoordinatorTokenOutcome,
    ReadCrossCellTransaction, ReadCrossCellTransactionInput, TransactionOperation,
    TransactionToken, coordinator_target,
};

pub(super) struct AdmittedTransaction {
    pub identity: ReadCrossCellTransactionInput,
    pub decision: CoordinatorDecision,
    pub replay: bool,
}

impl CellStorage {
    pub(super) async fn admit_transaction(
        &self,
        account_id: &str,
        token: Option<TransactionToken>,
        operations: Vec<TransactionOperation>,
        routing: Vec<(TableKeyInfo, Item)>,
    ) -> Result<AdmittedTransaction, StorageError> {
        let provisioner = self.coordinators.as_ref().ok_or_else(|| {
            StorageError::Connection("transaction coordinator admission is not configured".into())
        })?;
        let proposed_id = *uuid::Uuid::now_v7().as_bytes();
        let routing_key = token.as_ref().map_or_else(
            || proposed_id.to_vec(),
            |token| token.token.as_bytes().to_vec(),
        );
        provisioner
            .ensure(&self.client, account_id, &routing_key)
            .await?;
        let coordinator = coordinator_target(account_id, &routing_key)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        // Lookup precedes current routes: split children must never replace the
        // immutable participant set of a previously admitted client token.
        if let Some(token) = &token
            && let Some((id, decision)) = self.coordinator_token(&coordinator, token).await?
        {
            return self
                .complete_admission(account_id, routing_key, id, decision)
                .await;
        }
        let mut participants: BTreeMap<[u8; 32], CoordinatorParticipant> = BTreeMap::new();
        for (index, (operation, (info, key))) in operations.into_iter().zip(routing).enumerate() {
            let target = match self.routed_partition(&info, &key).await? {
                Some((partition_id, epoch)) => CoordinatorParticipantTarget::Data {
                    table_id: info.table_id,
                    partition_id,
                    epoch,
                },
                None => CoordinatorParticipantTarget::Account,
            };
            let cell = target
                .cell_id(account_id)
                .map_err(|error| StorageError::Internal(error.to_string()))?;
            let participant = participants
                .entry(cell)
                .or_insert_with(|| CoordinatorParticipant {
                    target: target.clone(),
                    operations: vec![],
                });
            if participant.target != target {
                return Err(StorageError::Transient(
                    "transaction routes changed during admission".into(),
                ));
            }
            let index = u8::try_from(index)
                .map_err(|_| StorageError::Validation("too many transaction operations".into()))?;
            participant
                .operations
                .push(IndexedTransactionOperation { index, operation });
        }
        let input = BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id: proposed_id,
            token: token.clone(),
            participants: participants.into_values().collect(),
        };
        let result = self
            .client
            .command::<BeginCrossCellTransaction>(&coordinator, mutation_identity()?, Json(input))
            .await;
        let (transaction_id, prior) = match result {
            Ok(result) => match result.output.0 {
                BeginCrossCellTransactionOutcome::Begun => {
                    (proposed_id, CoordinatorDecision::Begin)
                }
                BeginCrossCellTransactionOutcome::Existing {
                    transaction_id,
                    decision,
                } => (transaction_id, decision),
                _ => {
                    return Err(StorageError::Internal(
                        "unexpected successful transaction admission".into(),
                    ));
                }
            },
            Err(InvocationError::Rejected(result)) => {
                return Err(match result.output.0 {
                    BeginCrossCellTransactionOutcome::Mismatch => StorageError::IdempotentMismatch,
                    BeginCrossCellTransactionOutcome::InvalidParticipants => {
                        StorageError::Validation("invalid transaction participant set".into())
                    }
                    _ => StorageError::Internal("unexpected rejected transaction admission".into()),
                });
            }
            Err(InvocationError::Pending(_)) => {
                // A lost BEGIN reply can name another request's winning identity.
                // Resolve the token first; absence remains uncertain and retryable.
                let observed = if let Some(token) = &token {
                    self.coordinator_token(&coordinator, token).await?
                } else {
                    self.client
                        .query::<ReadCrossCellTransaction>(
                            &coordinator,
                            None,
                            Json(ReadCrossCellTransactionInput {
                                account_id: account_id.into(),
                                transaction_id: proposed_id,
                                routing_key: routing_key.clone(),
                            }),
                        )
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                        .map(|status| (proposed_id, status.decision))
                };
                observed.ok_or_else(|| {
                    StorageError::Transient("transaction admission outcome remains pending".into())
                })?
            }
            Err(error) => return Err(cell_error(error)),
        };
        self.complete_admission(account_id, routing_key, transaction_id, prior)
            .await
    }

    async fn complete_admission(
        &self,
        account_id: &str,
        routing_key: Vec<u8>,
        transaction_id: [u8; 16],
        prior: CoordinatorDecision,
    ) -> Result<AdmittedTransaction, StorageError> {
        let decision = self
            .resume_cross_cell_transaction(account_id, &routing_key, transaction_id)
            .await?;
        Ok(AdmittedTransaction {
            identity: ReadCrossCellTransactionInput {
                account_id: account_id.into(),
                routing_key,
                transaction_id,
            },
            decision,
            replay: prior == CoordinatorDecision::Commit,
        })
    }

    async fn coordinator_token(
        &self,
        coordinator: &CellTarget,
        token: &TransactionToken,
    ) -> Result<Option<([u8; 16], CoordinatorDecision)>, StorageError> {
        let result = self
            .client
            .query::<ReadCoordinatorToken>(coordinator, None, Json(token.clone()))
            .await
            .map_err(cell_error)?;
        match result.output.0 {
            ReadCoordinatorTokenOutcome::Missing => Ok(None),
            ReadCoordinatorTokenOutcome::Mismatch => Err(StorageError::IdempotentMismatch),
            ReadCoordinatorTokenOutcome::Found {
                transaction_id,
                decision,
            } => Ok(Some((transaction_id, decision))),
        }
    }
}
