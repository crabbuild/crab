mod deletion;
#[cfg(test)]
mod tests;

pub use deletion::{
    ContinueTableDeletion, DeleteTable, DeleteTableOutcome, PendingDirectoryRetirement,
    ReadPendingDirectoryRetirement, ReadTableLifecycle, RecordTableDirectoryRetirement,
    TableDirectoryRetirement, TableGeneration, TableLifecycle,
};

use super::*;
use extenddb_core::types::{LsiInput, Tag};

/// Table-wide pricing class, shared by the table's secondary indexes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableClass {
    #[default]
    #[serde(rename = "STANDARD")]
    Standard,
    #[serde(rename = "STANDARD_INFREQUENT_ACCESS")]
    StandardInfrequentAccess,
}

impl TryFrom<&str> for TableClass {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "STANDARD" => Ok(Self::Standard),
            "STANDARD_INFREQUENT_ACCESS" => Ok(Self::StandardInfrequentAccess),
            _ => Err(Error::Command("invalid table class")),
        }
    }
}

/// Immutable placement selected before a table's first range is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TablePlacement {
    /// Items are stored directly in the account Cell.
    Account,
    /// Base and index directories start with this many ranges each.
    Routed { initial_partitions: u16 },
}

/// An ExtendDB table's key contract stored in the account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableSpec {
    /// Initial table class.
    pub table_class: TableClass,
    /// Placement policy persisted with the new generation.
    pub placement: TablePlacement,
    /// Table name unique within the account.
    pub table_name: String,
    /// Primary key schema already validated by ExtendDB's operation engine.
    pub key_schema: Vec<KeySchemaElement>,
    /// Definitions of base and index key attributes.
    pub attribute_definitions: Vec<AttributeDefinition>,
    /// Immutable local secondary indexes maintained with each base item.
    pub local_secondary_indexes: Vec<LsiInput>,
    /// Global index definitions requested at table creation.
    pub global_secondary_indexes: Vec<extenddb_core::types::GsiInput>,
    /// Table billing mode.
    pub billing_mode: BillingMode,
    /// Capacity units for provisioned billing.
    pub provisioned_throughput: Option<ProvisionedThroughput>,
    /// Whether DeleteTable must be refused.
    pub deletion_protection_enabled: bool,
    /// Initial resource tags committed with table creation.
    pub initial_tags: Vec<Tag>,
    /// Canonical table ARN required when initial tags are supplied.
    pub resource_arn: Option<String>,
}

impl TableSpec {
    pub(crate) fn matches_record(&self, record: &TableRecord) -> bool {
        // Placement is service policy. A matching user retry keeps the original
        // generation's policy even if the serving node's configuration changed.
        self.table_name == record.table_name
            && self.table_class == record.table_class
            && self.key_schema == record.key_schema
            && self.attribute_definitions == record.attribute_definitions
            && self.local_secondary_indexes == record.local_secondary_indexes
            && self.global_secondary_indexes.iter().eq(record
                .global_secondary_indexes
                .iter()
                .map(|index| &index.specification))
            && self.billing_mode == record.billing_mode
            && self.provisioned_throughput == record.provisioned_throughput
            && self.deletion_protection_enabled == record.deletion_protection_enabled
    }
}

/// Outcome of creating a table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CreateTableOutcome {
    /// The table was created with this fresh, immutable ID.
    Created(Box<TableRecord>),
    /// A table already owns this name.
    AlreadyExists,
    /// The key contract was invalid.
    InvalidSchema,
}

