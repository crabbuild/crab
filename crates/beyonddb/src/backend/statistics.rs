//! Bounded sampling and publication of table and index statistics.

use std::{collections::VecDeque, time::Duration};

use crab_cell_host::CellNodeTaskGroup;
use crab_cell_runtime::client::InvocationError;
use extenddb_core::types::{TableDescription, TableStatus};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, description, mutation_identity, target};
use crate::statistics::{
    PublishStatistics, ReadAccountStatistics, ReadGlobalIndexStatistics, ReadPartitionStatistics,
    ReadTableStatistics, StatisticsSnapshot, TableStatistics,
};
use crate::{
    Json, ListTables, ListTablesInput, ListTablesOutcome, TableRecord, data_target,
    global_index_target,
};

struct Sweep {
    table: TableRecord,
    snapshot: StatisticsSnapshot,
    index: Option<usize>,
    after: Option<[u8; 16]>,
    leaf: Option<(crate::DirectorySpec, u64, TableStatistics)>,
}

impl Sweep {
    fn new(table: TableRecord) -> Result<Self, StorageError> {
        Ok(Self {
            snapshot: StatisticsSnapshot {
                table_id: table.id.clone(),
                sampled_at: mutation_identity()?.issued_at_ms,
                base_routed: matches!(table.placement, crate::TablePlacement::Routed { .. }),
                index_generations: Default::default(),
                statistics: TableStatistics::default(),
            },
            table,
            index: None,
            after: None,
            leaf: None,
        })
    }

