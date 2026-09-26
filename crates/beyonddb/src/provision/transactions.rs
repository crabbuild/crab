//! Restore coordinator and participant owners from durable transaction records.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Bound::{Excluded, Unbounded},
    sync::{Arc, RwLock},
    time::Duration,
};

use crab_cell_host::CellNodeTaskGroup;
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

    pub(super) async fn reclaim_coordinator_capacity(&self) -> Result<(), StorageError> {
        let stats = self.runtime.stats();
        if stats.active_cells() < stats.active_cell_capacity() {
            return Ok(());
        }
        let mut candidates = self
            .runtime
            .idle_transfer_candidates()
            .await
            .map_err(provision_error)?;
        candidates.sort_by_key(|(_, _, last_used, _)| *last_used);
        let candidates = {
            let shards =
                self.transaction_recovery.shards.read().map_err(|_| {
                    StorageError::Internal("coordinator recovery lock poisoned".into())
                })?;
            candidates
                .into_iter()
                .filter_map(|(cell, generation, _, _)| {
                    shards
                        .get(cell.as_bytes())
                        .map(|shard| (cell, generation, shard.target.clone()))
                })
                .collect::<Vec<_>>()
        };
        let client = CellClient::local_runtime(
            self.application.registry(),
            self.runtime.clone(),
            self.layout.clone(),
        );
        for (cell, generation, target) in candidates {
            let pending = client
                .query::<crate::ReadPendingTransactionBoundary>(&target, None, Json(()))
                .await
                .map_err(cell_error)?;
            if pending.output.0.is_some() {
                continue;
            }
            // Runtime rechecks the generation and settled-work gate, closes SQLite,
            // and publishes Idle before returning capacity. A BEGIN racing the query
            // stays recoverable unless the released root proves nothing changed.
            let mut release = self
                .runtime
                .release_idle_cell(cell, self.session, generation)
                .await;
            if matches!(release, Err(crab_cell_runtime::Error::Capacity(_))) {
                // Runtime movement admission replenishes once per second. Let
                // one window pass before failing a foreground multi-Cell admission;
                // the retry still checks generation and settled work atomically.
                tokio::time::sleep(Duration::from_secs(1)).await;
                release = self
                    .runtime
                    .release_idle_cell(cell, self.session, generation)
                    .await;
            }
            release.map_err(|error| match error {
                crab_cell_runtime::Error::Capacity(_) => StorageError::Transient(error.to_string()),
                _ => provision_error(error),
            })?;
            let released = CellAuthority::new(self.layout.clone())
                .load(cell)
                .await
                .map_err(provision_error)?;
            if released.as_ref().is_some_and(|record| {
                let control = record.value();
                control.owner.is_none()
                    && control.incarnation == pending.receipt.incarnation
                    && control
                        .root
                        .as_ref()
                        .is_some_and(|root| root.commit_sequence == pending.receipt.commit_sequence)
            }) {
                // The admission mutex excludes a local reactivation until this
                // retirement finishes. A later admission tracks the shard again.
                self.transaction_recovery
                    .shards
                    .write()
                    .map_err(|_| {
                        StorageError::Internal("coordinator recovery lock poisoned".into())
                    })?
                    .remove(cell.as_bytes());
            }
            return Ok(());
        }
        Err(StorageError::Transient(
            "no settled coordinator can release capacity".into(),
        ))
    }

    async fn activate_tracked_coordinator(
        &self,
        target: &CellTarget,
    ) -> Result<bool, StorageError> {
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("coordinator has no authority".into()))?;
        if let Some(owner) = &observed.value().owner {
            return Ok(owner.session == self.session);
        }
        let proof = self
            .cataloged(target, crate::transaction_coordinator::MODULE)
            .await?;
        self.admit_initialized(target, proof, initialize_coordinator)
            .await?;
        Ok(true)
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
        if !self.activate_tracked_coordinator(&shard.target).await? {
            self.transaction_recovery
                .shards
                .write()
                .map_err(|_| StorageError::Internal("coordinator recovery lock poisoned".into()))?
                .remove(&cell);
            return Ok(());
        }
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
    /// Resolve each shard before admitting the next; private peer routing must
    /// already be available, while public transaction admission remains stopped.
    pub async fn recover_registered_coordinators(
        &self,
        account_id: &str,
        client: &CellClient,
        storage: &CellStorage,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
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
                return Ok(());
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
                // Complete each shard before admitting the next. Registry size
                // must not require all historical coordinators to be resident.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                let local = loop {
                    let result = self
                        .recover_local_transaction_owner(
                            &target,
                            crate::transaction_coordinator::MODULE,
                            initialize_coordinator,
                            nodes,
                        )
                        .await;
                    match result {
                        Err(StorageError::Transient(_))
                            if tokio::time::Instant::now() < deadline =>
                        {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        result => break result?,
                    }
                };
                if local {
                    self.recover_transaction_participants(&target, client, nodes)
                        .await?;
                    storage.recover_fenced_coordinator(&target).await?;
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
            let registered = client
                .query::<crate::ReadCoordinatorRegistration>(&account, None, Json(input.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            // Registration is discovery, not residency. A released shard must
            // restore its published root before token lookup or a new BEGIN.
            let observed = CellAuthority::new(self.layout.clone())
                .load(target.cell_id())
                .await
                .map_err(provision_error)?;
            if registered
                && observed
                    .as_ref()
                    .is_none_or(|record| record.value().root.is_none())
            {
                return Err(StorageError::Transient(
                    "registered coordinator has no published authority".into(),
                ));
            }
            if observed.as_ref().is_some_and(|record| {
                record.value().owner.is_some() && record.value().root.is_some()
            }) {
                // Another node may have published this shard before registration.
                // Keep its authority; the routed client will reach that owner.
                self.cataloged(&target, crate::transaction_coordinator::MODULE)
                    .await?;
                if observed
                    .as_ref()
                    .and_then(|record| record.value().owner.as_ref())
                    .is_some_and(|owner| owner.session == self.session)
                {
                    self.track_coordinator(&target)?;
                }
            } else {
                self.admit_module(
                    &target,
                    crate::transaction_coordinator::MODULE,
                    initialize_coordinator,
                )
                .await?;
            }
            if registered {
                return Ok(());
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
