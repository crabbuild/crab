//! Restore coordinator and participant owners from durable transaction records.

use std::collections::HashSet;

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
    CoordinatorParticipantTarget, Json, ListCoordinatorShards, ListCoordinatorShardsInput,
    ReadCrossCellTransactionInput, ReadPendingCrossCellTransactions,
    ReadPendingCrossCellTransactionsInput, ReadUnresolvedCoordinatorParticipants, account_target,
    data_target, initialize_account, initialize_coordinator, initialize_partition,
};

type Initialize = for<'a> fn(&crab_ltx::rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>;

impl CellInitialPartitionProvisioner {
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
        Ok(true)
    }
}