    async fn step(
        &mut self,
        storage: &CellStorage,
        account_id: &str,
    ) -> Result<bool, StorageError> {
        let account = target(account_id)?;
        if self.index.is_none() && self.table.placement == crate::TablePlacement::Account {
            self.snapshot.statistics = storage
                .client
                .query::<ReadAccountStatistics>(&account, None, Json(self.table.id.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0;
        } else if !self.sample_directory(storage, account_id).await? {
            return Ok(false);
        }
        let next = self.index.map_or(0, |index| index + 1);
        if next < self.table.global_secondary_indexes.len() {
            self.index = Some(next);
            self.after = None;
            return Ok(false);
        }
        match storage
            .client
            .command::<PublishStatistics>(
                &account,
                mutation_identity()?,
                Json(self.snapshot.clone()),
            )
            .await
        {
            Ok(result) if result.output.0 => Ok(true),
            Ok(_) | Err(InvocationError::Rejected(_)) => Err(changed()),
            Err(error) => Err(cell_error(error)),
        }
    }
    async fn sample_directory(
        &mut self,
        storage: &CellStorage,
        account_id: &str,
    ) -> Result<bool, StorageError> {
        let index = self
            .index
            .map(|position| &self.table.global_secondary_indexes[position]);
        let id = index.map_or(&self.table.id, |index| &index.id);
        let account = target(account_id)?;
        if storage
            .client
            .query::<crate::ReadRouteDirectory>(&account, None, Json(id.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .is_none()
        {
            return Err(changed());
        }
        let hash = self.after.unwrap_or([0; 16]);
        let page = crate::read_directory_leaf(&storage.client, account.tenant(), id, hash).await?;
        let (spec, version, values) = self
            .leaf
            .get_or_insert_with(|| (page.spec.clone(), page.version, Default::default()));
        if *spec != page.spec || *version != page.version {
            return Err(changed());
        }
        let range = page
            .ranges
            .first()
            .filter(|range| range.lower == hash)
            .ok_or_else(changed)?;
        if let Some(index) = index {
            let owner = global_index_target(account_id, id, &range.partition_id)
                .map_err(|error| StorageError::Internal(error.to_string()))?;
            let sampled = storage
                .client
                .query::<ReadGlobalIndexStatistics>(&owner, None, Json(()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            values
                .global
                .entry(index.specification.index_name.clone())
                .or_default()
                .add(&sampled)
                .map_err(|error| StorageError::Internal(error.to_string()))?;
        } else {
            let owner = data_target(account_id, id, &range.partition_id)
                .map_err(|error| StorageError::Internal(error.to_string()))?;
            let sampled = storage
                .client
                .query::<ReadPartitionStatistics>(&owner, None, Json(id.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            add_statistics(values, &sampled)?;
        }
        self.after = range.upper;
        if range.upper != spec.upper {
            return Ok(false);
        }
        let leaf = crate::directory_target(account_id, spec)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let checked = storage
            .client
            .query::<crate::ReadDirectoryPage>(
                &leaf,
                None,
                Json(crate::DirectoryPageInput {
                    hash: spec.lower,
                    expected_version: Some(*version),
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        if !matches!(checked, crate::DirectoryPage::Leaf { .. }) {
            return Err(changed());
        }
        // Validate membership at its owning leaf, then leave this immutable interval.
        // Later splits behind the bound cannot count a source and its children twice.
        add_statistics(&mut self.snapshot.statistics, values)?;
        self.leaf = None;
        if self.after.is_some() {
            return Ok(false);
        }
        if let Some(index) = index {
            self.snapshot.index_generations.insert(index.id.clone());
        }
        Ok(true)
    }
}

fn add_statistics(
    total: &mut TableStatistics,
    sample: &TableStatistics,
) -> Result<(), StorageError> {
    let mut add = || -> crate::Result<()> {
        total.base.add(&sample.base)?;
        for (name, value) in &sample.local {
            total.local.entry(name.clone()).or_default().add(value)?;
        }
        for (name, value) in &sample.global {
            total.global.entry(name.clone()).or_default().add(value)?;
        }
        Ok(())
    };
    add().map_err(|error| StorageError::Internal(error.to_string()))
}

fn changed() -> StorageError {
    StorageError::Transient("table route changed during statistics sampling".into())
}

impl CellStorage {
    pub(super) async fn refresh_statistics(
        &self,
        account: &str,
        name: &str,
    ) -> Result<(), StorageError> {
        let storage = CellStorage::new(self.client.clone(), self.region.clone());
        let mut sweep = Sweep::new(storage.record(account, name).await?)?;
        while !sweep.step(&storage, account).await? {}
        Ok(())
    }

    pub(super) async fn statistics(
        &self,
        account: &str,
        table_id: &str,
    ) -> Result<TableStatistics, StorageError> {
        Ok(self
            .client
            .query::<ReadTableStatistics>(&target(account)?, None, Json(table_id.into()))
            .await
            .map_err(cell_error)?
            .output
            .0)
    }

    pub(super) async fn table_description(
        &self,
        record: TableRecord,
        account: &str,
        status: TableStatus,
    ) -> Result<TableDescription, StorageError> {
        let statistics = self.statistics(account, &record.id).await?;
        let mut result = description(record, account, &self.region, status);
        apply(&mut result, statistics);
        Ok(result)
    }

    /// Sample one table range per tick and durably publish completed table statistics.
    ///
    /// Route changes discard the partial sample. Completed snapshots survive owner
    /// restart; interrupted sampling restarts from the table's first range.
    pub fn install_statistics_loop(
        &self,
        tasks: &CellNodeTaskGroup,
        accounts: Vec<String>,
    ) -> Result<(), StorageError> {
        for account in &accounts {
            target(account)?;
        }
        let storage = CellStorage::new(self.client.clone(), self.region.clone());
        let mut accounts: VecDeque<_> = accounts
            .into_iter()
            .map(|account| (account, None, None::<Sweep>))
            .collect();
        let cancellation = tasks.cancellation_token();
        tasks.spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(250));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return Ok::<(), StorageError>(()),
                    _ = ticks.tick() => {}
                }
                let Some((account, mut after, mut sweep)) = accounts.pop_front() else { continue; };
                let result = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    result = async {
                        if sweep.is_none() {
                            let page = storage.client.query::<ListTables>(&target(&account)?, None,
                                Json(ListTablesInput { limit: 1, exclusive_start: after.clone() }))
                                .await.map_err(cell_error)?.output.0;
                            let ListTablesOutcome::Page(page) = page else {
                                return Err(StorageError::Internal("invalid statistics table page".into()));
                            };
                            after = page.names.first().cloned();
                            if let Some(name) = &after {
                                sweep = Some(Sweep::new(storage.record(&account, name).await?)?);
                            }
                        }
                        if let Some(current) = &mut sweep && current.step(&storage, &account).await? {
                            sweep = None;
                        }
                        Ok(())
                    } => result,
                };
                if let Err(error) = result {
                    match error {
                        StorageError::Transient(_) | StorageError::LimitExceeded(_) | StorageError::TableNotFound(_) => {
                            tracing::debug!(account, %error, "table statistics sample deferred");
                            sweep = None;
                        }
                        _ => return Err(error),
                    }
                }
                accounts.push_back((account, after, sweep));
            }
        }).map_err(|error| StorageError::Internal(error.to_string()))
    }
}

pub(super) fn apply(description: &mut TableDescription, statistics: TableStatistics) {
    description.item_count = statistics.base.count;
    description.table_size_bytes = statistics.base.bytes;
    for index in description.local_secondary_indexes.iter_mut().flatten() {
        if let Some(value) = statistics.local.get(&index.index_name) {
            index.item_count = value.count;
            index.index_size_bytes = value.bytes;
        }
    }
    for index in description.global_secondary_indexes.iter_mut().flatten() {
        if let Some(value) = statistics.global.get(&index.index_name) {
            index.item_count = value.count;
            index.index_size_bytes = value.bytes;
        }
    }
}
