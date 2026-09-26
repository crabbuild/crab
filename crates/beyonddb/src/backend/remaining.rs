//! ExtendDB storage traits awaiting durable Cell implementations.
//!
//! Each unimplemented operation fails explicitly. This allows the completed
//! table and item paths to run through ExtendDB's real dispatch contract
//! without claiming Streams or backups work yet. TTL uses per-account sweeps.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::identity::CellTarget;
use extenddb_core::expression::{CompareOp, Expr, ExpressionMaps, PathElement};
use extenddb_core::types::{
    AttributeValue, BackupDescription, BackupDetails, BackupSummary, ContinuousBackupsDescription,
    DescribeStreamInput, Item, StreamDescription, StreamRecord, TableDescription, Tag,
    TimeToLiveDescription, TimeToLiveStatus, extract_key,
};
use extenddb_storage::error::StorageError;
use extenddb_storage::{
    BackupEngine, BoxedFuture, DataEngine, MetadataEngine, StreamEngine, StreamListResult,
    StreamRecordsResult, TableEngine, TtlTableInfo, WorkerStore,
};

use crate::Json;
use crate::tags::{ReadTags, TagChange, TagRequest, UpdateTags, UpdateTagsInput, parse_table_arn};
use crate::ttl::{
    ListTtlTables, ReadTtl, ReadTtlOutcome, UpdateTtl, UpdateTtlInput, UpdateTtlOutcome,
};
use crate::{
    BackfillPartitionTtl, BackfillPartitionTtlOutcome, ConfigurePartitionTtl,
    ConfigurePartitionTtlInput, ConfigurePartitionTtlOutcome, ExpiredPartitionInput,
    ExpiredPartitionOutcome, PartitionTtlInput, ReadExpiredPartition, ReadPartitionTtl,
    ReadPartitionTtlInput, ReadPartitionTtlOutcome, ReadRoutePage, RoutePageInput,
    RoutePageOutcome, data_target,
};

use super::{CellStorage, cell_error, mutation_identity, target, unsupported};

