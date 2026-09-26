//! Reclaim local slots while retaining published transaction and table history.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use crab_cell_runtime::client::{CellClient, ReadPolicy};
use crab_cell_runtime::identity::{CellId, CellTarget};
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, provision_error};
use crate::backend::cell_error;
use crate::{
    DescribeTableById, Json, ReadGlobalIndexPartition, ReadPartitionState, account_target,
};

impl CellInitialPartitionProvisioner {
    pub(super) async fn reclaim_deleted_ranges(
        &self,
        client: &CellClient,
        account_id: &str,
    ) -> Result<(), StorageError> {
        let _admission = self.admission.lock().await;
        let stats = self.runtime.stats();
        if stats.active_cells() < stats.active_cell_capacity() {
            return Ok(());
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let mut retired = HashSet::new();
        let mut tables = HashMap::new();
        let local = CellClient::local_runtime(
            self.application.registry(),
            self.runtime.clone(),
            self.layout.clone(),
        );
        // Missing metadata is permanent only for this exact generation and
        // only at the account owner. A stale replica cannot authorize release.
        let client = client.clone().with_read_policy(ReadPolicy::CurrentOwner);
        for entry in self
            .runtime
            .active_catalog_entries()
            .await
            .map_err(provision_error)?
        {
            if ![crate::DATA_NAMESPACE, crate::global_index::NAMESPACE].contains(&entry.namespace())
            {
                continue;
            }
            let target = CellTarget::new(
                account.tenant(),
                account.application(),
                entry.namespace(),
                entry.partition(),
            )
            .map_err(provision_error)?;
            if target.cell_id() != entry.cell() {
                continue;
            }
            let table_id = if entry.namespace() == crate::DATA_NAMESPACE {
                local
                    .query::<ReadPartitionState>(&target, None, Json(()))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .map(|state| state.spec.table.id)
            } else {
                local
                    .query::<ReadGlobalIndexPartition>(&target, None, Json(()))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .map(|spec| spec.table.id)
            };
            let Some(table_id) = table_id else {
                continue;
            };
            let present = match tables.get(&table_id) {
                Some(present) => *present,
                None => {
                    let present = client
                        .query::<DescribeTableById>(&account, None, Json(table_id.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                        .is_some();
                    tables.insert(table_id, present);
                    present
                }
            };
            if present {
                continue;
            }
            // Table names may be recreated; IDs never are. Release only residency:
            // original transaction participants retain their roots for recovery.
            retired.insert(entry.cell());
        }
        if retired.is_empty() {
            // Ordinary admission can still reclaim a settled coordinator.
            return Ok(());
        }
        // A just-committed range may await the runtime's inventory refresh.
        // Wait for eligibility; unknown or busy state never authorizes release.
        let candidate = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let candidates = self
                    .runtime
                    .idle_transfer_candidates()
                    .await
                    .map_err(provision_error)?;
                if let Some((cell, generation, _, _)) = candidates
                    .into_iter()
                    .filter(|(cell, _, _, _)| retired.contains(cell))
                    .min_by_key(|(_, _, last_used, _)| *last_used)
                {
                    return Ok::<_, StorageError>((cell, generation));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| StorageError::Transient("deleted table ranges have not settled".into()))??;
        self.release_capacity(candidate.0, candidate.1).await
    }

    pub(super) async fn release_capacity(
        &self,
        cell: CellId,
        generation: u64,
    ) -> Result<(), StorageError> {
        let mut release = self
            .runtime
            .release_idle_cell(cell, self.session, generation)
            .await;
        if matches!(release, Err(crab_cell_runtime::Error::Capacity(_))) {
            // Movement credits replenish each second. Retrying preserves the
            // runtime's generation, settled-work, and authority checks.
            tokio::time::sleep(Duration::from_secs(1)).await;
            release = self
                .runtime
                .release_idle_cell(cell, self.session, generation)
                .await;
        }
        release.map_err(|error| match error {
            crab_cell_runtime::Error::Capacity(_) => StorageError::Transient(error.to_string()),
            _ => provision_error(error),
        })
    }
}
