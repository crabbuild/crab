//! Restore coordinator and participant owners from durable transaction records.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Bound::{Excluded, Unbounded},
    sync::{Arc, RwLock},
    time::Duration,
};

use crab_cell_host::CellNodeTaskGroup;
use crab_cell_runtime::cell::actor::CellHandle;
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::CellTarget;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::partition_for_shard;
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, provision_error, wait_for_expired};
use crate::backend::cell_error;
use crate::{
    CellStorage, CoordinatorParticipantTarget, Json, ListCoordinatorShards,
    ListCoordinatorShardsInput, PendingTransactionCursor, ReadCrossCellTransactionInput,
    ReadPendingCrossCellTransactions, ReadPendingCrossCellTransactionsInput,
    ReadUnresolvedCoordinatorParticipants, account_target, data_target, initialize_account,
    initialize_coordinator, initialize_partition,
};

type Initialize = for<'a> fn(&crab_ltx::rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>;

#[derive(Default)]
pub(super) struct CoordinatorRecovery {
    shards: RwLock<BTreeMap<[u8; 32], RecoveryShard>>,
}

#[derive(Clone)]
struct RecoveryShard {
    target: CellTarget,
    after: Option<PendingTransactionCursor>,
    through: Option<PendingTransactionCursor>,
}

impl CellInitialPartitionProvisioner {
    pub(super) fn track_coordinator(&self, target: &CellTarget) -> Result<(), StorageError> {
        if target.namespace() != crate::transaction_coordinator::NAMESPACE {
            return Ok(());
        }
        self.transaction_recovery
            .shards
            .write()
            .map_err(|_| StorageError::Internal("coordinator recovery lock poisoned".into()))?
            .entry(*target.cell_id().as_bytes())
            .or_insert_with(|| RecoveryShard {
                target: target.clone(),
                after: None,
                through: None,
            });
        Ok(())
    }

