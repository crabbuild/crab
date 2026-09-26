//! ExtendDB item operations over account and published data Cell routes.

mod routed;

use routed::{
    partition_delete_rejection, partition_put_rejection, partition_update_rejection,
    stale_partition,
};

use extenddb_core::expression::{
    CompareOp, Expr, ExpressionMaps, KeyCondition, PathElement, SortKeyCondition, UpdateAction,
};
use extenddb_core::types::{
    AttributeValue, CancellationReason, Item, KeyType, ReturnValuesOnConditionCheckFailure,
    TableKeyInfo, extract_key,
};
use extenddb_storage::error::StorageError;
use extenddb_storage::{
    BoxedFuture, DataEngine, IdempotencyKey, ItemPairResult, QueryResult, StreamCapture,
    TransactGetOp, TransactWriteOp,
};

use super::{CellStorage, cell_error, mutation_identity, target, unsupported};
use crate::TransactionToken;
use crate::expression_wire::{WireCondition, WireUpdate};
use crate::{
    ConditionCheckInput, DeleteItem, DeleteItemInput, GetItem, GetItemInput, GetItemOutcome,
    ItemMutationOutcome, Json, PartitionDelete, PartitionDeleteInput, PartitionDeleteOutcome,
    PartitionGet, PartitionGetInput, PartitionGetOutcome, PartitionPut, PartitionPutInput,
    PartitionPutOutcome, PartitionQuery, PartitionQueryInput, PartitionQueryOutcome,
    PartitionUpdate, PartitionUpdateInput, PartitionUpdateOutcome, PutItem, PutItemInput,
    ScanItems, ScanItemsInput, ScanItemsOutcome, SortComparison, SortPredicate, TransactionFailure,
    TransactionOperation, UpdateItem, UpdateItemInput, UpdateItemOutcome, data_key_hash,
};
use crab_cell_runtime::client::InvocationError;

