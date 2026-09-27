//! Mutable index write policy and resumable, versioned backfill in a data Cell.

use super::*;
use extenddb_core::types::{AttributeDefinition, CreateTableInput};

/// Account-issued index configuration, separate from immutable range installation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionIndexPolicy {
    pub revision: u64,
    pub attribute_definitions: Vec<AttributeDefinition>,
    pub indexes: Vec<crate::GlobalIndexRecord>,
}

impl PartitionIndexPolicy {
    fn apply(&self, table: &mut TableRecord) {
        table
            .attribute_definitions
            .clone_from(&self.attribute_definitions);
        table.global_secondary_indexes.clone_from(&self.indexes);
    }

    fn valid_for(&self, table: &TableRecord) -> bool {
        let immutable_keys = table.key_schema.iter().chain(
            table
                .local_secondary_indexes
                .iter()
                .flat_map(|index| &index.key_schema),
        );
        if self.revision == 0
            || immutable_keys.into_iter().any(|key| {
                table
                    .attribute_definitions
                    .iter()
                    .find(|attr| attr.attribute_name == key.attribute_name)
                    != self
                        .attribute_definitions
                        .iter()
                        .find(|attr| attr.attribute_name == key.attribute_name)
            })
        {
            return false;
        }
        let mut ids = std::collections::HashSet::new();
        if self
            .indexes
            .iter()
            .any(|index| !ids.insert(&index.id) || blake3::Hash::from_hex(&index.id).is_err())
        {
            return false;
        }
        // Billing belongs to the account and can change after installation.
        // Validate only the data schema here, independent of that old snapshot.
        let indexes = self
            .indexes
            .iter()
            .map(|index| {
                let mut specification = index.specification.clone();
                specification.provisioned_throughput = None;
                specification
            })
            .collect();
        let input = CreateTableInput {
            table_name: table.table_name.clone(),
            key_schema: table.key_schema.clone(),
            attribute_definitions: self.attribute_definitions.clone(),
            local_secondary_indexes: Some(table.local_secondary_indexes.clone()),
            global_secondary_indexes: Some(indexes),
            billing_mode: Some(extenddb_core::types::BillingMode::PayPerRequest),
            ..CreateTableInput::default()
        };
        extenddb_core::validation::validate_create_table(&input, &Default::default()).is_ok()
    }
}

/// Durable configuration and generations whose historical rows are not delivered yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionIndexState {
    pub policy: PartitionIndexPolicy,
    pub pending_backfills: Vec<String>,
}

