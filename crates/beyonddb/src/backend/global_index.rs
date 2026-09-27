//! Replay durable base changes into independently owned index ranges.

use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::identity::CellTarget;
use extenddb_core::types::{Item, extract_key};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, mutation_identity, target};
use crate::global_index::outbox::{IndexChange, IndexChangeDelivery};
use crate::{
    ApplyGlobalIndexMutation, DescribeTableById, GlobalIndexApplyOutcome, GlobalIndexMutation,
    GlobalIndexRecord, IndexChangeChunk, Json, ReadAccountIndexChange, ReadAccountIndexChangeChunk,
    ReadPartitionIndexChange, ReadPartitionIndexChangeChunk, RecordAccountIndexDelivery,
    RecordPartitionIndexDelivery, RoutePageInput, RoutePageOutcome, TableRecord,
    global_index_target,
};

impl CellStorage {
    /// Project one journal entry, retiring it only after every index apply is durable.
    ///
    /// Returns false when the source has no pending entry. Lost replies leave the
    /// immutable entry available for replay; index versions fence delayed delivery.
    pub async fn project_index_changes(
        &self,
        account_id: &str,
        source: &CellTarget,
        table_id: &str,
    ) -> Result<bool, StorageError> {
        let account = target(account_id)?;
        if source.tenant() != account.tenant()
            || source.application() != account.application()
            || ![crate::NAMESPACE, crate::DATA_NAMESPACE].contains(&source.namespace())
        {
            return Err(StorageError::Validation(
                "invalid index journal source".into(),
            ));
        }
        let header = if source.namespace() == crate::NAMESPACE {
            self.client
                .query::<ReadAccountIndexChange>(source, None, Json(table_id.into()))
                .await
        } else {
            self.client
                .query::<ReadPartitionIndexChange>(source, None, Json(table_id.into()))
                .await
        }
        .map_err(cell_error)?
        .output
        .0;
        let Some(header) = header else {
            return Ok(false);
        };
        let mut bytes = Vec::new();
        while bytes.len() < header.bytes as usize {
            let input = Json(IndexChangeChunk {
                id: header.id,
                offset: bytes.len() as u32,
            });
            let part = if source.namespace() == crate::NAMESPACE {
                self.client
                    .query::<ReadAccountIndexChangeChunk>(source, None, input)
                    .await
            } else {
                self.client
                    .query::<ReadPartitionIndexChangeChunk>(source, None, input)
                    .await
            }
            .map_err(cell_error)?
            .output
            .0;
            // Another worker can acknowledge only after the whole entry projects.
            let Some(part) = part else {
                return Ok(true);
            };
            if part.is_empty() || bytes.len() + part.len() > header.bytes as usize {
                return Err(StorageError::Internal(
                    "invalid index journal length".into(),
                ));
            }
            bytes.extend_from_slice(&part);
        }
        let change: IndexChange = serde_json::from_slice(&bytes)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        if change.table.id != table_id {
            return Err(StorageError::Internal(
                "index journal table mismatch".into(),
            ));
        }
        let current = self
            .client
            .query::<DescribeTableById>(&account, None, Json(table_id.into()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let mut failure = None;
        if let Some(current) = current {
            for index in &change.table.global_secondary_indexes {
                // A retired generation cannot receive new projection work.
                if !current
                    .global_secondary_indexes
                    .iter()
                    .any(|entry| entry.id == index.id)
                {
                    continue;
                }
                if let Err(error) = self.project_to_index(account_id, index, &change).await {
                    failure.get_or_insert(error);
                }
            }
        }
        // Indexes propagate independently. A failed owner must not suppress
        // healthy indexes, but the source journal stays until all have applied.
        let delivery = if failure.is_some() {
            IndexChangeDelivery::Deferred(header.id)
        } else {
            IndexChangeDelivery::Applied(header.id)
        };
        let identity = mutation_identity()?;
        if source.namespace() == crate::NAMESPACE {
            self.client
                .command::<RecordAccountIndexDelivery>(source, identity, Json(delivery))
                .await
        } else {
            self.client
                .command::<RecordPartitionIndexDelivery>(source, identity, Json(delivery))
                .await
        }
        .map_err(cell_error)?;
        failure.map_or(Ok(true), Err)
    }

    async fn project_to_index(
        &self,
        account_id: &str,
        index: &GlobalIndexRecord,
        change: &IndexChange,
    ) -> Result<(), StorageError> {
        let old = change
            .old
            .as_ref()
            .and_then(|item| index.project(&change.table, item));
        let new = change
            .new
            .as_ref()
            .and_then(|item| index.project(&change.table, item));
        if old == new {
            return Ok(());
        }
        let schema = index.key_schema(&change.table);
        let key_bytes = |item: &Item| {
            crate::items::item_key(item, &schema)
                .map_err(|error| StorageError::Internal(error.to_string()))
        };
        let old_key = old.as_ref().map(key_bytes).transpose()?;
        let new_key = new.as_ref().map(key_bytes).transpose()?;
        if old_key != new_key
            && let Some(old) = old
        {
            let key = extract_key(&old, &schema);
            self.apply_index_change(account_id, index, key, None, change.version)
                .await?;
        }
        if let Some(new) = new {
            let key = extract_key(&new, &schema);
            self.apply_index_change(account_id, index, key, Some(new), change.version)
                .await?;
        }
        Ok(())
    }

    async fn apply_index_change(
        &self,
        account_id: &str,
        index: &GlobalIndexRecord,
        key: Item,
        item: Option<Item>,
        version: crate::ProjectionVersion,
    ) -> Result<(), StorageError> {
        let (destination, epoch) = self
            .index_owner(account_id, &index.id, &index.specification.key_schema, &key)
            .await?;
        let result = self
            .client
            .command::<ApplyGlobalIndexMutation>(
                &destination,
                mutation_identity()?,
                Json(GlobalIndexMutation {
                    index_id: index.id.clone(),
                    epoch,
                    key,
                    item,
                    version,
                }),
            )
            .await;
        match result {
            Ok(applied)
                if matches!(
                    applied.output.0,
                    GlobalIndexApplyOutcome::Applied
                        | GlobalIndexApplyOutcome::Replay
                        | GlobalIndexApplyOutcome::Superseded
                ) =>
            {
                Ok(())
            }
            Err(InvocationError::Rejected(result))
                if result.output.0 == GlobalIndexApplyOutcome::StaleRoute =>
            {
                Err(StorageError::Transient(
                    "global index route changed; retry projection".into(),
                ))
            }
            Ok(_) | Err(InvocationError::Rejected(_)) => Err(StorageError::Internal(
                "global index rejected journal mutation".into(),
            )),
            Err(error) => Err(cell_error(error)),
        }
    }

    pub(super) async fn index_owner(
        &self,
        account_id: &str,
        index_id: &str,
        schema: &[extenddb_core::types::KeySchemaElement],
        key: &Item,
    ) -> Result<(CellTarget, u64), StorageError> {
        let hash = crate::data_key_hash(index_id, key, schema)
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        let page = crate::read_global_index_route_page(
            &self.client,
            account_id,
            RoutePageInput {
                table_id: index_id.into(),
                start_hash: Some(hash),
                after_lower: None,
                expected_epoch: None,
            },
        )
        .await?;
        let RoutePageOutcome::Page { partitions, .. } = page else {
            return Err(StorageError::Transient(
                "global index route is not ready".into(),
            ));
        };
        let range = partitions
            .first()
            .filter(|range| range.lower <= hash && range.upper.is_none_or(|upper| hash < upper))
            .ok_or_else(|| StorageError::Internal("global index route has no owner".into()))?;
        let destination = global_index_target(account_id, index_id, &range.partition_id)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        Ok((destination, range.epoch))
    }
}

#[derive(Default)]
struct ProjectionCursor {
    after_table: Option<String>,
    table: Option<TableRecord>,
    after_range: Option<[u8; 16]>,
    epoch: Option<u64>,
    index: Option<usize>,
}

impl ProjectionCursor {
    fn next_index(&mut self, count: usize) {
        self.after_range = None;
        self.epoch = None;
        let next = self.index.map_or(0, |position| position + 1);
        self.index = (next < count).then_some(next);
        if self.index.is_none() {
            self.table = None;
        }
    }
}

impl CellStorage {
    /// Supervise bounded, fair journal replay for configured accounts.
    pub fn install_global_index_loop(
        self,
        tasks: &crab_cell_host::CellNodeTaskGroup,
        accounts: Vec<String>,
        provisioner: std::sync::Arc<crate::CellInitialPartitionProvisioner>,
        nodes: crab_cell_runtime::node::NodeDirectory,
    ) -> Result<(), StorageError> {
        use std::time::Duration;
        let mut accounts: std::collections::VecDeque<_> = accounts
            .into_iter()
            .map(|account| (account, ProjectionCursor::default()))
            .collect();
        if accounts.is_empty() {
            return Ok(());
        }
        let cancellation = tasks.cancellation_token();
        tasks.spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(100));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { () = cancellation.cancelled() => return Ok::<(), StorageError>(()), _ = ticks.tick() => {} }
                let Some((account, mut cursor)) = accounts.pop_front() else { return Ok(()); };
                let result = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    result = self.project_account_page(&account, &mut cursor, &provisioner, &nodes) => result,
                };
                accounts.push_back((account, cursor));
                if let Err(error) = result { tracing::warn!(%error, "global index projection deferred"); }
            }
        }).map_err(|error| StorageError::Internal(error.to_string()))
    }

    async fn project_account_page(
        &self,
        account: &str,
        cursor: &mut ProjectionCursor,
        provisioner: &crate::CellInitialPartitionProvisioner,
        nodes: &crab_cell_runtime::node::NodeDirectory,
    ) -> Result<(), StorageError> {
        use futures_util::{StreamExt, stream};
        if cursor.table.is_none() {
            let page = self
                .client
                .query::<crate::ListTables>(
                    &target(account)?,
                    None,
                    Json(crate::ListTablesInput {
                        limit: 1,
                        exclusive_start: cursor.after_table.clone(),
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            let crate::ListTablesOutcome::Page(page) = page else {
                return Err(StorageError::Internal(
                    "invalid projection table listing".into(),
                ));
            };
            let Some(name) = page.names.first() else {
                cursor.after_table = None;
                return Ok(());
            };
            cursor.after_table = Some(name.clone());
            let table = self.record(account, name).await?;
            if table.global_secondary_indexes.is_empty() {
                return Ok(());
            }
            cursor.table = Some(table);
        }
        let Some(table) = cursor.table.as_ref() else {
            return Ok(());
        };
        let table_id = table.id.clone();
        let index = cursor
            .index
            .map(|position| &table.global_secondary_indexes[position]);
        let index_id = index.map(|index| index.id.as_str());
        let index_count = table.global_secondary_indexes.len();
        let input = Json(RoutePageInput {
            table_id: index_id.unwrap_or(&table_id).to_owned(),
            start_hash: None,
            after_lower: cursor.after_range,
            expected_epoch: cursor.epoch,
        });
        let account_target = target(account)?;
        let page = if let Some(index_id) = index_id {
            let page = async {
                provisioner
                    .recover_index_directory_path(
                        &self.client,
                        account,
                        index_id,
                        cursor.after_range,
                        nodes,
                    )
                    .await?;
                crate::read_global_index_route_page(&self.client, account, input.0).await
            }
            .await;
            match page {
                Ok(page) => page,
                Err(error) => {
                    // An unavailable metadata path cannot pin the account sweep.
                    // Revisit it on the next pass while other indexes recover.
                    cursor.next_index(index_count);
                    return Err(error);
                }
            }
        } else {
            self.client
                .query::<crate::ReadRoutePage>(&account_target, None, input)
                .await
                .map_err(cell_error)?
                .output
                .0
        };
        let mut sources = Vec::new();
        match page {
            RoutePageOutcome::Changed => {
                cursor.epoch = None;
                cursor.after_range = None;
                return Ok(());
            }
            RoutePageOutcome::Unrouted => {
                cursor.after_range = None;
                if index.is_none() {
                    sources.push(account_target);
                }
            }
            RoutePageOutcome::Page {
                epoch,
                partitions,
                has_more,
            } => {
                cursor.after_range = if has_more {
                    partitions.last().map(|range| range.lower)
                } else {
                    None
                };
                for range in partitions {
                    let target = if let Some(id) = index_id {
                        global_index_target(account, id, &range.partition_id)
                    } else {
                        crate::data_target(account, &table_id, &range.partition_id)
                    }
                    .map_err(|error| StorageError::Internal(error.to_string()))?;
                    sources.push(target);
                }
                cursor.epoch = Some(epoch);
            }
        }
        let project = index.is_none();
        if cursor.after_range.is_none() {
            cursor.next_index(index_count);
        }
        // Visit index owners even without pending base writes. Advance before
        // admission so a failed range cannot pin the account's entire sweep.
        let table_id = table_id.as_str();
        let mut jobs = stream::iter(sources)
            .map(|source| async move {
                provisioner.recover_projection_owner(&source, nodes).await?;
                if project {
                    self.project_index_changes(account, &source, table_id)
                        .await
                        .map(|_| ())
                } else {
                    Ok(())
                }
            })
            .buffer_unordered(4);
        let mut failure = None;
        while let Some(result) = jobs.next().await {
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