/// Durable identity and key contract for one account table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableRecord {
    /// Current table class for this generation and all its indexes.
    pub table_class: TableClass,
    /// At most two class changes retained for the trailing thirty-day limit.
    pub table_class_updates_ms: Vec<i64>,
    /// Original placement policy, unaffected by later server configuration.
    pub placement: TablePlacement,
    /// Fresh ID assigned by the account Cell.
    pub id: String,
    /// Logical creation time assigned by the Cell runtime.
    pub created_at_ms: i64,
    /// Account-unique table name.
    pub table_name: String,
    /// Primary key schema.
    pub key_schema: Vec<KeySchemaElement>,
    /// Definitions of base and index key attributes.
    pub attribute_definitions: Vec<AttributeDefinition>,
    /// Immutable local secondary indexes maintained with each base item.
    pub local_secondary_indexes: Vec<LsiInput>,
    /// Immutable global-index generations owned by this table.
    pub global_secondary_indexes: Vec<crate::GlobalIndexRecord>,
    /// Persisted table billing mode.
    pub billing_mode: BillingMode,
    /// Persisted provisioned capacity, if applicable.
    pub provisioned_throughput: Option<ProvisionedThroughput>,
    /// Whether this table resists deletion.
    pub deletion_protection_enabled: bool,
    /// Logical time when the current on-demand mode started.
    pub pay_per_request_since_ms: Option<i64>,
}

/// Create one table and its key contract atomically.
pub struct CreateTable;

impl Command for CreateTable {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TableSpec>;
    type Output = Json<CreateTableOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !valid_table_spec(&input) {
            return Ok(CommandResult::Rejected(Json(
                CreateTableOutcome::InvalidSchema,
            )));
        }
        if !context.sql(&statement(
            "SELECT table_id FROM ddb_tables WHERE table_name = ?1",
            vec![SqlValue::Text(input.table_name.clone())],
        ))?[0]
            .rows
            .is_empty()
        {
            return Ok(CommandResult::Rejected(Json(
                CreateTableOutcome::AlreadyExists,
            )));
        }
        let table_id = table_id(context, &input.table_name);
        if !input.initial_tags.is_empty() {
            let Some(arn) = input.resource_arn.as_deref() else {
                return Ok(CommandResult::Rejected(Json(
                    CreateTableOutcome::InvalidSchema,
                )));
            };
            let Some((_, account_id, table_name)) = crate::tags::parse_table_arn(arn) else {
                return Ok(CommandResult::Rejected(Json(
                    CreateTableOutcome::InvalidSchema,
                )));
            };
            if table_name != input.table_name || account_target(account_id)? != *context.target() {
                return Ok(CommandResult::Rejected(Json(
                    CreateTableOutcome::InvalidSchema,
                )));
            }
        }
        let record = TableRecord {
            table_class: input.table_class,
            table_class_updates_ms: Vec::new(),
            placement: input.placement,
            id: table_id.clone(),
            created_at_ms: context.now_ms(),
            table_name: input.table_name.clone(),
            key_schema: input.key_schema.clone(),
            attribute_definitions: input.attribute_definitions.clone(),
            local_secondary_indexes: input.local_secondary_indexes.clone(),
            global_secondary_indexes: input
                .global_secondary_indexes
                .iter()
                .cloned()
                .map(|spec| crate::GlobalIndexRecord::create(&table_id, spec, context.sequence()))
                .collect::<Result<Vec<_>>>()?,
            billing_mode: input.billing_mode,
            provisioned_throughput: input.provisioned_throughput,
            deletion_protection_enabled: input.deletion_protection_enabled,
            pay_per_request_since_ms: (input.billing_mode == BillingMode::PayPerRequest)
                .then_some(context.now_ms()),
        };
        context.sql(&statement(
            "INSERT INTO ddb_tables (table_name, table_id, record) VALUES (?1, ?2, ?3)",
            vec![
                SqlValue::Text(input.table_name),
                SqlValue::Text(table_id),
                SqlValue::Blob(serde_json::to_vec(&record)?),
            ],
        ))?;
        if matches!(record.placement, TablePlacement::Routed { .. }) {
            // Persist lifecycle ownership before provisioning independent roots.
            // Deletion must fence even an installer that never publishes its copy.
            for id in std::iter::once(&record.id).chain(
                record
                    .global_secondary_indexes
                    .iter()
                    .map(|index| &index.id),
            ) {
                context.sql(&statement(
                    "INSERT INTO ddb_directory_roots (table_id, base_table_id) VALUES (?1, ?2)",
                    vec![
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(record.id.clone()),
                    ],
                ))?;
            }
        }
        if let Some(arn) = input.resource_arn {
            for tag in input.initial_tags {
                context.sql(&statement(
                    "INSERT INTO ddb_table_tags (table_id, resource_arn, tag_key, tag_value) \
                     VALUES (?1, ?2, ?3, ?4)",
                    vec![
                        SqlValue::Text(record.id.clone()),
                        SqlValue::Text(arn.clone()),
                        SqlValue::Text(tag.key),
                        SqlValue::Text(tag.value),
                    ],
                ))?;
            }
        }
        Ok(CommandResult::Success(Json(CreateTableOutcome::Created(
            Box::new(record),
        ))))
    }
}