impl CellStorage {
    /// Sweep bounded expired items for TTL tables owned by one account.
    pub async fn sweep_account_ttl(&self, account_id: &str) -> Result<u64, StorageError> {
        let tables = self.tables_with_ttl(account_id).await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageError::Internal("system clock predates epoch".into()))?
            .as_secs();
        let mut deleted = 0_u64;
        for (table_name, attribute) in tables {
            match self
                .create_ttl_index(account_id, &table_name, &attribute)
                .await
            {
                Ok(()) => {}
                Err(StorageError::Transient(_)) => continue,
                Err(error) => return Err(error),
            }
            let items = self
                .find_expired_items_indexed(account_id, &table_name, &attribute, 100)
                .await?;
            if items.is_empty() {
                continue;
            }
            let key_info = self.table_key_info(account_id, &table_name).await?;
            let (condition, maps) = ttl_condition(&attribute, now);
            for item in items {
                let key = extract_key(&item, &key_info.key_schema);
                match self
                    .delete_item(&key_info, &key, false, Some(&condition), &maps, None)
                    .await
                {
                    Ok(_) => deleted += 1,
                    Err(StorageError::ConditionFailed(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(deleted)
    }

    async fn ttl_partitions(
        &self,
        account_id: &str,
        table_name: &str,
    ) -> Result<(String, Vec<(CellTarget, u64)>), StorageError> {
        let table = self.record(account_id, table_name).await?;
        let account = target(account_id)?;
        let mut after_lower = None;
        let mut expected_epoch = None;
        let mut owners = Vec::new();
        loop {
            let page = self
                .client
                .query::<ReadRoutePage>(
                    &account,
                    None,
                    Json(RoutePageInput {
                        table_id: table.id.clone(),
                        start_hash: None,
                        after_lower,
                        expected_epoch,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            let (epoch, partitions, has_more) = match page {
                RoutePageOutcome::Page {
                    epoch,
                    partitions,
                    has_more,
                } => (epoch, partitions, has_more),
                RoutePageOutcome::Unrouted => {
                    return Err(StorageError::TableNotActive(table_name.to_owned()));
                }
                RoutePageOutcome::Changed => {
                    return Err(StorageError::Transient("TTL route changed".into()));
                }
            };
            after_lower = partitions.last().map(|partition| partition.lower);
            for partition in partitions {
                let owner = data_target(account_id, &table.id, &partition.partition_id)
                    .map_err(|error| StorageError::Internal(error.to_string()))?;
                owners.push((owner, partition.epoch));
            }
            if !has_more {
                return Ok((table.id, owners));
            }
            if after_lower.is_none() {
                return Err(StorageError::Internal("empty TTL route page".into()));
            }
            expected_epoch = Some(epoch);
        }
    }

    async fn configure_ttl_partition(
        &self,
        owner: &CellTarget,
        table_id: &str,
        epoch: u64,
        attribute: Option<&str>,
    ) -> Result<bool, StorageError> {
        let state = self
            .client
            .query::<ReadPartitionTtl>(
                owner,
                None,
                Json(ReadPartitionTtlInput {
                    table_id: table_id.to_owned(),
                    epoch,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let ready = match state {
            ReadPartitionTtlOutcome::State {
                attribute_name,
                ready,
            } if attribute_name.as_deref() == attribute => ready,
            ReadPartitionTtlOutcome::State { .. } | ReadPartitionTtlOutcome::Missing => {
                let result = self
                    .client
                    .command::<ConfigurePartitionTtl>(
                        owner,
                        mutation_identity()?,
                        Json(ConfigurePartitionTtlInput {
                            table_id: table_id.to_owned(),
                            epoch,
                            attribute_name: attribute.map(str::to_owned),
                        }),
                    )
                    .await;
                match result {
                    Ok(committed) => match committed.output.0 {
                        ConfigurePartitionTtlOutcome::Configured { ready } => ready,
                        _ => {
                            return Err(StorageError::Internal(
                                "unexpected TTL configuration".into(),
                            ));
                        }
                    },
                    Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                        ConfigurePartitionTtlOutcome::StaleRoute
                        | ConfigurePartitionTtlOutcome::NotReady
                        | ConfigurePartitionTtlOutcome::NotInstalled => {
                            return Err(StorageError::Transient(
                                "TTL partition is not ready".into(),
                            ));
                        }
                        ConfigurePartitionTtlOutcome::InvalidAttribute => {
                            return Err(StorageError::Validation("invalid TTL attribute".into()));
                        }
                        ConfigurePartitionTtlOutcome::Configured { .. } => {
                            return Err(StorageError::Internal(
                                "unexpected rejected TTL config".into(),
                            ));
                        }
                    },
                    Err(error) => return Err(cell_error(error)),
                }
            }
            ReadPartitionTtlOutcome::StaleRoute | ReadPartitionTtlOutcome::NotReady => {
                return Err(StorageError::Transient("TTL partition is not ready".into()));
            }
        };
        let Some(attribute) = attribute else {
            return Ok(ready);
        };
        if ready {
            return Ok(true);
        }
        let result = self
            .client
            .command::<BackfillPartitionTtl>(
                owner,
                mutation_identity()?,
                Json(PartitionTtlInput {
                    table_id: table_id.to_owned(),
                    epoch,
                    attribute_name: attribute.to_owned(),
                }),
            )
            .await;
        match result {
            Ok(committed) => match committed.output.0 {
                BackfillPartitionTtlOutcome::Ready => Ok(true),
                BackfillPartitionTtlOutcome::Progress => Ok(false),
                _ => Err(StorageError::Internal("unexpected TTL backfill".into())),
            },
            Err(InvocationError::Rejected(_)) => {
                Err(StorageError::Transient("TTL backfill route changed".into()))
            }
            Err(error) => Err(cell_error(error)),
        }
    }

    fn tag_request(&self, arn: &str) -> Result<TagRequest, StorageError> {
        let (region, account_id, table_name) = parse_table_arn(arn)
            .ok_or_else(|| StorageError::Validation("invalid table resource ARN".into()))?;
        if region != self.region {
            return Err(StorageError::Validation(
                "table ARN region does not match server".into(),
            ));
        }
        Ok(TagRequest {
            account_id: account_id.to_owned(),
            table_name: table_name.to_owned(),
            resource_arn: arn.to_owned(),
        })
    }

    async fn update_tags(&self, arn: &str, changes: Vec<TagChange>) -> Result<(), StorageError> {
        let request = self.tag_request(arn)?;
        let account = target(&request.account_id)?;
        match self
            .client
            .command::<UpdateTags>(
                &account,
                mutation_identity()?,
                Json(UpdateTagsInput { request, changes }),
            )
            .await
        {
            Ok(committed) if committed.output.0 => Ok(()),
            Ok(_) => Err(StorageError::Internal(
                "unexpected tag update result".into(),
            )),
            Err(InvocationError::Rejected(_)) => Err(StorageError::TableNotFound(arn.to_owned())),
            Err(error) => Err(cell_error(error)),
        }
    }
}

fn ttl_condition(attribute: &str, now: u64) -> (Expr, ExpressionMaps) {
    let path = Expr::Path(vec![PathElement::Attribute("#ttl".into())]);
    let lower = Expr::Compare {
        left: Box::new(path.clone()),
        op: CompareOp::Gt,
        right: Box::new(Expr::Placeholder("zero".into())),
    };
    let upper = Expr::Compare {
        left: Box::new(path),
        op: CompareOp::Le,
        right: Box::new(Expr::Placeholder("now".into())),
    };
    (
        Expr::And(Box::new(lower), Box::new(upper)),
        ExpressionMaps::new(
            HashMap::from([("ttl".into(), attribute.to_owned())]),
            HashMap::from([
                ("zero".into(), AttributeValue::N("0".into())),
                ("now".into(), AttributeValue::N(now.to_string())),
            ]),
        ),
    )
}

impl MetadataEngine for CellStorage {
    fn describe_ttl(
        &self,
        account_id: &str,
        table_name: &str,
    ) -> BoxedFuture<'_, Result<TimeToLiveDescription, StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        Box::pin(async move {
            let account = target(&account_id)?;
            let outcome = self
                .client
                .query::<ReadTtl>(&account, None, Json(table_name.clone()))
                .await
                .map_err(cell_error)?
                .output
                .0;
            match outcome {
                ReadTtlOutcome::TableNotFound => Err(StorageError::TableNotFound(table_name)),
                ReadTtlOutcome::Disabled => Ok(TimeToLiveDescription {
                    time_to_live_status: TimeToLiveStatus::Disabled,
                    attribute_name: None,
                }),
                ReadTtlOutcome::Enabled(attribute_name) => Ok(TimeToLiveDescription {
                    time_to_live_status: TimeToLiveStatus::Enabled,
                    attribute_name: Some(attribute_name),
                }),
            }
        })
    }

    fn update_ttl(
        &self,
        account_id: &str,
        table_name: &str,
        attribute_name: &str,
        enabled: bool,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        let attribute_name = enabled.then(|| attribute_name.to_owned());
        Box::pin(async move {
            let account = target(&account_id)?;
            match self
                .client
                .command::<UpdateTtl>(
                    &account,
                    mutation_identity()?,
                    Json(UpdateTtlInput {
                        table_name: table_name.clone(),
                        attribute_name,
                    }),
                )
                .await
            {
                Ok(committed) if committed.output.0 == UpdateTtlOutcome::Updated => Ok(()),
                Ok(_) => Err(StorageError::Internal(
                    "unexpected TTL update result".into(),
                )),
                Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                    UpdateTtlOutcome::TableNotFound => Err(StorageError::TableNotFound(table_name)),
                    UpdateTtlOutcome::InvalidAttribute => {
                        Err(StorageError::Validation("invalid TTL attribute".into()))
                    }
                    UpdateTtlOutcome::Updated => Err(StorageError::Internal(
                        "unexpected rejected TTL update".into(),
                    )),
                },
                Err(error) => Err(cell_error(error)),
            }
        })
    }

    fn tag_resource(&self, arn: &str, tags: &[Tag]) -> BoxedFuture<'_, Result<(), StorageError>> {
        let arn = arn.to_owned();
        let changes = tags.iter().cloned().map(TagChange::Put).collect();
        Box::pin(async move { self.update_tags(&arn, changes).await })
    }

    fn untag_resource(
        &self,
        arn: &str,
        tag_keys: &[String],
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        let arn = arn.to_owned();
        let changes = tag_keys.iter().cloned().map(TagChange::Remove).collect();
        Box::pin(async move { self.update_tags(&arn, changes).await })
    }

    fn list_tags(&self, arn: &str) -> BoxedFuture<'_, Result<Vec<Tag>, StorageError>> {
        let arn = arn.to_owned();
        Box::pin(async move {
            let request = self.tag_request(&arn)?;
            let account = target(&request.account_id)?;
            self.client
                .query::<ReadTags>(&account, None, Json(request))
                .await
                .map_err(cell_error)?
                .output
                .0
                .ok_or(StorageError::TableNotFound(arn))
        })
    }

    fn tables_with_ttl(
        &self,
        account_id: &str,
    ) -> BoxedFuture<'_, Result<Vec<(String, String)>, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            let account = target(&account_id)?;
            let mut after = None;
            let mut tables = Vec::new();
            loop {
                let page = self
                    .client
                    .query::<ListTtlTables>(&account, None, Json(after))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                tables.extend(page.tables);
                let Some(next) = page.last_evaluated else {
                    return Ok(tables);
                };
                after = Some(next);
            }
        })
    }