impl DataEngine for CellStorage {
    fn put_item(
        &self,
        key_info: &TableKeyInfo,
        item: Item,
        return_old: bool,
        condition: Option<&Expr>,
        maps: &ExpressionMaps,
        stream: Option<&StreamCapture>,
    ) -> BoxedFuture<'_, Result<Option<Item>, StorageError>> {
        let key_info = key_info.clone();
        let condition = condition.map(|expr| WireCondition::from_core(expr, maps));
        let streamed = stream.is_some();
        Box::pin(async move {
            if streamed {
                return Err(unsupported("streamed PutItem"));
            }
            let key = extract_key(&item, &key_info.base_key_schema);
            if let Some((partition, epoch)) = self.routed_owner(&key_info, &key).await? {
                let outcome = self
                    .client
                    .command::<PartitionPut>(
                        &partition,
                        mutation_identity()?,
                        Json(PartitionPutInput {
                            table_id: key_info.table_id,
                            epoch,
                            item,
                            condition,
                        }),
                    )
                    .await;
                let old = match outcome {
                    Ok(committed) => match committed.output.0 {
                        PartitionPutOutcome::Applied(old) => old,
                        _ => return Err(StorageError::Internal("unexpected partition put".into())),
                    },
                    Err(InvocationError::Rejected(committed)) => {
                        return Err(partition_put_rejection(committed.output.0));
                    }
                    Err(error) => return Err(cell_error(error)),
                };
                return Ok(if return_old { old } else { None });
            }
            let target = target(&key_info.account_id)?;
            let input = PutItemInput {
                table_name: key_info.table_name.clone(),
                table_id: key_info.table_id,
                item,
                condition,
            };
            let old = match self
                .client
                .command::<PutItem>(&target, mutation_identity()?, Json(input))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    ItemMutationOutcome::Applied(old) => old,
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful put result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => {
                    return Err(mutation_rejection(committed.output.0, &key_info.table_name));
                }
                Err(error) => return Err(cell_error(error)),
            };
            Ok(if return_old { old } else { None })
        })
    }

    fn get_item(
        &self,
        key_info: &TableKeyInfo,
        key: &Item,
    ) -> BoxedFuture<'_, Result<Option<Item>, StorageError>> {
        let key_info = key_info.clone();
        let key = key.clone();
        Box::pin(async move {
            if let Some((partition, epoch)) = self.routed_owner(&key_info, &key).await? {
                let output = self
                    .query_resolving::<PartitionGet>(
                        &partition,
                        &key_info.account_id,
                        Json(PartitionGetInput {
                            table_id: key_info.table_id,
                            epoch,
                            key,
                        }),
                    )
                    .await?;
                return match output.output.0 {
                    PartitionGetOutcome::Found(item) => Ok(item),
                    PartitionGetOutcome::Conflict(_) => Err(StorageError::Transient(
                        "read is waiting for transaction resolution".into(),
                    )),
                    PartitionGetOutcome::InvalidKey => Err(StorageError::Validation(
                        "provided key does not match schema".into(),
                    )),
                    PartitionGetOutcome::NotInstalled
                    | PartitionGetOutcome::StaleRoute
                    | PartitionGetOutcome::Sealed
                    | PartitionGetOutcome::NotReady
                    | PartitionGetOutcome::WrongPartition => Err(stale_partition()),
                };
            }
            let target = target(&key_info.account_id)?;
            let output = self
                .query_resolving::<GetItem>(
                    &target,
                    &key_info.account_id,
                    Json(GetItemInput {
                        table_name: key_info.table_name.clone(),
                        table_id: key_info.table_id,
                        key,
                    }),
                )
                .await?;
            match output.output.0 {
                GetItemOutcome::Found(item) => Ok(item),
                GetItemOutcome::Conflict(_) => Err(StorageError::Transient(
                    "item is locked by a transaction".into(),
                )),
                GetItemOutcome::TableNotFound => {
                    Err(StorageError::TableNotFound(key_info.table_name))
                }
                GetItemOutcome::InvalidKey => Err(StorageError::Validation(
                    "provided key does not match schema".into(),
                )),
            }
        })
    }

    fn delete_item(
        &self,
        key_info: &TableKeyInfo,
        key: &Item,
        return_old: bool,
        condition: Option<&Expr>,
        maps: &ExpressionMaps,
        stream: Option<&StreamCapture>,
    ) -> BoxedFuture<'_, Result<Option<Item>, StorageError>> {
        let key_info = key_info.clone();
        let key = key.clone();
        let condition = condition.map(|expr| WireCondition::from_core(expr, maps));
        let streamed = stream.is_some();
        Box::pin(async move {
            if streamed {
                return Err(unsupported("streamed DeleteItem"));
            }
            if let Some((partition, epoch)) = self.routed_owner(&key_info, &key).await? {
                let outcome = self
                    .client
                    .command::<PartitionDelete>(
                        &partition,
                        mutation_identity()?,
                        Json(PartitionDeleteInput {
                            return_old,
                            table_id: key_info.table_id,
                            epoch,
                            key,
                            condition,
                        }),
                    )
                    .await;
                let old = match outcome {
                    Ok(committed) => match committed.output.0 {
                        PartitionDeleteOutcome::Applied(old) => old,
                        _ => {
                            return Err(StorageError::Internal(
                                "unexpected partition delete".into(),
                            ));
                        }
                    },
                    Err(InvocationError::Rejected(committed)) => {
                        return Err(partition_delete_rejection(committed.output.0));
                    }
                    Err(error) => return Err(cell_error(error)),
                };
                return Ok(old);
            }
            let target = target(&key_info.account_id)?;
            let input = DeleteItemInput {
                return_old,
                table_name: key_info.table_name.clone(),
                table_id: key_info.table_id,
                key,
                condition,
            };
            let old = match self
                .client
                .command::<DeleteItem>(&target, mutation_identity()?, Json(input))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    ItemMutationOutcome::Applied(old) => old,
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful delete result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => {
                    return Err(mutation_rejection(committed.output.0, &key_info.table_name));
                }
                Err(error) => return Err(cell_error(error)),
            };
            Ok(old)
        })
    }

    fn update_item(
        &self,
        key_info: &TableKeyInfo,
        key: &Item,
        actions: &[UpdateAction],
        return_old: bool,
        return_new: bool,
        condition: Option<&Expr>,
        maps: &ExpressionMaps,
        stream: Option<&StreamCapture>,
    ) -> BoxedFuture<'_, ItemPairResult> {
        let key_info = key_info.clone();
        let key = key.clone();
        let update = WireUpdate::from_core(actions, maps);
        let condition = condition.map(|expr| WireCondition::from_core(expr, maps));
        let streamed = stream.is_some();
        Box::pin(async move {
            if streamed {
                return Err(unsupported("streamed UpdateItem"));
            }
            if let Some((partition, epoch)) = self.routed_owner(&key_info, &key).await? {
                let input = PartitionUpdateInput {
                    table_id: key_info.table_id,
                    epoch,
                    key,
                    update,
                    condition,
                };
                let outcome = self
                    .client
                    .command::<PartitionUpdate>(&partition, mutation_identity()?, Json(input))
                    .await;
                let (old, new) = match outcome {
                    Ok(committed) => match committed.output.0 {
                        PartitionUpdateOutcome::Applied { old, new } => (old, new),
                        _ => {
                            return Err(StorageError::Internal(
                                "unexpected partition update".into(),
                            ));
                        }
                    },
                    Err(InvocationError::Rejected(committed)) => {
                        return Err(partition_update_rejection(committed.output.0));
                    }
                    Err(error) => return Err(cell_error(error)),
                };
                return Ok((
                    if return_old { old } else { None },
                    if return_new { Some(new) } else { None },
                ));
            }
            let target = target(&key_info.account_id)?;
            let input = UpdateItemInput {
                table_name: key_info.table_name.clone(),
                table_id: key_info.table_id,
                key,
                update,
                condition,
            };
            let (old, new) = match self
                .client
                .command::<UpdateItem>(&target, mutation_identity()?, Json(input))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    UpdateItemOutcome::Applied { old, new } => (old, new),
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful update result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => {
                    return Err(update_rejection(committed.output.0, &key_info.table_name));
                }
                Err(error) => return Err(cell_error(error)),
            };
            Ok((
                if return_old { old } else { None },
                if return_new { Some(new) } else { None },
            ))
        })
    }

    fn query(
        &self,
        key_info: &TableKeyInfo,
        key_condition: &KeyCondition,
        maps: &ExpressionMaps,
        forward: bool,
        limit: Option<i64>,
        exclusive_start_key: Option<&Item>,
        index_name: Option<&str>,
    ) -> BoxedFuture<'_, QueryResult> {
        let prepared = normalized_query(key_info, key_condition, maps);
        let key_info = key_info.clone();
        let exclusive_start_key = exclusive_start_key.cloned();
        let index_name = index_name.map(str::to_owned);
        Box::pin(async move {
            if limit.is_some_and(|value| value <= 0) {
                return Err(StorageError::Validation(
                    "query limit must be positive".into(),
                ));
            }
            let PreparedQuery {
                partition_key,
                sort,
                extra_range_equals,
            } = prepared?;
            if key_info
                .key_schema
                .iter()
                .any(|key| key.key_type == KeyType::Range)
            {
                let owner = self.routed_owner(&key_info, &partition_key).await?;
                if owner.is_none() && index_name.is_none() {
                    return Err(unsupported("sort-key Query on an account-local table"));
                }
                let epoch = owner.as_ref().map_or(0, |(_, epoch)| *epoch);
                let limit = limit
                    .map(|value| {
                        u32::try_from(value).map_err(|_| {
                            StorageError::Validation(
                                "query limit is outside supported range".into(),
                            )
                        })
                    })
                    .transpose()?
                    .unwrap_or(10_000);
                let input = Json(PartitionQueryInput {
                    index_name,
                    table_id: key_info.table_id.clone(),
                    epoch,
                    partition_key,
                    sort,
                    extra_range_equals,
                    forward,
                    limit,
                    exclusive_start_key,
                });
                let output = if let Some((owner, _)) = owner {
                    self.query_resolving::<PartitionQuery>(&owner, &key_info.account_id, input)
                        .await?
                } else {
                    let account = target(&key_info.account_id)?;
                    self.query_resolving::<crate::secondary_index::QueryAccountIndex>(
                        &account,
                        &key_info.account_id,
                        input,
                    )
                    .await?
                };
                return match output.output.0 {
                    PartitionQueryOutcome::Page {
                        items,
                        last_evaluated_key,
                    } => Ok((items, last_evaluated_key)),
                    PartitionQueryOutcome::Conflict(_) => Err(StorageError::Transient(
                        "query is waiting for transaction resolution".into(),
                    )),
                    PartitionQueryOutcome::InvalidKey | PartitionQueryOutcome::InvalidCondition => {
                        Err(StorageError::Validation(
                            "invalid Query key condition or continuation".into(),
                        ))
                    }
                    PartitionQueryOutcome::InvalidLimit => Err(StorageError::Validation(
                        "query limit must be positive".into(),
                    )),
                    PartitionQueryOutcome::NotInstalled
                    | PartitionQueryOutcome::StaleRoute
                    | PartitionQueryOutcome::Sealed
                    | PartitionQueryOutcome::NotReady
                    | PartitionQueryOutcome::WrongPartition => Err(stale_partition()),
                };
            }
            if let Some(start) = exclusive_start_key {
                if start != partition_key {
                    return Err(StorageError::Validation(
                        "query continuation key does not match partition key".into(),
                    ));
                }
                return Ok((Vec::new(), None));
            }
            let item = self.get_item(&key_info, &partition_key).await?;
            Ok((item.into_iter().collect(), None))
        })
    }

    fn scan(
        &self,
        key_info: &TableKeyInfo,
        limit: Option<i64>,
        exclusive_start_key: Option<&Item>,
        segment: Option<i64>,
        total_segments: Option<i64>,
        index_name: Option<&str>,
    ) -> BoxedFuture<'_, QueryResult> {
        let key_info = key_info.clone();
        let exclusive_start_key = exclusive_start_key.cloned();
        let index_name = index_name.map(str::to_owned);
        Box::pin(async move {
            let segment = match (segment, total_segments) {
                (None, None) => None,
                (Some(segment), Some(total)) if total > 0 && (0..total).contains(&segment) => {
                    Some((segment as u64, total as u64))
                }
                _ => {
                    return Err(StorageError::Validation(
                        "invalid parallel Scan segment".into(),
                    ));
                }
            };
            let limit = limit
                .map(|value| {
                    u32::try_from(value).map_err(|_| {
                        StorageError::Validation("scan limit is outside supported range".into())
                    })
                })
                .transpose()?;
            if let Some((items, last_evaluated_key)) = self
                .scan_routed(
                    &key_info,
                    limit,
                    exclusive_start_key.clone(),
                    segment,
                    index_name.as_deref(),
                )
                .await?
            {
                // Retain the unfiltered cursor so empty segment pages still advance.
                return Ok((scan_segment(items, &key_info, segment)?, last_evaluated_key));
            }
            let target = target(&key_info.account_id)?;
            let output = self
                .query_resolving::<ScanItems>(
                    &target,
                    &key_info.account_id,
                    Json(ScanItemsInput {
                        index_name,
                        table_name: key_info.table_name.clone(),
                        table_id: key_info.table_id.clone(),
                        limit,
                        exclusive_start_key,
                    }),
                )
                .await?;
            match output.output.0 {
                ScanItemsOutcome::Page {
                    items,
                    last_evaluated_key,
                } => Ok((scan_segment(items, &key_info, segment)?, last_evaluated_key)),
                ScanItemsOutcome::Conflict(_) => Err(StorageError::Transient(
                    "scan range is locked by a transaction".into(),
                )),
                ScanItemsOutcome::TableNotFound => {
                    Err(StorageError::TableNotFound(key_info.table_name))
                }
                ScanItemsOutcome::InvalidKey => Err(StorageError::Validation(
                    "scan continuation key does not match table schema".into(),
                )),
                ScanItemsOutcome::InvalidLimit => Err(StorageError::Validation(
                    "scan limit must be positive".into(),
                )),
            }
        })
    }

    fn transact_get_items(
        &self,
        ops: &[TransactGetOp<'_>],
    ) -> BoxedFuture<'_, Result<Vec<Option<Item>>, StorageError>> {
        let prepared = prepare_gets(ops);
        let routing: Vec<_> = ops
            .iter()
            .map(|op| ((*op.key_info).clone(), (*op.key).clone()))
            .collect();
        Box::pin(async move {
            let (account_id, inputs) = prepared?;
            // Saved participant images keep a legal aggregate read from crossing
            // one Cell response. Use the same serialization boundary for every route.
            self.transaction_read(&account_id, inputs, routing).await
        })
    }

    fn transact_write_items(
        &self,
        ops: &[TransactWriteOp<'_>],
        idempotency: Option<IdempotencyKey<'_>>,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        let prepared = prepare_writes(ops);
        let token = idempotency.map(|key| TransactionToken {
            account_id: key.account_id.to_owned(),
            token: key.token.to_owned(),
            fingerprint: key.fingerprint.to_owned(),
        });
        let routing: Vec<_> = ops
            .iter()
            .map(|op| match op {
                TransactWriteOp::Put { key_info, item, .. } => {
                    ((*key_info).clone(), (*item).clone())
                }
                TransactWriteOp::Delete { key_info, key, .. }
                | TransactWriteOp::Update { key_info, key, .. }
                | TransactWriteOp::ConditionCheck { key_info, key, .. } => {
                    ((*key_info).clone(), (*key).clone())
                }
            })
            .collect();
        let return_old_on_failure: Vec<bool> = ops
            .iter()
            .map(|op| match op {
                TransactWriteOp::Put {
                    return_values_on_ccf,
                    ..
                }
                | TransactWriteOp::Delete {
                    return_values_on_ccf,
                    ..
                }
                | TransactWriteOp::Update {
                    return_values_on_ccf,
                    ..
                }
                | TransactWriteOp::ConditionCheck {
                    return_values_on_ccf,
                    ..
                } => *return_values_on_ccf == ReturnValuesOnConditionCheckFailure::AllOld,
            })
            .collect();
        Box::pin(async move {
            let (account_id, operations) = prepared?;
            if token
                .as_ref()
                .is_some_and(|token| token.account_id != account_id)
            {
                return Err(StorageError::Validation(
                    "transaction token account differs from items".into(),
                ));
            }
            let count = operations.len();
            let admitted = self
                .admit_transaction(&account_id, token, operations, routing)
                .await?;
            match admitted.decision {
                crate::CoordinatorDecision::Commit if admitted.replay => {
                    Err(StorageError::IdempotentReplay)
                }
                crate::CoordinatorDecision::Commit => Ok(()),
                crate::CoordinatorDecision::Abort { index, reason } => Err(transaction_canceled(
                    usize::from(index.unwrap_or(0)),
                    reason.unwrap_or(TransactionFailure::Conflict),
                    count,
                    &return_old_on_failure,
                )),
                crate::CoordinatorDecision::Begin => Err(StorageError::Transient(
                    "transaction decision remains pending".into(),
                )),
            }
        })
    }

    fn cleanup_expired_idempotency_tokens(
        &self,
        _max_age_seconds: i64,
    ) -> BoxedFuture<'_, Result<u64, StorageError>> {
        Box::pin(async { Ok(0) })
    }
}