/// Table class, billing and deletion settings applied with one account mutation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TableSettings {
    pub table_class: Option<TableClass>,
    pub billing_mode: Option<BillingMode>,
    pub provisioned_throughput: Option<ProvisionedThroughput>,
    pub deletion_protection_enabled: Option<bool>,
}

impl TableSettings {
    #[must_use]
    fn apply(self, table: &mut TableRecord, now_ms: i64) -> bool {
        if let Some(class) = self.table_class
            && class != table.table_class
        {
            // Keep this limit in the account command so concurrent updates and
            // owner replacement cannot reset or overspend the trailing window.
            let cutoff = now_ms.saturating_sub(30 * 24 * 60 * 60 * 1_000);
            table.table_class_updates_ms.retain(|time| *time > cutoff);
            if table.table_class_updates_ms.len() >= 2 {
                return false;
            }
            table.table_class_updates_ms.push(now_ms);
            table.table_class = class;
        }
        if let Some(mode) = self.billing_mode {
            if mode != table.billing_mode {
                table.pay_per_request_since_ms =
                    (mode == BillingMode::PayPerRequest).then_some(now_ms);
            }
            table.billing_mode = mode;
            if mode == BillingMode::PayPerRequest {
                table.provisioned_throughput = None;
            }
        }
        if let Some(capacity) = self.provisioned_throughput {
            table.provisioned_throughput = Some(capacity);
        }
        if let Some(enabled) = self.deletion_protection_enabled {
            table.deletion_protection_enabled = enabled;
        }
        true
    }
}

/// Supported table settings changed atomically in the account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableUpdate {
    pub table_name: String,
    pub settings: TableSettings,
}

/// Outcome of updating supported table settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateTableOutcome {
    /// The durable table record after the update.
    Updated(Box<TableRecord>),
    /// No table has this name.
    TableNotFound,
    /// Initial routed placement has not been published yet.
    TableNotActive,
    /// Billing and capacity settings are inconsistent.
    InvalidUpdate,
    /// The table has already changed class twice in the trailing thirty days.
    TableClassLimitExceeded,
}

/// Update table class, billing and deletion protection in one durable command.
pub struct UpdateTable;

impl Command for UpdateTable {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TableUpdate>;
    type Output = Json<UpdateTableOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let mut table = match deletion::command_lifecycle(context, &input.table_name)? {
            TableLifecycle::Live(table) => table,
            TableLifecycle::Deleting(_) => {
                return Ok(CommandResult::Rejected(Json(
                    UpdateTableOutcome::TableNotActive,
                )));
            }
            TableLifecycle::Missing => {
                return Ok(CommandResult::Rejected(Json(
                    UpdateTableOutcome::TableNotFound,
                )));
            }
        };
        // Initial owners persist this record verbatim. Keep it immutable until
        // route publication so a retry cannot conflict with installed owners.
        if matches!(table.placement, TablePlacement::Routed { .. })
            && context.sql(&statement(
                "SELECT 1 FROM ddb_directory_roots WHERE table_id = ?1 AND initial_fingerprint IS NOT NULL",
                vec![SqlValue::Text(table.id.clone())],
            ))?[0]
                .rows
                .is_empty()
        {
            return Ok(CommandResult::Rejected(Json(
                UpdateTableOutcome::TableNotActive,
            )));
        }
        if !input.settings.apply(&mut table, context.now_ms()) {
            return Ok(CommandResult::Rejected(Json(
                UpdateTableOutcome::TableClassLimitExceeded,
            )));
        }
        let spec = TableSpec {
            table_class: table.table_class,
            placement: table.placement,
            table_name: table.table_name.clone(),
            key_schema: table.key_schema.clone(),
            attribute_definitions: table.attribute_definitions.clone(),
            local_secondary_indexes: table.local_secondary_indexes.clone(),
            global_secondary_indexes: table
                .global_secondary_indexes
                .iter()
                .map(|index| index.specification.clone())
                .collect(),
            billing_mode: table.billing_mode,
            provisioned_throughput: table.provisioned_throughput.clone(),
            deletion_protection_enabled: table.deletion_protection_enabled,
            initial_tags: Vec::new(),
            resource_arn: None,
        };
        if !valid_table_spec(&spec) {
            return Ok(CommandResult::Rejected(Json(
                UpdateTableOutcome::InvalidUpdate,
            )));
        }
        context.sql(&statement(
            "UPDATE ddb_tables SET record = ?1 WHERE table_id = ?2",
            vec![
                SqlValue::Blob(serde_json::to_vec(&table)?),
                SqlValue::Text(table.id.clone()),
            ],
        ))?;
        Ok(CommandResult::Success(Json(UpdateTableOutcome::Updated(
            Box::new(table),
        ))))
    }
}