    fn all_tables_with_ttl(&self) -> BoxedFuture<'_, Result<Vec<TtlTableInfo>, StorageError>> {
        Box::pin(async { Err(unsupported("global TTL listing")) })
    }

    fn all_tables_with_ttl_index_ready(
        &self,
    ) -> BoxedFuture<'_, Result<Vec<TtlTableInfo>, StorageError>> {
        Box::pin(async { Err(unsupported("global TTL listing")) })
    }

    fn create_ttl_index(
        &self,
        account_id: &str,
        table_name: &str,
        ttl_attribute: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        let ttl_attribute = ttl_attribute.to_owned();
        Box::pin(async move {
            let (table_id, partitions) = self.ttl_partitions(&account_id, &table_name).await?;
            let mut ready = true;
            for (owner, epoch) in partitions {
                ready &= self
                    .configure_ttl_partition(&owner, &table_id, epoch, Some(&ttl_attribute))
                    .await?;
            }
            if ready {
                Ok(())
            } else {
                Err(StorageError::Transient("TTL backfill in progress".into()))
            }
        })
    }

    fn drop_ttl_index(
        &self,
        account_id: &str,
        table_name: &str,
        _ttl_attribute: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        Box::pin(async move {
            let (table_id, partitions) = self.ttl_partitions(&account_id, &table_name).await?;
            for (owner, epoch) in partitions {
                self.configure_ttl_partition(&owner, &table_id, epoch, None)
                    .await?;
            }
            Ok(())
        })
    }

    fn find_expired_items_indexed(
        &self,
        account_id: &str,
        table_name: &str,
        ttl_attribute: &str,
        limit: usize,
    ) -> BoxedFuture<'_, Result<Vec<Item>, StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        let ttl_attribute = ttl_attribute.to_owned();
        Box::pin(async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let cutoff_epoch = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| StorageError::Internal("system clock predates epoch".into()))?
                    .as_secs(),
            )
            .map_err(|_| StorageError::Internal("TTL clock overflow".into()))?;
            let (table_id, partitions) = self.ttl_partitions(&account_id, &table_name).await?;
            let mut expired = Vec::new();
            for (owner, epoch) in partitions {
                let outcome = self
                    .client
                    .query::<ReadExpiredPartition>(
                        &owner,
                        None,
                        Json(ExpiredPartitionInput {
                            table_id: table_id.clone(),
                            epoch,
                            attribute_name: ttl_attribute.clone(),
                            cutoff_epoch,
                        }),
                    )
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                match outcome {
                    ExpiredPartitionOutcome::Items(items) => expired.extend(items),
                    ExpiredPartitionOutcome::StaleRoute | ExpiredPartitionOutcome::NotReady => {
                        return Err(StorageError::Transient("TTL partition is not ready".into()));
                    }
                }
                if expired.len() >= limit {
                    expired.truncate(limit);
                    break;
                }
            }
            Ok(expired)
        })
    }

    fn refresh_table_size(
        &self,
        _account_id: &str,
        _table_name: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("table size refresh")) })
    }

    fn list_active_table_names(
        &self,
        _account_id: &str,
    ) -> BoxedFuture<'_, Result<Vec<String>, StorageError>> {
        Box::pin(async { Err(unsupported("background table listing")) })
    }

    fn all_active_tables(&self) -> BoxedFuture<'_, Result<Vec<(String, String)>, StorageError>> {
        Box::pin(async { Err(unsupported("global table listing")) })
    }
}