fn scan_segment(
    items: Vec<Item>,
    key_info: &TableKeyInfo,
    segment: Option<(u64, u64)>,
) -> Result<Vec<Item>, StorageError> {
    let Some((segment, total)) = segment else {
        return Ok(items);
    };
    items
        .into_iter()
        .filter_map(|item| {
            let key = extract_key(&item, &key_info.base_key_schema);
            match data_key_hash(&key_info.table_id, &key, &key_info.base_key_schema) {
                Ok(hash) if segment_for_hash(hash, total) == segment => Some(Ok(item)),
                Ok(_) => None,
                Err(error) => Some(Err(StorageError::Internal(error.to_string()))),
            }
        })
        .collect()
}

fn segment_for_hash(hash: [u8; 16], total: u64) -> u64 {
    let prefix = u128::from_be_bytes(hash) >> 64;
    ((prefix * u128::from(total)) >> 64) as u64
}

fn segment_bounds(segment: u64, total: u64) -> ([u8; 16], Option<[u8; 16]>) {
    let space = 1_u128 << 64;
    let total = u128::from(total);
    let start = (u128::from(segment) * space).div_ceil(total);
    let end = ((u128::from(segment) + 1) * space).div_ceil(total);
    (
        (start << 64).to_be_bytes(),
        (end < space).then(|| (end << 64).to_be_bytes()),
    )
}

