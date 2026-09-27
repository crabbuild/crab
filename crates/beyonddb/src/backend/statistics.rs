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
    Json, ListTables, ListTablesInput, ListTablesOutcome, ReadRoutePage, RoutePageInput,
    RoutePageOutcome, TableRecord, data_target, global_index_target,
};

struct Sweep {
    table: TableRecord,
    snapshot: StatisticsSnapshot,
    index: Option<usize>,
    after: Option<[u8; 16]>,
    epoch: Option<u64>,
    index_leaf: Option<(crate::DirectorySpec, u64, crate::statistics::ItemStatistics)>,
}

impl Sweep {
    fn new(table: TableRecord) -> Result<Self, StorageError> {
        Ok(Self {
            snapshot: StatisticsSnapshot {
                table_id: table.id.clone(),
                sampled_at: mutation_identity()?.issued_at_ms,
                base_epoch: None,
                index_generations: Default::default(),
                statistics: TableStatistics::default(),
            },
            table,
            index: None,
            after: None,
            epoch: None,
            index_leaf: None,
        })
    }

    async fn step(
        &mut self,
        storage: &CellStorage,
        account_id: &str,
    ) -> Result<bool, StorageError> {
        let account = target(account_id)?;
        if let Some(position) = self.index {
            if !self.sample_index(storage, account_id, position).await? {
                return Ok(false);
            }
        } else {
            let page = storage
                .client
                .query::<ReadRoutePage>(
                    &account,
                    None,
                    Json(RoutePageInput {
                        table_id: self.table.id.clone(),
                        start_hash: None,
                        after_lower: self.after,
                        expected_epoch: self.epoch,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            match page {
                RoutePageOutcome::Changed => return Err(changed()),
                RoutePageOutcome::Unrouted if self.epoch.is_none() => {
                    self.snapshot.statistics = storage
                        .client
                        .query::<ReadAccountStatistics>(&account, None, Json(self.table.id.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0;
                }
                RoutePageOutcome::Unrouted => return Err(changed()),
                RoutePageOutcome::Page {
                    epoch, partitions, ..
                } => {
                    let range = partitions.first().ok_or_else(changed)?;
                    let owner = data_target(account_id, &self.table.id, &range.partition_id)
                        .map_err(|error| StorageError::Internal(error.to_string()))?;
                    let values = storage
                        .client
                        .query::<ReadPartitionStatistics>(&owner, None, Json(self.table.id.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0;
                    self.snapshot
                        .statistics
                        .base
                        .add(&values.base)
                        .map_err(|error| StorageError::Internal(error.to_string()))?;
                    for (name, value) in values.local {
                        self.snapshot
                            .statistics
                            .local
                            .entry(name)
                            .or_default()
                            .add(&value)
                            .map_err(|error| StorageError::Internal(error.to_string()))?;
                    }
                    self.snapshot.base_epoch = Some(epoch);
                    self.epoch = Some(epoch);
                    self.after = Some(range.lower);
                    if range.upper.is_some() {
                        return Ok(false);
                    }
                }
            }
        }
        let next = self.index.map_or(0, |index| index + 1);
        if next < self.table.global_secondary_indexes.len() {
            self.index = Some(next);
            self.epoch = None;
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
    async fn sample_index(
        &mut self,
        storage: &CellStorage,
        account_id: &str,
        position: usize,
    ) -> Result<bool, StorageError> {
        let index = &self.table.global_secondary_indexes[position];
        let hash = self.after.unwrap_or([0; 16]);
        let page = crate::read_directory_leaf(
            &storage.client,
            target(account_id)?.tenant(),
            &index.id,
            hash,
        )
        .await?;
        let (spec, version, values) = self
            .index_leaf
            .get_or_insert_with(|| (page.spec.clone(), page.version, Default::default()));
        if *spec != page.spec || *version != page.version {
            return Err(changed());
        }
        let range = page
            .ranges
            .first()
            .filter(|range| range.lower == hash)
            .ok_or_else(changed)?;
        let owner = global_index_target(account_id, &index.id, &range.partition_id)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let sampled = storage
            .client
            .query::<ReadGlobalIndexStatistics>(&owner, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        values
            .add(&sampled)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
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
        // Validate the sampled membership at its owning leaf, then leave that
        // immutable interval permanently. A subsequent metadata/data split there
        // cannot make this sweep count both its old source and new children.
        self.snapshot
            .statistics
            .global
            .entry(index.specification.index_name.clone())
            .or_default()
            .add(values)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        self.index_leaf = None;
        if self.after.is_some() {
            return Ok(false);
        }
        self.snapshot.index_generations.insert(index.id.clone());
        Ok(true)
    }
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