impl StreamEngine for CellStorage {
    fn write_stream_record(
        &self,
        _account_id: &str,
        _record: &StreamRecord,
        _shard_id: &str,
        _table_name: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn get_stream_records(
        &self,
        _account_id: &str,
        _shard_id: &str,
        _after_sequence: Option<&str>,
        _limit: i64,
    ) -> BoxedFuture<'_, StreamRecordsResult> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn describe_stream(
        &self,
        _account_id: &str,
        _input: &DescribeStreamInput,
    ) -> BoxedFuture<'_, Result<StreamDescription, StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn list_streams(
        &self,
        _account_id: &str,
        _table_name: Option<&str>,
        _limit: i64,
        _exclusive_start_stream_arn: Option<&str>,
    ) -> BoxedFuture<'_, StreamListResult> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn cleanup_expired_stream_records(
        &self,
        _retention_hours: i64,
    ) -> BoxedFuture<'_, Result<u64, StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn assign_shard(
        &self,
        _account_id: &str,
        _table_name: &str,
        _partition_key: &str,
    ) -> BoxedFuture<'_, Result<String, StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn next_sequence_number(
        &self,
        _shard_id: &str,
    ) -> BoxedFuture<'_, Result<String, StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn validate_shard(
        &self,
        _account_id: &str,
        _stream_arn: &str,
        _shard_id: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }

    fn latest_sequence_number(
        &self,
        _shard_id: &str,
    ) -> BoxedFuture<'_, Result<Option<String>, StorageError>> {
        Box::pin(async { Err(unsupported("DynamoDB Streams")) })
    }
}