pub(super) fn transaction_canceled(
    index: usize,
    reason: TransactionFailure,
    count: usize,
    return_old_on_failure: &[bool],
) -> StorageError {
    let mut reasons = vec![CancellationReason::none(); count];
    if let Some(slot) = reasons.get_mut(index) {
        *slot = match reason {
            TransactionFailure::Throttled => CancellationReason {
                code: "ThrottlingError".into(),
                message: Some("transaction capacity is exhausted; retry with backoff".into()),
                item: None,
            },
            TransactionFailure::Conflict => CancellationReason {
                code: "TransactionConflict".into(),
                message: Some("transaction conflicts with another operation".into()),
                item: None,
            },
            TransactionFailure::Validation(message) => {
                CancellationReason::validation_error(message)
            }
            TransactionFailure::ConditionFailed(old) => {
                let include_old = return_old_on_failure.get(index).copied().unwrap_or(false);
                CancellationReason::condition_check_failed_with_item(if include_old {
                    old
                } else {
                    None
                })
            }
        };
    }
    StorageError::TransactionCanceled(reasons)
}

struct PreparedQuery {
    partition_key: Item,
    sort: Option<SortPredicate>,
    extra_range_equals: Vec<(String, AttributeValue)>,
}

fn normalized_query(
    key_info: &TableKeyInfo,
    condition: &KeyCondition,
    maps: &ExpressionMaps,
) -> Result<PreparedQuery, StorageError> {
    let mut partition_key = Item::new();
    for (path, value) in std::iter::once((&condition.pk_path, &condition.pk_value)).chain(
        condition
            .extra_pk_conditions
            .iter()
            .map(|(path, value)| (path, value)),
    ) {
        let name = query_name(path, maps)?;
        if !key_info
            .key_schema
            .iter()
            .any(|key| key.key_type == KeyType::Hash && key.attribute_name == name)
            || partition_key
                .insert(name, query_value(value, maps)?)
                .is_some()
        {
            return Err(StorageError::Validation(
                "key condition does not match partition key".into(),
            ));
        }
    }
    if partition_key.len()
        != key_info
            .key_schema
            .iter()
            .filter(|key| key.key_type == KeyType::Hash)
            .count()
    {
        return Err(StorageError::Validation(
            "key condition is missing a partition key".into(),
        ));
    }
    let sort = condition
        .sk_condition
        .as_ref()
        .map(|condition| match condition {
            SortKeyCondition::Compare { path, op, value } => {
                let op = match op {
                    CompareOp::Eq => SortComparison::Eq,
                    CompareOp::Lt => SortComparison::Lt,
                    CompareOp::Le => SortComparison::Le,
                    CompareOp::Gt => SortComparison::Gt,
                    CompareOp::Ge => SortComparison::Ge,
                    CompareOp::Ne => {
                        return Err(StorageError::Validation(
                            "sort key does not support inequality".into(),
                        ));
                    }
                };
                Ok(SortPredicate::Compare {
                    attribute: query_name(path, maps)?,
                    op,
                    value: query_value(value, maps)?,
                })
            }
            SortKeyCondition::Between { path, low, high } => Ok(SortPredicate::Between {
                attribute: query_name(path, maps)?,
                low: query_value(low, maps)?,
                high: query_value(high, maps)?,
            }),
            SortKeyCondition::BeginsWith { path, prefix } => Ok(SortPredicate::BeginsWith {
                attribute: query_name(path, maps)?,
                prefix: query_value(prefix, maps)?,
            }),
        })
        .transpose()?;
    let extra_range_equals = condition
        .extra_sk_conditions
        .iter()
        .map(|(path, value)| Ok((query_name(path, maps)?, query_value(value, maps)?)))
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(PreparedQuery {
        partition_key,
        sort,
        extra_range_equals,
    })
}

