//! Replay durable base changes into independently owned index ranges.

use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::identity::CellTarget;
use extenddb_core::types::{Item, extract_key};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, mutation_identity, target};
use crate::global_index::outbox::IndexChange;
use crate::{
    AckAccountIndexChange, AckPartitionIndexChange, ApplyGlobalIndexMutation, DescribeTableById,
    GlobalIndexApplyOutcome, GlobalIndexMutation, GlobalIndexRecord, IndexChangeChunk, Json,
    ReadAccountIndexChange, ReadAccountIndexChangeChunk, ReadGlobalIndexRoutePage,
    ReadPartitionIndexChange, ReadPartitionIndexChangeChunk, RoutePageInput, RoutePageOutcome,
    TableRecord, global_index_target,
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
        if let Some(current) = current {
            for index in &change.table.global_secondary_indexes {
                // Dropped table/index generations are unreachable. A replacement
                // has a different ID and must never receive the retired journal.
                if !current
                    .global_secondary_indexes
                    .iter()
                    .any(|entry| entry.id == index.id)
                {
                    continue;
                }
                let old = change
                    .old
                    .as_ref()
                    .and_then(|item| index.project(&change.table, item));
                let new = change
                    .new
                    .as_ref()
                    .and_then(|item| index.project(&change.table, item));
                if old == new {
                    continue;
                }
                let schema = index.key_schema(&change.table);
                let old_key = old
                    .as_ref()
                    .map(|item| crate::items::item_key(item, &schema))
                    .transpose()
                    .map_err(|error| StorageError::Internal(error.to_string()))?;
                let new_key = new
                    .as_ref()
                    .map(|item| crate::items::item_key(item, &schema))
                    .transpose()
                    .map_err(|error| StorageError::Internal(error.to_string()))?;
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
            }
        }
        let identity = mutation_identity()?;
        if source.namespace() == crate::NAMESPACE {
            self.client
                .command::<AckAccountIndexChange>(source, identity, Json(header.id))
                .await
        } else {
            self.client
                .command::<AckPartitionIndexChange>(source, identity, Json(header.id))
                .await
        }
        .map_err(cell_error)?;
        Ok(true)
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
        let page = self
            .client
            .query::<ReadGlobalIndexRoutePage>(
                &target(account_id)?,
                None,
                Json(RoutePageInput {
                    table_id: index_id.into(),
                    start_hash: Some(hash),
                    after_lower: None,
                    expected_epoch: None,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
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
}

impl CellStorage {
    /// Supervise bounded, fair journal replay for configured accounts.
    pub fn install_global_index_loop(
        self,
        tasks: &crab_cell_host::CellNodeTaskGroup,
        accounts: Vec<String>,
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
                    result = self.project_account_page(&account, &mut cursor) => result,
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
        let page = self
            .client
            .query::<crate::ReadRoutePage>(
                &target(account)?,
                None,
                Json(RoutePageInput {
                    table_id: table_id.clone(),
                    start_hash: None,
                    after_lower: cursor.after_range,
                    expected_epoch: cursor.epoch,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let mut sources = Vec::new();
        match page {
            RoutePageOutcome::Changed => {
                cursor.epoch = None;
                cursor.after_range = None;
                return Ok(());
            }
            RoutePageOutcome::Unrouted => sources.push(target(account)?),
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
                    sources.push(
                        crate::data_target(account, &table_id, &range.partition_id)
                            .map_err(|error| StorageError::Internal(error.to_string()))?,
                    );
                }
                cursor.epoch = Some(epoch);
            }
        }
        if cursor.after_range.is_none() {
            cursor.table = None;
            cursor.epoch = None;
        }
        // Advance the bounded sweep even after a refusal. Durable entries remain
        // for the next pass, while another table or owner can make progress.
        let table_id = table_id.as_str();
        let mut jobs =
            stream::iter(sources)
                .map(|source| async move {
                    self.project_index_changes(account, &source, table_id).await
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