/// Read one table's durable identity and key contract.
pub struct DescribeTable;

impl Query for DescribeTable {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<TableRecord>>;

    fn execute(context: &mut QueryContext<'_>, Json(name): Self::Input) -> Result<Self::Output> {
        Ok(Json(query_table(context, &name)?))
    }
}

/// Read a table by its immutable ID.
pub struct DescribeTableById;

impl Query for DescribeTableById {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<TableRecord>>;

    fn execute(context: &mut QueryContext<'_>, Json(id): Self::Input) -> Result<Self::Output> {
        Ok(Json(decode_table(
            &context.sql(&statement(
                "SELECT record FROM ddb_live_tables WHERE table_id = ?1",
                vec![SqlValue::Text(id)],
            ))?[0],
        )?))
    }
}

/// Bounds an account-local table listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListTablesInput {
    /// Number of table names to return, from one to one hundred.
    pub limit: i64,
    /// Resume strictly after this table name.
    pub exclusive_start: Option<String>,
}

/// One page of table names in binary UTF-8 order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListTablesPage {
    /// Table names in ascending order.
    pub names: Vec<String>,
    /// Last returned name when another page exists.
    pub last_evaluated: Option<String>,
}

/// Result of listing account tables.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ListTablesOutcome {
    /// A valid page.
    Page(ListTablesPage),
    /// The requested page size is outside one through one hundred.
    InvalidLimit,
}

/// List account tables from a consistent Cell snapshot.
pub struct ListTables;

impl Query for ListTables {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ListTablesInput>;
    type Output = Json<ListTablesOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if !(1..=100).contains(&input.limit) {
            return Ok(Json(ListTablesOutcome::InvalidLimit));
        }
        let start = input.exclusive_start.map_or(SqlValue::Null, SqlValue::Text);
        let result = context.sql(&statement(
            "SELECT table_name FROM ddb_tables WHERE (?1 IS NULL OR table_name > ?1) ORDER BY table_name LIMIT ?2",
            vec![start, SqlValue::Integer(input.limit + 1)],
        ))?;
        let mut names = Vec::with_capacity(result[0].rows.len());
        for row in &result[0].rows {
            let [SqlValue::Text(name)] = row.as_slice() else {
                return Err(Error::Command("invalid table name row"));
            };
            names.push(name.clone());
        }
        let limit = usize::try_from(input.limit)
            .map_err(|_| Error::Command("invalid table listing limit"))?;
        let last_evaluated = if names.len() > limit {
            names.pop();
            names.last().cloned()
        } else {
            None
        };
        Ok(Json(ListTablesOutcome::Page(ListTablesPage {
            names,
            last_evaluated,
        })))
    }
}

pub(super) fn command_table(
    context: &CommandContext<'_, '_>,
    name: &str,
) -> Result<Option<TableRecord>> {
    decode_table(
        &context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_name = ?1",
            vec![SqlValue::Text(name.to_owned())],
        ))?[0],
    )
}

