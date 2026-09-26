//! Routing a published table across independently owned data Cells.

use crab_cell_runtime::identity::CellTarget;
use extenddb_core::types::{Item, TableKeyInfo, extract_key, item_size_bytes};
use extenddb_storage::error::StorageError;

use super::{CellStorage, cell_error, segment_bounds, target};
use crate::{
    Json, PartitionDeleteOutcome, PartitionLookupInput, PartitionLookupOutcome,
    PartitionPutOutcome, PartitionScan, PartitionScanInput, PartitionScanOutcome,
    PartitionUpdateOutcome, ReadPartitionRoute, ReadRoutePage, RoutePageInput, RoutePageOutcome,
    data_key_hash, data_target,
};

impl CellStorage {
    pub(super) async fn routed_owner(
        &self,
        key_info: &TableKeyInfo,
        key: &Item,
    ) -> Result<Option<(CellTarget, u64)>, StorageError> {
        let hash = data_key_hash(&key_info.table_id, key, &key_info.base_key_schema)
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        let account = target(&key_info.account_id)?;
        let response = self
            .client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: key_info.table_id.clone(),
                    hash,
                }),
            )
            .await
            .map_err(cell_error)?;
        match response.output.0 {
            PartitionLookupOutcome::Unrouted if self.initial_partitions.is_none() => Ok(None),
            PartitionLookupOutcome::Unrouted => {
                Err(StorageError::TableNotActive(key_info.table_id.clone()))
            }
            PartitionLookupOutcome::Routed {
                partition_id,
                epoch,
            } => Ok(Some((
                data_target(&key_info.account_id, &key_info.table_id, &partition_id)
                    .map_err(|error| StorageError::Internal(error.to_string()))?,
                epoch,
            ))),
        }
    }

    pub(super) async fn scan_routed(
        &self,
        key_info: &TableKeyInfo,
        limit: Option<u32>,
        exclusive_start_key: Option<Item>,
        segment: Option<(u64, u64)>,
    ) -> Result<Option<(Vec<Item>, Option<Item>)>, StorageError> {
        let mut remaining = limit.unwrap_or(10_000).min(10_000);
        if remaining == 0 {
            return Err(StorageError::Validation(
                "scan limit must be positive".into(),
            ));
        }
        let start_hash = exclusive_start_key
            .as_ref()
            .map(|key| data_key_hash(&key_info.table_id, key, &key_info.base_key_schema))
            .transpose()
            .map_err(|error| StorageError::Validation(error.to_string()))?;
        let bounds = segment.map(|(segment, total)| segment_bounds(segment, total));
        // Contiguous hash intervals let each segment skip unrelated Cell ranges.
        let start_hash = match (start_hash, bounds) {
            (Some(hash), Some((lower, _))) => Some(hash.max(lower)),
            (None, Some((lower, _))) => Some(lower),
            (hash, None) => hash,
        };
        let account = target(&key_info.account_id)?;
        let mut route_page = RoutePageInput {
            table_id: key_info.table_id.clone(),
            start_hash,
            after_lower: None,
            expected_epoch: None,
        };
        let mut cursor = exclusive_start_key;
        let mut items = Vec::new();
        let mut last_returned = None;
        let mut bytes = 0_usize;
        let mut expected_lower = None;
        loop {
            let response = self
                .client
                .query::<ReadRoutePage>(&account, None, Json(route_page.clone()))
                .await
                .map_err(cell_error)?;
            let (epoch, partitions, has_more) = match response.output.0 {
                RoutePageOutcome::Unrouted if route_page.expected_epoch.is_none() => {
                    return if self.initial_partitions.is_some() {
                        Err(StorageError::TableNotActive(key_info.table_id.clone()))
                    } else {
                        Ok(None)
                    };
                }
                RoutePageOutcome::Unrouted | RoutePageOutcome::Changed => {
                    return Err(stale_partition());
                }
                RoutePageOutcome::Page {
                    epoch,
                    partitions,
                    has_more,
                } => (epoch, partitions, has_more),
            };
            if partitions.is_empty() {
                return Ok(Some((items, None)));
            }
            if expected_lower.is_some_and(|lower| partitions[0].lower != lower) {
                return Err(StorageError::Internal(
                    "route pages are not contiguous".into(),
                ));
            }
            let partition_count = partitions.len();
            let last_range = partitions
                .last()
                .ok_or_else(|| StorageError::Internal("route page is empty".into()))?;
            let next_lower = last_range.upper;
            let after_lower = last_range.lower;
            for (index, partition) in partitions.into_iter().enumerate() {
                if let Some((lower, upper)) = bounds {
                    if partition.upper.is_some_and(|end| end <= lower) {
                        continue;
                    }
                    if upper.is_some_and(|end| partition.lower >= end) {
                        return Ok(Some((items, None)));
                    }
                }
                let owner = data_target(
                    &key_info.account_id,
                    &key_info.table_id,
                    &partition.partition_id,
                )
                .map_err(|error| StorageError::Internal(error.to_string()))?;
                loop {
                    let response = self
                        .client
                        .query::<PartitionScan>(
                            &owner,
                            None,
                            Json(PartitionScanInput {
                                table_id: key_info.table_id.clone(),
                                epoch: partition.epoch,
                                limit: Some(remaining),
                                exclusive_start_key: cursor.clone(),
                            }),
                        )
                        .await
                        .map_err(cell_error)?;
                    let (page, next) = match response.output.0 {
                        PartitionScanOutcome::Page {
                            items,
                            last_evaluated_key,
                        } => (items, last_evaluated_key),
                        PartitionScanOutcome::InvalidKey => {
                            return Err(StorageError::Validation(
                                "scan continuation key does not match table schema".into(),
                            ));
                        }
                        PartitionScanOutcome::InvalidLimit => {
                            return Err(StorageError::Validation(
                                "scan limit must be positive".into(),
                            ));
                        }
                        PartitionScanOutcome::NotInstalled
                        | PartitionScanOutcome::StaleRoute
                        | PartitionScanOutcome::Sealed
                        | PartitionScanOutcome::NotReady => {
                            return Err(stale_partition());
                        }
                        PartitionScanOutcome::NotSealed => {
                            return Err(StorageError::Internal(
                                "ordinary partition scan returned export state".into(),
                            ));
                        }
                    };
                    let item_count = page.len();
                    for (position, item) in page.into_iter().enumerate() {
                        let encoded = serde_json::to_vec(&item)
                            .map_err(|error| StorageError::Internal(error.to_string()))?;
                        let next_bytes = bytes
                            .saturating_add(encoded.len().max(item_size_bytes(&item)))
                            .saturating_add(128);
                        if !items.is_empty() && next_bytes > 900_000 {
                            return Ok(Some((items, last_returned)));
                        }
                        bytes = next_bytes;
                        cursor = Some(extract_key(&item, &key_info.base_key_schema));
                        last_returned = cursor.clone();
                        items.push(item);
                        remaining -= 1;
                        if remaining == 0 {
                            let more = position + 1 < item_count
                                || next.is_some()
                                || index + 1 < partition_count
                                || has_more;
                            return Ok(Some((items, if more { last_returned } else { None })));
                        }
                    }
                    let Some(next) = next else {
                        break;
                    };
                    cursor = Some(next);
                }
                cursor = None;
            }
            if !has_more {
                return Ok(Some((items, None)));
            }
            expected_lower = Some(next_lower.ok_or_else(|| {
                StorageError::Internal("route page ends before next page".into())
            })?);
            // A split between pages must restart the request, or ranges may be missed.
            route_page.start_hash = None;
            route_page.after_lower = Some(after_lower);
            route_page.expected_epoch = Some(epoch);
        }
    }
}