fn policy(
    mut sql: impl FnMut(&crate::SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<PartitionIndexPolicy>> {
    let rows = sql(&statement(
        "SELECT policy FROM ddb_partition_index_policy WHERE singleton = 1",
        vec![],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid partition index policy")),
    }
}

pub(super) fn command_spec(context: &CommandContext<'_, '_>) -> Result<Option<PartitionSpec>> {
    let rows = context.sql(&statement(
        "SELECT spec FROM ddb_partition WHERE singleton = 1",
        vec![],
    ))?;
    let Some(mut spec) = decode_spec(&rows[0])? else {
        return Ok(None);
    };
    if let Some(policy) = policy(|batch| context.sql(batch))? {
        policy.apply(&mut spec.table);
    }
    Ok(Some(spec))
}

/// Install an account-issued index revision on a serving range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigurePartitionIndexesInput {
    pub table_id: String,
    pub epoch: u64,
    pub policy: PartitionIndexPolicy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConfigurePartitionIndexesOutcome {
    Configured,
    StaleRoute,
    StalePolicy,
    Conflict,
    InvalidPolicy,
    NotReady,
    InFlightTransaction,
}

/// Atomically change future write validation and create historical backfill work.
pub struct ConfigurePartitionIndexes;
impl Command for ConfigurePartitionIndexes {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ConfigurePartitionIndexesInput>;
    type Output = Json<ConfigurePartitionIndexesOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        use ConfigurePartitionIndexesOutcome as Outcome;
        let reject = |outcome| Ok(CommandResult::Rejected(Json(outcome)));
        let Some(spec) = command_spec(context)? else {
            return reject(Outcome::StaleRoute);
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return reject(Outcome::StaleRoute);
        }
        if let Some(current) = policy(|batch| context.sql(batch))? {
            if input.policy.revision < current.revision {
                return reject(Outcome::StalePolicy);
            }
            if input.policy.revision == current.revision {
                return if input.policy == current {
                    Ok(CommandResult::Success(Json(Outcome::Configured)))
                } else {
                    reject(Outcome::Conflict)
                };
            }
        }
        if !matches!(command_access(context)?, AccessState::Serving) {
            return reject(Outcome::NotReady);
        }
        // Prepared writes reserved space under the current policy. Increasing
        // their index work after Prepare could make a durable COMMIT impossible.
        if transaction::has_transaction_locks(context)? {
            return reject(Outcome::InFlightTransaction);
        }
        if !input.policy.valid_for(&spec.table)
            || input.policy.indexes.iter().any(|index| {
                spec.table.global_secondary_indexes.iter().any(|old| {
                    old.id == index.id
                        && (old.specification.index_name != index.specification.index_name
                            || old.specification.key_schema != index.specification.key_schema
                            || old.specification.projection != index.specification.projection
                            || old.specification.key_schema.iter().any(|key| {
                                spec.table.attribute_definitions.iter().find(|attribute| {
                                    attribute.attribute_name == key.attribute_name
                                }) != input.policy.attribute_definitions.iter().find(|attribute| {
                                    attribute.attribute_name == key.attribute_name
                                })
                            }))
                })
            })
        {
            return reject(Outcome::InvalidPolicy);
        }
        for previous in &spec.table.global_secondary_indexes {
            if !input
                .policy
                .indexes
                .iter()
                .any(|index| index.id == previous.id)
            {
                context.sql(&statement(
                    "DELETE FROM ddb_partition_index_backfills WHERE index_id = ?1",
                    vec![SqlValue::Text(previous.id.clone())],
                ))?;
            }
        }
        for index in &input.policy.indexes {
            if !spec
                .table
                .global_secondary_indexes
                .iter()
                .any(|previous| previous.id == index.id)
            {
                context.sql(&statement("INSERT INTO ddb_partition_index_backfills (index_id, cursor, scanned) VALUES (?1, NULL, 0)", vec![SqlValue::Text(index.id.clone())]))?;
            }
        }
        store_policy(context, &input.policy)?;
        Ok(CommandResult::Success(Json(Outcome::Configured)))
    }
}

fn store_policy(context: &CommandContext<'_, '_>, policy: &PartitionIndexPolicy) -> Result<()> {
    context.sql(&statement("INSERT INTO ddb_partition_index_policy (singleton, policy) VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET policy = excluded.policy", vec![SqlValue::Blob(serde_json::to_vec(policy)?)]))?;
    Ok(())
}

/// Read authoritative backfill progress, including undelivered historical images.
pub struct ReadPartitionIndexes;
impl Query for ReadPartitionIndexes {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 15;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<PartitionIndexState>>;

    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        let Some(policy) = policy(|batch| context.sql(batch))? else {
            return Ok(Json(None));
        };
        let rows = context.sql(&statement("SELECT b.index_id FROM ddb_partition_index_backfills b WHERE b.scanned = 0 OR EXISTS (SELECT 1 FROM ddb_index_changes c WHERE c.backfill_index_id = b.index_id) ORDER BY b.index_id", vec![]))?;
        let pending_backfills = rows[0]
            .rows
            .iter()
            .map(|row| match row.as_slice() {
                [SqlValue::Text(id)] => Ok(id.clone()),
                _ => Err(Error::Command("invalid partition backfill identity")),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Json(Some(PartitionIndexState {
            policy,
            pending_backfills,
        })))
    }
}

/// Cursor-fenced request for one bounded historical projection batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackfillPartitionIndexInput {
    pub table_id: String,
    pub epoch: u64,
    pub revision: u64,
    pub index_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BackfillPartitionIndexOutcome {
    Scanned,
    Progress,
    StaleRoute,
    StalePolicy,
    NotReady,
}

/// Append bounded historical images and advance their cursor in the same commit.
pub struct BackfillPartitionIndex;
impl Command for BackfillPartitionIndex {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 18;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<BackfillPartitionIndexInput>;
    type Output = Json<BackfillPartitionIndexOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        use BackfillPartitionIndexOutcome as Outcome;
        let reject = |outcome| Ok(CommandResult::Rejected(Json(outcome)));
        let Some(mut spec) = command_spec(context)? else {
            return reject(Outcome::StaleRoute);
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return reject(Outcome::StaleRoute);
        }
        if !matches!(command_access(context)?, AccessState::Serving) {
            return reject(Outcome::NotReady);
        }
        if policy(|batch| context.sql(batch))?
            .is_none_or(|policy| policy.revision != input.revision)
        {
            return reject(Outcome::StalePolicy);
        }
        let Some(index) = spec
            .table
            .global_secondary_indexes
            .iter()
            .find(|index| index.id == input.index_id)
            .cloned()
        else {
            return reject(Outcome::StalePolicy);
        };
        let rows = context.sql(&statement(
            "SELECT cursor, scanned FROM ddb_partition_index_backfills WHERE index_id = ?1",
            vec![SqlValue::Text(input.index_id.clone())],
        ))?;
        let Some([cursor, SqlValue::Integer(scanned)]) = rows[0].rows.first().map(Vec::as_slice)
        else {
            return reject(Outcome::StalePolicy);
        };
        if *scanned == 1 {
            return Ok(CommandResult::Success(Json(Outcome::Scanned)));
        }
        let mut cursor = cursor.clone();
        // A dedicated entry for this generation makes its delivery barrier
        // independent of failed sibling indexes and the ordinary write tail.
        spec.table.global_secondary_indexes = vec![index.clone()];
        let mut bytes = 0;
        for _ in 0..16 {
            let rows = context.sql(&statement("SELECT item_key FROM ddb_partition_items WHERE (?1 IS NULL OR item_key > ?1) ORDER BY item_key LIMIT 1", vec![cursor.clone()]))?;
            let Some(row) = rows[0].rows.first() else {
                context.sql(&statement("UPDATE ddb_partition_index_backfills SET cursor = NULL, scanned = 1 WHERE index_id = ?1", vec![SqlValue::Text(input.index_id)]))?;
                return Ok(CommandResult::Success(Json(Outcome::Scanned)));
            };
            let [SqlValue::Blob(key)] = row.as_slice() else {
                return Err(Error::Command("invalid backfill item key"));
            };
            let item = super::command_item(context, key)?
                .ok_or(Error::Command("backfill key has no item"))?;
            bytes += serde_json::to_vec(&item)?.len();
            if let Some(id) = crate::global_index::outbox::enqueue(
                context,
                &spec.table,
                key,
                spec.epoch,
                None,
                Some(item),
            )? {
                context.sql(&statement(
                    "UPDATE ddb_index_changes SET backfill_index_id = ?1 WHERE id = ?2",
                    vec![
                        SqlValue::Text(input.index_id.clone()),
                        SqlValue::Blob(id.to_vec()),
                    ],
                ))?;
            }
            cursor = SqlValue::Blob(key.clone());
            if bytes >= 512 * 1024 {
                break;
            }
        }
        context.sql(&statement(
            "UPDATE ddb_partition_index_backfills SET cursor = ?2 WHERE index_id = ?1",
            vec![SqlValue::Text(input.index_id), cursor],
        ))?;
        Ok(CommandResult::Success(Json(Outcome::Progress)))
    }
}

/// Sealed source policy carried into an import-only split child.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InheritPartitionIndexesInput {
    pub source: PartitionSeal,
    pub state: Option<PartitionIndexState>,
}