fn query_name(path: &[PathElement], maps: &ExpressionMaps) -> Result<String, StorageError> {
    let [PathElement::Attribute(name)] = path else {
        return Err(StorageError::Validation(
            "invalid key condition path".into(),
        ));
    };
    if let Some(alias) = name.strip_prefix('#') {
        maps.resolve_name(alias)
            .map(ToOwned::to_owned)
            .map_err(|error| StorageError::Validation(error.to_string()))
    } else {
        Ok(name.clone())
    }
}

fn query_value(expr: &Expr, maps: &ExpressionMaps) -> Result<AttributeValue, StorageError> {
    let Expr::Placeholder(name) = expr else {
        return Err(StorageError::Validation(
            "invalid key condition value".into(),
        ));
    };
    maps.resolve_value(name)
        .cloned()
        .map_err(|error| StorageError::Validation(error.to_string()))
}

fn mutation_rejection(outcome: ItemMutationOutcome, table_name: &str) -> StorageError {
    match outcome {
        ItemMutationOutcome::Conflict => {
            StorageError::TransactionConflict("item is locked by a transaction".into())
        }
        ItemMutationOutcome::TableNotFound => StorageError::TableNotFound(table_name.to_owned()),
        ItemMutationOutcome::InvalidItem => {
            StorageError::Validation("item violates table schema".into())
        }
        ItemMutationOutcome::ConditionFailed(old) => StorageError::ConditionFailed(old),
        ItemMutationOutcome::InvalidExpression(message) => StorageError::Validation(message),
        ItemMutationOutcome::Applied(_) => {
            StorageError::Internal("unexpected rejected item mutation".into())
        }
    }
}