pub(super) fn stale_partition() -> StorageError {
    StorageError::Transient("table partition route changed; retry the request".into())
}

pub(super) fn partition_put_rejection(outcome: PartitionPutOutcome) -> StorageError {
    match outcome {
        PartitionPutOutcome::InvalidItem => {
            StorageError::Validation("item violates table schema".into())
        }
        PartitionPutOutcome::ConditionFailed(old) => StorageError::ConditionFailed(old),
        PartitionPutOutcome::InvalidExpression(message) => StorageError::Validation(message),
        PartitionPutOutcome::TransactionConflict => {
            StorageError::TransactionConflict("item is locked by a transaction".into())
        }
        PartitionPutOutcome::NotInstalled
        | PartitionPutOutcome::StaleRoute
        | PartitionPutOutcome::Sealed
        | PartitionPutOutcome::NotReady
        | PartitionPutOutcome::WrongPartition => stale_partition(),
        PartitionPutOutcome::Applied(_) => {
            StorageError::Internal("unexpected rejected partition put".into())
        }
    }
}

pub(super) fn partition_delete_rejection(outcome: PartitionDeleteOutcome) -> StorageError {
    match outcome {
        PartitionDeleteOutcome::InvalidKey => {
            StorageError::Validation("provided key does not match schema".into())
        }
        PartitionDeleteOutcome::ConditionFailed(old) => StorageError::ConditionFailed(old),
        PartitionDeleteOutcome::InvalidExpression(message) => StorageError::Validation(message),
        PartitionDeleteOutcome::TransactionConflict => {
            StorageError::TransactionConflict("item is locked by a transaction".into())
        }
        PartitionDeleteOutcome::NotInstalled
        | PartitionDeleteOutcome::StaleRoute
        | PartitionDeleteOutcome::Sealed
        | PartitionDeleteOutcome::NotReady
        | PartitionDeleteOutcome::WrongPartition => stale_partition(),
        PartitionDeleteOutcome::Applied(_) => {
            StorageError::Internal("unexpected rejected partition delete".into())
        }
    }
}

pub(super) fn partition_update_rejection(outcome: PartitionUpdateOutcome) -> StorageError {
    match outcome {
        PartitionUpdateOutcome::InvalidItem => {
            StorageError::Validation("item violates table schema".into())
        }
        PartitionUpdateOutcome::ConditionFailed(old) => StorageError::ConditionFailed(old),
        PartitionUpdateOutcome::InvalidExpression(message) => StorageError::Validation(message),
        PartitionUpdateOutcome::TransactionConflict => {
            StorageError::TransactionConflict("item is locked by a transaction".into())
        }
        PartitionUpdateOutcome::NotInstalled
        | PartitionUpdateOutcome::StaleRoute
        | PartitionUpdateOutcome::Sealed
        | PartitionUpdateOutcome::NotReady
        | PartitionUpdateOutcome::WrongPartition => stale_partition(),
        PartitionUpdateOutcome::Applied { .. } => {
            StorageError::Internal("unexpected rejected partition update".into())
        }
    }
}
