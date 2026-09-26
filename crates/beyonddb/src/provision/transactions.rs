//! Restore coordinator and participant owners from durable transaction records.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    ops::Bound::{Excluded, Unbounded},
    sync::{Arc, RwLock},
    time::Duration,
};

use crab_cell_host::CellNodeTaskGroup;
use crab_cell_runtime::client::{CellClient, Receipt};
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::CellTarget;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::partition_for_shard;
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, provision_error};
use crate::backend::cell_error;
use crate::{
    CellStorage, CoordinatorParticipantTarget, Json, ListCoordinatorShards,
    ListCoordinatorShardsInput, PendingCrossCellTransaction, PendingTransactionCursor,
    ReadCrossCellTransactionInput, ReadPendingCrossCellTransactions,
    ReadPendingCrossCellTransactionsInput, ReadUnresolvedCoordinatorParticipants, account_target,
    data_target, initialize_account, initialize_coordinator, initialize_partition,
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

    /// Discover and recover abandoned transactions for configured accounts.
    ///
    /// Install once after startup recovery, using the same provisioner as public
    /// admission. Each tick discovers one registered shard and visits at most one
    /// pending transaction. Live remote owners remain in place; expired owners
    /// require fenced takeover and local capacity. BEGIN is resumed, never aged out.
    pub fn install_transaction_recovery_loop(
        self: &Arc<Self>,
        tasks: &CellNodeTaskGroup,
        storage: CellStorage,
        nodes: NodeDirectory,
        accounts: Vec<String>,
    ) -> Result<(), StorageError> {
        for account_id in &accounts {
            account_target(account_id).map_err(provision_error)?;
        }
        let mut accounts: VecDeque<_> = accounts.into_iter().map(|id| (id, None)).collect();
        let provisioner = Arc::clone(self);
        let cancellation = tasks.cancellation_token();
        tasks
            .spawn(async move {
                let mut ticks = tokio::time::interval(Duration::from_millis(250));
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut after = None;
                let mut settled = BTreeMap::new();
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok::<(), StorageError>(()),
                        _ = ticks.tick() => {}
                    }
                    if let Some((account, mut cursor)) = accounts.pop_front() {
                        let result = tokio::select! {
                            () = cancellation.cancelled() => return Ok(()),
                            result = provisioner.discover_coordinator(
                                &account, storage.client(), &nodes, &mut cursor, &mut settled,
                            ) => result,
                        };
                        // Rotate accounts even after a failed lookup. Discovery must
                        // neither starve other accounts nor suppress local recovery.
                        accounts.push_back((account, cursor));
                        if let Err(error) = result {
                            tracing::warn!(%error, "coordinator discovery deferred");
                        }
                    }
                    let result = tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        result = provisioner.recover_next_transaction(&storage, &nodes, &mut after) => result,
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "transaction recovery deferred");
                    }
                }
            })
            .map_err(|error| StorageError::Internal(error.to_string()))
    }

    async fn discover_coordinator(
        &self,
        account_id: &str,
        client: &CellClient,
        nodes: &NodeDirectory,
        after: &mut Option<u32>,
        settled: &mut BTreeMap<[u8; 32], Receipt>,
    ) -> Result<(), StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
        let page = client
            .query::<ListCoordinatorShards>(
                &account,
                None,
                Json(ListCoordinatorShardsInput {
                    account_id: account_id.to_owned(),
                    after: *after,
                    limit: 1,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        // Advance before activation, including when capacity or takeover fails.
        // Wrapping an empty page also discovers later registrations behind us.
        *after = page.first().copied();
        let Some(shard) = *after else {
            return Ok(());
        };
        let target = CellTarget::new(
            account.tenant(),
            crate::APPLICATION,
            crate::transaction_coordinator::NAMESPACE,
            &partition_for_shard(shard),
        )
        .map_err(provision_error)?;
        let cell = target.cell_id();
        if let Some(receipt) = settled.get(cell.as_bytes()) {
            let observed = CellAuthority::new(self.layout.clone())
                .load(cell)
                .await
                .map_err(provision_error)?;
            if observed.as_ref().is_some_and(|record| {
                let control = record.value();
                control.owner.is_none()
                    && control.incarnation == receipt.incarnation
                    && control
                        .root
                        .as_ref()
                        .is_some_and(|root| root.commit_sequence == receipt.commit_sequence)
            }) {
                // Only the exact published empty root can skip reactivation.
                // Unknown Idle roots may contain BEGIN or unfinished decisions;
                // repeatedly acquiring completed history would churn scarce slots.
                return Ok(());
            }
        }
        if self
            .recover_discovered_owner(
                &target,
                crate::transaction_coordinator::MODULE,
                initialize_coordinator,
                nodes,
            )
            .await?
        {
            let pending = client
                .query::<crate::ReadPendingTransactionBoundary>(&target, None, Json(()))
                .await
                .map_err(cell_error)?;
            if pending.output.0.is_none() {
                settled.insert(*cell.as_bytes(), pending.receipt);
            } else {
                settled.remove(cell.as_bytes());
            }
        }
        Ok(())
    }

    async fn recover_next_transaction(
        &self,
        storage: &CellStorage,
        nodes: &NodeDirectory,
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
        if !self
            .recover_discovered_owner(
                &shard.target,
                crate::transaction_coordinator::MODULE,
                initialize_coordinator,
                nodes,
            )
            .await?
        {
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
            .insert(cell, shard.clone());
        if let Some((entry, _)) = pending {
            let admission = self
                .recover_pending_participants(
                    &shard.target,
                    &entry,
                    storage.client(),
                    nodes,
                    &mut HashSet::new(),
                )
                .await;
            if entry.state == crate::PendingTransactionState::Begin {
                admission?;
                storage
                    .resume_cross_cell_transaction(
                        &entry.account_id,
                        &entry.routing_key,
                        entry.transaction_id,
                    )
                    .await?;
            } else {
                // A published decision can release healthy participants even if
                // another owner cannot be admitted. Resolution still checks the
                // coordinator and normal Cell authority; admission errors survive.
                let resolution = storage
                    .finish_decided_cross_cell_transaction(
                        &entry.account_id,
                        &entry.routing_key,
                        entry.transaction_id,
                    )
                    .await;
                admission.and(resolution)?;
            }
        }
        Ok(())
    }

    /// Restore registered data and coordinator Cells before serving an account.
    ///
    /// Requires an owned account Cell and available private peer routing. Data
    /// admission errors do not skip coordinator recovery, but still fail readiness.
    pub async fn recover_registered_account(
        &self,
        account_id: &str,
        account: crab_cell_runtime::cell::actor::CellHandle,
        client: &CellClient,
        storage: &CellStorage,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        let admission = self
            .recover_registered_partitions(account_id, account, nodes)
            .await;
        let resolution = self
            .recover_registered_coordinators(account_id, client, storage, nodes)
            .await;
        admission.and(resolution)
    }

    /// Recover idle or expired registered coordinators for a configured account.
    ///
    /// Call during startup before accepting transaction requests. Idle shards
    /// can be acquired; expired owners require a fenced session and Cell CAS.
    /// Live remote owners remain in place, regardless of their endpoint.
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
                        .recover_discovered_owner(
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
                    let admission = self
                        .recover_transaction_participants(&target, client, nodes)
                        .await;
                    // Startup owns a fenced coordinator, so BEGIN may be aborted.
                    // Finish reachable work before failing readiness for any
                    // remaining admission or resolution error.
                    let resolution = storage.recover_fenced_coordinator(&target).await;
                    admission.and(resolution)?;
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
        let mut failure = None;
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
                return failure.map_or(Ok(()), Err);
            }
            after = page.last().map(|entry| entry.cursor.clone());
            // Bound deduplication memory to one page, even for a large backlog.
            let mut visited = HashSet::new();
            for entry in page {
                if let Err(error) = self
                    .recover_pending_participants(coordinator, &entry, client, nodes, &mut visited)
                    .await
                {
                    failure.get_or_insert(error);
                }
            }
        }
    }

    async fn recover_pending_participants(
        &self,
        coordinator: &CellTarget,
        entry: &PendingCrossCellTransaction,
        client: &CellClient,
        nodes: &NodeDirectory,
        visited: &mut HashSet<crab_cell_runtime::identity::CellId>,
    ) -> Result<(), StorageError> {
        let targets = client
            .query::<ReadUnresolvedCoordinatorParticipants>(
                coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: entry.account_id.clone(),
                    transaction_id: entry.transaction_id,
                    routing_key: entry.routing_key.clone(),
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let mut failure = None;
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
            if !visited.contains(&target.cell_id()) {
                match self
                    .recover_discovered_owner(&target, module, initialize, nodes)
                    .await
                {
                    Ok(_) => {
                        visited.insert(target.cell_id());
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
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