impl WorkerStore for CellStorage {
    fn process_control_plane_transitions(
        &self,
    ) -> BoxedFuture<'_, Result<Vec<(String, &'static str)>, StorageError>> {
        // Table creation and deletion reach their durable end state in one Cell command.
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl BackupEngine for CellStorage {
    fn create_backup(
        &self,
        _account_id: &str,
        _table_name: &str,
        _backup_name: &str,
    ) -> BoxedFuture<'_, Result<BackupDetails, StorageError>> {
        Box::pin(async { Err(unsupported("table backups")) })
    }

    fn describe_backup(
        &self,
        _account_id: &str,
        _backup_arn: &str,
    ) -> BoxedFuture<'_, Result<BackupDescription, StorageError>> {
        Box::pin(async { Err(unsupported("table backups")) })
    }

    fn list_backups(
        &self,
        _account_id: &str,
        _table_name: Option<&str>,
    ) -> BoxedFuture<'_, Result<Vec<BackupSummary>, StorageError>> {
        Box::pin(async { Err(unsupported("table backups")) })
    }

    fn delete_backup(
        &self,
        _account_id: &str,
        _backup_arn: &str,
    ) -> BoxedFuture<'_, Result<BackupDescription, StorageError>> {
        Box::pin(async { Err(unsupported("table backups")) })
    }

    fn restore_table_from_backup(
        &self,
        _account_id: &str,
        _target_table_name: &str,
        _backup_arn: &str,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        Box::pin(async { Err(unsupported("table backups")) })
    }

    fn describe_continuous_backups(
        &self,
        _account_id: &str,
        _table_name: &str,
    ) -> BoxedFuture<'_, Result<ContinuousBackupsDescription, StorageError>> {
        Box::pin(async { Err(unsupported("point-in-time recovery")) })
    }

    fn update_continuous_backups(
        &self,
        _account_id: &str,
        _table_name: &str,
        _pitr_enabled: bool,
    ) -> BoxedFuture<'_, Result<ContinuousBackupsDescription, StorageError>> {
        Box::pin(async { Err(unsupported("point-in-time recovery")) })
    }

    fn restore_table_to_point_in_time(
        &self,
        _account_id: &str,
        _source_table_name: &str,
        _target_table_name: &str,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        Box::pin(async { Err(unsupported("point-in-time recovery")) })
    }
}