/// Preserve schema and unfinished backfill before opening a split child.
pub struct InheritPartitionIndexes;
impl Command for InheritPartitionIndexes {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 19;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<InheritPartitionIndexesInput>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let reject = || Ok(CommandResult::Rejected(Json(false)));
        match command_state(context)? {
            Some(
                PartitionState::Importing { source, .. } | PartitionState::Activated { source, .. },
            ) if source == input.source => {}
            // Replays after Open must preserve any newer serving configuration.
            Some(PartitionState::Opened { source, .. }) if source == input.source => {
                return Ok(CommandResult::Success(Json(true)));
            }
            _ => return reject(),
        }
        let Some(state) = input.state else {
            return if policy(|batch| context.sql(batch))?.is_none() {
                Ok(CommandResult::Success(Json(true)))
            } else {
                reject()
            };
        };
        let Some(spec) = command_spec(context)? else {
            return reject();
        };
        if !state.policy.valid_for(&spec.table)
            || state
                .pending_backfills
                .iter()
                .any(|id| !state.policy.indexes.iter().any(|index| &index.id == id))
        {
            return reject();
        }
        if let Some(current) = policy(|batch| context.sql(batch))? {
            return if current == state.policy {
                Ok(CommandResult::Success(Json(true)))
            } else {
                reject()
            };
        }
        store_policy(context, &state.policy)?;
        // Hash splitting changes item-key membership. Restart each unfinished
        // scan in the child; the child's higher epoch fences duplicate images.
        for id in state.pending_backfills {
            context.sql(&statement("INSERT INTO ddb_partition_index_backfills (index_id, cursor, scanned) VALUES (?1, NULL, 0)", vec![SqlValue::Text(id)]))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}
