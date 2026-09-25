//! ExtendDB storage traits awaiting durable Cell implementations.
//!
//! Each unimplemented operation fails explicitly. This allows the completed
//! table and item paths to run through ExtendDB's real dispatch contract
//! without claiming Streams, TTL, tags, or backups work yet.

use extenddb_core::types::{
    BackupDescription, BackupDetails, BackupSummary, ContinuousBackupsDescription,
    DescribeStreamInput, Item, StreamDescription, StreamRecord, TableDescription, Tag,
    TimeToLiveDescription, TimeToLiveStatus,
};
use extenddb_storage::error::StorageError;
use extenddb_storage::{
    BackupEngine, BoxedFuture, MetadataEngine, StreamEngine, StreamListResult, StreamRecordsResult,
    TableEngine, TtlTableInfo, WorkerStore,
};

use super::{CellStorage, unsupported};

impl MetadataEngine for CellStorage {
    fn describe_ttl(
        &self,
        account_id: &str,
        table_name: &str,
    ) -> BoxedFuture<'_, Result<TimeToLiveDescription, StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        Box::pin(async move {
            self.table_key_info(&account_id, &table_name).await?;
            Ok(TimeToLiveDescription {
                time_to_live_status: TimeToLiveStatus::Disabled,
                attribute_name: None,
            })
        })
    }

    fn update_ttl(
        &self,
        _account_id: &str,
        _table_name: &str,
        _attribute_name: &str,
        _enabled: bool,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("TTL")) })
    }

    fn tag_resource(&self, _arn: &str, _tags: &[Tag]) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("resource tags")) })
    }

    fn untag_resource(
        &self,
        _arn: &str,
        _tag_keys: &[String],
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("resource tags")) })
    }

    fn list_tags(&self, _arn: &str) -> BoxedFuture<'_, Result<Vec<Tag>, StorageError>> {
        Box::pin(async { Err(unsupported("resource tags")) })
    }

    fn tables_with_ttl(
        &self,
        _account_id: &str,
    ) -> BoxedFuture<'_, Result<Vec<(String, String)>, StorageError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn all_tables_with_ttl(&self) -> BoxedFuture<'_, Result<Vec<TtlTableInfo>, StorageError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn all_tables_with_ttl_index_ready(
        &self,
    ) -> BoxedFuture<'_, Result<Vec<TtlTableInfo>, StorageError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn create_ttl_index(
        &self,
        _account_id: &str,
        _table_name: &str,
        _ttl_attribute: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("TTL index")) })
    }

    fn drop_ttl_index(
        &self,
        _account_id: &str,
        _table_name: &str,
        _ttl_attribute: &str,
    ) -> BoxedFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Err(unsupported("TTL index")) })
    }

    fn find_expired_items_indexed(
        &self,
        _account_id: &str,
        _table_name: &str,
        _ttl_attribute: &str,
        _limit: usize,
    ) -> BoxedFuture<'_, Result<Vec<Item>, StorageError>> {
        Box::pin(async { Err(unsupported("TTL sweep")) })
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