    /// Supervise recovery of abandoned transactions on locally admitted coordinators.
    ///
    /// Install once after startup recovery, using the same provisioner as public
    /// admission. Each tick visits one shard and at most one pending transaction.
    /// BEGIN is resumed alongside active requests, never aborted based on age.
    pub fn install_transaction_recovery_loop(
        self: &Arc<Self>,
        tasks: &CellNodeTaskGroup,
        storage: CellStorage,
    ) -> Result<(), StorageError> {
        let provisioner = Arc::clone(self);
        let cancellation = tasks.cancellation_token();
        tasks
            .spawn(async move {
                let mut ticks = tokio::time::interval(Duration::from_millis(250));
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut after = None;
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok::<(), StorageError>(()),
                        _ = ticks.tick() => {}
                    }
                    let result = tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        result = provisioner.recover_next_transaction(&storage, &mut after) => result,
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "transaction recovery deferred");
                    }
                }
            })
            .map_err(|error| StorageError::Internal(error.to_string()))
    }

    async fn recover_next_transaction(
        &self,
        storage: &CellStorage,
        after: &mut Option<[u8; 32]>,
    ) -> Result<(), StorageError> {
        let selected = {
            let shards =
                self.transaction_recovery.shards.read().map_err(|_| {
                    StorageError::Internal("coordinator recovery lock poisoned".into())
                })?;
            after
                .and_then(|cell| shards.range((Excluded(cell), Unbounded)).next())
                .or_else(|| shards.first_key_value())
                .map(|(cell, shard)| (*cell, shard.clone()))
        };
        let Some((cell, mut shard)) = selected else {
            return Ok(());
        };
        // Advance even on failure: an unreachable shard or participant must not
        // prevent unrelated transactions from releasing their locks.
        *after = Some(cell);
        let pending = storage
            .pending_coordinator_transaction(
                &shard.target,
                shard.after.clone(),
                shard.through.clone(),
            )
            .await?;
        // Freeze the pass at a durable cursor, not the host's wall clock. New
        // arrivals must not postpone retries of earlier failed records forever.
        shard.after = pending.as_ref().map(|(entry, _)| entry.cursor.clone());
        shard.through = pending.as_ref().map(|(_, through)| through.clone());
        self.transaction_recovery
            .shards
            .write()
            .map_err(|_| StorageError::Internal("coordinator recovery lock poisoned".into()))?
            .insert(cell, shard);
        if let Some((entry, _)) = pending {
            storage
                .resume_cross_cell_transaction(
                    &entry.account_id,
                    &entry.routing_key,
                    entry.transaction_id,
                )
                .await?;
        }
        Ok(())
    }

    /// Recover registered coordinator shards assigned to this endpoint.
    ///
    /// Call during startup before accepting transaction requests. Idle shards
    /// can be acquired; active shards require the former owner's expired lease.
    pub async fn recover_registered_coordinators(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        nodes: &NodeDirectory,
    ) -> Result<Vec<CellTarget>, StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle);
        let mut recovered = Vec::new();
        let mut after = None;
        loop {
            let page = client
                .query::<ListCoordinatorShards>(
                    &account,
                    None,
                    Json(ListCoordinatorShardsInput {
                        account_id: account_id.to_owned(),
                        after,
                        limit: 100,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            if page.is_empty() {
                return Ok(recovered);
            }
            for shard in page {
                after = Some(shard);
                let target = CellTarget::new(
                    account.tenant(),
                    crate::APPLICATION,
                    crate::transaction_coordinator::NAMESPACE,
                    &partition_for_shard(shard),
                )
                .map_err(provision_error)?;
                if self
                    .recover_local_transaction_owner(
                        &target,
                        crate::transaction_coordinator::MODULE,
                        initialize_coordinator,
                        nodes,
                    )
                    .await?
                {
                    recovered.push(target);
                }
            }
        }
    }

    /// Recover original participant targets before resolving a fenced coordinator.
    ///
    /// Current table routes can omit split sources that still retain transaction
    /// evidence. The coordinator's immutable targets remain the recovery authority.
    pub async fn recover_transaction_participants(
        &self,
        coordinator: &CellTarget,
        client: &CellClient,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        let mut after = None;
        loop {
            let page = client
                .query::<ReadPendingCrossCellTransactions>(
                    coordinator,
                    None,
                    Json(ReadPendingCrossCellTransactionsInput { after, limit: 100 }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            if page.is_empty() {
                return Ok(());
            }
            after = page.last().map(|entry| entry.cursor.clone());
            // Bound deduplication memory to one page, even for a large backlog.
            let mut visited = HashSet::new();
            for entry in page {
                let targets = client
                    .query::<ReadUnresolvedCoordinatorParticipants>(
                        coordinator,
                        None,
                        Json(ReadCrossCellTransactionInput {
                            account_id: entry.account_id.clone(),
                            transaction_id: entry.transaction_id,
                            routing_key: entry.routing_key,
                        }),
                    )
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                for participant in targets {
                    let (target, module, initialize): (CellTarget, &'static str, Initialize) =
                        match participant.target {
                            CoordinatorParticipantTarget::Account => (
                                account_target(&entry.account_id).map_err(provision_error)?,
                                crate::MODULE,
                                initialize_account,
                            ),
                            CoordinatorParticipantTarget::Data {
                                table_id,
                                partition_id,
                                ..
                            } => (
                                data_target(&entry.account_id, &table_id, &partition_id)
                                    .map_err(provision_error)?,
                                crate::DATA_MODULE,
                                initialize_partition,
                            ),
                        };
                    if visited.insert(target.cell_id()) {
                        self.recover_local_transaction_owner(&target, module, initialize, nodes)
                            .await?;
                    }
                }
            }
        }
    }

    async fn recover_local_transaction_owner(
        &self,
        target: &CellTarget,
        module: &'static str,
        initialize: Initialize,
        nodes: &NodeDirectory,
    ) -> Result<bool, StorageError> {
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("transaction Cell has no authority".into()))?;
        let former = observed
            .value()
            .owner
            .as_ref()
            .map(|owner| (owner.session, owner.endpoint.clone()));
        match former {
            Some((session, _)) if session == self.session => {}
            Some((session, endpoint)) if endpoint == self.endpoint => {
                wait_for_expired(nodes, session).await?;
                let proof = self.cataloged(target, module).await?;
                self.takeover_expired(target, proof, nodes).await?;
            }
            None if observed.value().root.is_some() => {
                let proof = self.cataloged(target, module).await?;
                self.admit_initialized(target, proof, initialize).await?;
            }
            _ => return Ok(false),
        }
        self.track_coordinator(target)?;
        Ok(true)
    }
}

impl crate::CoordinatorProvisioner for CellInitialPartitionProvisioner {
    fn ensure<'a>(
        &'a self,
        client: &'a CellClient,
        account_id: &'a str,
        routing_key: &'a [u8],
    ) -> extenddb_storage::BoxedFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            let account = account_target(account_id).map_err(provision_error)?;
            let target =
                crate::coordinator_target(account_id, routing_key).map_err(provision_error)?;
            let shard = u32::from_be_bytes(
                target
                    .partition()
                    .try_into()
                    .map_err(|_| StorageError::Internal("invalid coordinator partition".into()))?,
            );
            let input = crate::RegisterCoordinatorShardInput {
                account_id: account_id.into(),
                shard,
            };
            if client
                .query::<crate::ReadCoordinatorRegistration>(&account, None, Json(input.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0
            {
                return Ok(());
            }
            let observed = CellAuthority::new(self.layout.clone())
                .load(target.cell_id())
                .await
                .map_err(provision_error)?;
            if observed.as_ref().is_some_and(|record| {
                record.value().owner.is_some() && record.value().root.is_some()
            }) {
                // Another node may have published this shard before registration.
                // Keep its authority; the routed client will reach that owner.
                self.cataloged(&target, crate::transaction_coordinator::MODULE)
                    .await?;
            } else {
                self.admit_module(
                    &target,
                    crate::transaction_coordinator::MODULE,
                    initialize_coordinator,
                )
                .await?;
            }
            client
                .command::<crate::RegisterCoordinatorShard>(
                    &account,
                    crate::backend::mutation_identity()?,
                    Json(input),
                )
                .await
                .map_err(cell_error)?;
            Ok(())
        })
    }
}
