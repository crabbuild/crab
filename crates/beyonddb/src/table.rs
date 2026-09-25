use super::*;
use extenddb_core::types::Tag;

/// An ExtendDB table's key contract stored in the account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableSpec {
    /// Table name unique within the account.
    pub table_name: String,
    /// Primary key schema already validated by ExtendDB's operation engine.
    pub key_schema: Vec<KeySchemaElement>,
    /// Definitions of the primary key attributes.
    pub attribute_definitions: Vec<AttributeDefinition>,
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
        self.table_name == record.table_name
            && self.key_schema == record.key_schema
            && self.attribute_definitions == record.attribute_definitions
            && self.billing_mode == record.billing_mode
            && self.provisioned_throughput == record.provisioned_throughput
            && self.deletion_protection_enabled == record.deletion_protection_enabled
    }
}

/// Outcome of creating a table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CreateTableOutcome {
    /// The table was created with this fresh, immutable ID.
    Created(TableRecord),
    /// A table already owns this name.
    AlreadyExists,
    /// The key contract was invalid.
    InvalidSchema,
}

/// Durable identity and key contract for one account table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableRecord {
    /// Fresh ID assigned by the account Cell.
    pub id: String,
    /// Logical creation time assigned by the Cell runtime.
    pub created_at_ms: i64,
    /// Account-unique table name.
    pub table_name: String,
    /// Primary key schema.
    pub key_schema: Vec<KeySchemaElement>,
    /// Definitions of primary key attributes.
    pub attribute_definitions: Vec<AttributeDefinition>,
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
            id: table_id.clone(),
            created_at_ms: context.now_ms(),
            table_name: input.table_name.clone(),
            key_schema: input.key_schema.clone(),
            attribute_definitions: input.attribute_definitions.clone(),
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
            record,
        ))))
    }
}

/// Result of removing a table and all its items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeleteTableOutcome {
    /// The removed table's last description.
    Deleted(TableRecord),
    /// No table has this name.
    TableNotFound,
    /// The table's deletion protection is enabled.
    DeletionProtected,
}

/// Remove a table and all its items in one durable command.
pub struct DeleteTable;

impl Command for DeleteTable {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<DeleteTableOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(name): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(table) = command_table(context, &name)? else {
            return Ok(CommandResult::Rejected(Json(
                DeleteTableOutcome::TableNotFound,
            )));
        };
        if table.deletion_protection_enabled {
            return Ok(CommandResult::Rejected(Json(
                DeleteTableOutcome::DeletionProtected,
            )));
        }
        context.sql(&SqlBatch {
            statements: vec![
                SqlStatement {
                    sql: "DELETE FROM ddb_route_partitions WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
                SqlStatement {
                    sql: "DELETE FROM ddb_split_plans WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
                SqlStatement {
                    sql: "DELETE FROM ddb_routes WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
                SqlStatement {
                    sql: "DELETE FROM ddb_items WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
                SqlStatement {
                    sql: "DELETE FROM ddb_table_tags WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
                SqlStatement {
                    sql: "DELETE FROM ddb_tables WHERE table_id = ?1".into(),
                    parameters: vec![SqlValue::Text(table.id.clone())],
                },
            ],
        })?;
        Ok(CommandResult::Success(Json(DeleteTableOutcome::Deleted(
            table,
        ))))
    }
}

/// Supported table settings changed atomically in the account Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableUpdate {
    /// Table to update.
    pub table_name: String,
    /// Billing mode transition, if requested.
    pub billing_mode: Option<BillingMode>,
    /// New capacity units for provisioned billing, if requested.
    pub provisioned_throughput: Option<ProvisionedThroughput>,
    /// New deletion-protection setting, if requested.
    pub deletion_protection_enabled: Option<bool>,
}

/// Outcome of updating supported table settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateTableOutcome {
    /// The durable table record after the update.
    Updated(TableRecord),
    /// No table has this name.
    TableNotFound,
    /// Billing and capacity settings are inconsistent.
    InvalidUpdate,
}

/// Update table billing and deletion protection in one durable command.
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
        let Some(mut table) = command_table(context, &input.table_name)? else {
            return Ok(CommandResult::Rejected(Json(
                UpdateTableOutcome::TableNotFound,
            )));
        };
        if let Some(mode) = input.billing_mode {
            if mode != table.billing_mode {
                table.pay_per_request_since_ms =
                    (mode == BillingMode::PayPerRequest).then_some(context.now_ms());
            }
            table.billing_mode = mode;
            if mode == BillingMode::PayPerRequest {
                table.provisioned_throughput = None;
            }
        }
        if let Some(capacity) = input.provisioned_throughput {
            table.provisioned_throughput = Some(capacity);
        }
        if let Some(enabled) = input.deletion_protection_enabled {
            table.deletion_protection_enabled = enabled;
        }
        let spec = TableSpec {
            table_name: table.table_name.clone(),
            key_schema: table.key_schema.clone(),
            attribute_definitions: table.attribute_definitions.clone(),
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
            table,
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
                "SELECT record FROM ddb_tables WHERE table_id = ?1",
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
            "SELECT record FROM ddb_tables WHERE table_name = ?1",
            vec![SqlValue::Text(name.to_owned())],
        ))?[0],
    )
}

pub(super) fn query_table(context: &QueryContext<'_>, name: &str) -> Result<Option<TableRecord>> {
    decode_table(
        &context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_name = ?1",
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
            "SELECT t.record FROM ddb_tables t LEFT JOIN ddb_routes r ON t.table_id = r.table_id \
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
            "SELECT t.record FROM ddb_tables t LEFT JOIN ddb_routes r ON t.table_id = r.table_id \
             WHERE t.table_name = ?1 AND r.table_id IS NULL",
            vec![SqlValue::Text(name.to_owned())],
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
    let input = CreateTableInput {
        table_name: spec.table_name.clone(),
        key_schema: spec.key_schema.clone(),
        attribute_definitions: spec.attribute_definitions.clone(),
        billing_mode: Some(spec.billing_mode),
        provisioned_throughput: spec.provisioned_throughput.clone(),
        deletion_protection_enabled: Some(spec.deletion_protection_enabled),
        tags: Some(spec.initial_tags.clone()),
        ..CreateTableInput::default()
    };
    validation::validate_create_table(&input, &LimitsConfig::default()).is_ok()
}