fn update_rejection(outcome: UpdateItemOutcome, table_name: &str) -> StorageError {
    match outcome {
        UpdateItemOutcome::Conflict => {
            StorageError::TransactionConflict("item is locked by a transaction".into())
        }
        UpdateItemOutcome::TableNotFound => StorageError::TableNotFound(table_name.to_owned()),
        UpdateItemOutcome::InvalidItem => {
            StorageError::Validation("item violates table schema".into())
        }
        UpdateItemOutcome::ConditionFailed(old) => StorageError::ConditionFailed(old),
        UpdateItemOutcome::InvalidExpression(message) => StorageError::Validation(message),
        UpdateItemOutcome::Applied { .. } => {
            StorageError::Internal("unexpected rejected update result".into())
        }
    }
}

fn prepare_gets(ops: &[TransactGetOp<'_>]) -> Result<(String, Vec<GetItemInput>), StorageError> {
    let Some(first) = ops.first() else {
        return Err(StorageError::Validation("transaction is empty".into()));
    };
    let account_id = first.key_info.account_id.clone();
    let mut inputs = Vec::with_capacity(ops.len());
    for op in ops {
        if op.key_info.account_id != account_id {
            return Err(StorageError::Validation(
                "transaction crosses accounts".into(),
            ));
        }
        inputs.push(GetItemInput {
            table_name: op.key_info.table_name.clone(),
            table_id: op.key_info.table_id.clone(),
            key: op.key.clone(),
        });
    }
    Ok((account_id, inputs))
}

fn prepare_writes(
    ops: &[TransactWriteOp<'_>],
) -> Result<(String, Vec<TransactionOperation>), StorageError> {
    let mut account_id = None;
    let mut operations = Vec::with_capacity(ops.len());
    for op in ops {
        let (key_info, operation) = match op {
            TransactWriteOp::Put {
                key_info,
                item,
                condition,
                maps,
                stream,
                ..
            } if stream.is_none() => (
                *key_info,
                TransactionOperation::Put(PutItemInput {
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    item: (*item).clone(),
                    condition: condition.map(|expr| WireCondition::from_core(expr, maps)),
                }),
            ),
            TransactWriteOp::Delete {
                key_info,
                key,
                condition,
                maps,
                stream,
                ..
            } if stream.is_none() => (
                *key_info,
                TransactionOperation::Delete(DeleteItemInput {
                    return_old: false,
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    key: (*key).clone(),
                    condition: condition.map(|expr| WireCondition::from_core(expr, maps)),
                }),
            ),
            TransactWriteOp::Update {
                key_info,
                key,
                actions,
                condition,
                maps,
                stream,
                ..
            } if stream.is_none() => (
                *key_info,
                TransactionOperation::Update(UpdateItemInput {
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    key: (*key).clone(),
                    update: WireUpdate::from_core(actions, maps),
                    condition: condition.map(|expr| WireCondition::from_core(expr, maps)),
                }),
            ),
            TransactWriteOp::ConditionCheck {
                key_info,
                key,
                condition,
                maps,
                ..
            } => (
                *key_info,
                TransactionOperation::ConditionCheck(ConditionCheckInput {
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    key: (*key).clone(),
                    condition: WireCondition::from_core(condition, maps),
                }),
            ),
            _ => {
                return Err(unsupported("streamed transaction write"));
            }
        };
        if account_id
            .as_ref()
            .is_some_and(|account| account != &key_info.account_id)
        {
            return Err(StorageError::Validation(
                "transaction crosses accounts".into(),
            ));
        }
        account_id.get_or_insert_with(|| key_info.account_id.clone());
        operations.push(operation);
    }
    let account_id =
        account_id.ok_or_else(|| StorageError::Validation("transaction is empty".into()))?;
    Ok((account_id, operations))
}