pub(super) fn query_table(context: &QueryContext<'_>, name: &str) -> Result<Option<TableRecord>> {
    decode_table(
        &context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_name = ?1",
            vec![SqlValue::Text(name.to_owned())],
        ))?[0],
    )
}

pub(super) fn command_unrouted_table(
    context: &CommandContext<'_, '_>,
    name: &str,
) -> Result<Option<TableRecord>> {
    // Route publication transfers item authority; account writes must fail closed.
    decode_table(
        &context.sql(&statement(
            "SELECT t.record FROM ddb_live_tables t LEFT JOIN ddb_directory_roots r ON t.table_id = r.table_id AND r.initial_fingerprint IS NOT NULL \
             WHERE t.table_name = ?1 AND r.table_id IS NULL",
            vec![SqlValue::Text(name.to_owned())],
        ))?[0],
    )
}

pub(super) fn query_unrouted_table(
    context: &QueryContext<'_>,
    name: &str,
) -> Result<Option<TableRecord>> {
    // The account item image is no longer authoritative after activation.
    decode_table(
        &context.sql(&statement(
            "SELECT t.record FROM ddb_live_tables t LEFT JOIN ddb_directory_roots r ON t.table_id = r.table_id AND r.initial_fingerprint IS NOT NULL \
             WHERE t.table_name = ?1 AND r.table_id IS NULL",
            vec![SqlValue::Text(name.to_owned())],
        ))?[0],
    )
}

pub(super) fn query_unrouted_table_by_id(
    context: &QueryContext<'_>,
    id: &str,
) -> Result<Option<TableRecord>> {
    decode_table(
        &context.sql(&statement(
            "SELECT t.record FROM ddb_live_tables t LEFT JOIN ddb_directory_roots r ON t.table_id = r.table_id AND r.initial_fingerprint IS NOT NULL \
             WHERE t.table_id = ?1 AND r.table_id IS NULL",
            vec![SqlValue::Text(id.to_owned())],
        ))?[0],
    )
}

pub(super) fn decode_table(result: &SqlResultSet) -> Result<Option<TableRecord>> {
    let Some(row) = result.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(record)] = row.as_slice() else {
        return Err(Error::Command("invalid account table row"));
    };
    Ok(Some(serde_json::from_slice(record)?))
}

pub(super) fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlBatch {
    SqlBatch {
        statements: vec![SqlStatement {
            sql: sql.to_owned(),
            parameters,
        }],
    }
}

fn table_id(context: &CommandContext<'_, '_>, name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"beyonddb.table.v1\0");
    hasher.update(context.cell_id().as_bytes());
    hasher.update(&context.sequence().to_be_bytes());
    hasher.update(name.as_bytes());
    let mut id = [0_u8; 32];
    id[..16].copy_from_slice(context.target().tenant().as_bytes());
    id[16..].copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    blake3::Hash::from_bytes(id).to_hex().to_string()
}

fn valid_table_spec(spec: &TableSpec) -> bool {
    if let TablePlacement::Routed { initial_partitions } = spec.placement
        && (initial_partitions == 0
            || initial_partitions > 256
            || !initial_partitions.is_power_of_two())
    {
        return false;
    }
    let input = CreateTableInput {
        table_name: spec.table_name.clone(),
        key_schema: spec.key_schema.clone(),
        attribute_definitions: spec.attribute_definitions.clone(),
        local_secondary_indexes: Some(spec.local_secondary_indexes.clone()),
        global_secondary_indexes: Some(spec.global_secondary_indexes.clone()),
        billing_mode: Some(spec.billing_mode),
        provisioned_throughput: spec.provisioned_throughput.clone(),
        deletion_protection_enabled: Some(spec.deletion_protection_enabled),
        tags: Some(spec.initial_tags.clone()),
        ..CreateTableInput::default()
    };
    validation::validate_create_table(&input, &LimitsConfig::default()).is_ok()
        && spec.local_secondary_indexes.iter().all(|index| {
            index.projection.projection_type == extenddb_core::types::ProjectionType::All
        })
}
