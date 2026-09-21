use rusqlite::{
    Connection, Transaction,
    hooks::{AuthAction, AuthContext, Authorization},
    params_from_iter,
    types::{Value, ValueRef},
};

use crate::{Error, Result};

mod api;

pub use api::{SqlBatchCommand, SqlBatchQuery, SqlCell, SqlModule, register_sql};

const MAX_STATEMENTS: usize = 128;
const MAX_ROWS: usize = 1_000;
const MAX_PARAMETERS: usize = 32_766;
const MAX_OPERATION_BYTES: usize = 1 << 20;
const MAX_RESULT_BYTES: usize = 1 << 20;

/// One typed SQLite parameter or result value.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One parameterized application SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlStatement {
    pub sql: String,
    pub parameters: Vec<SqlValue>,
}

/// A bounded group of application SQL statements executed in order.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlBatch {
    pub statements: Vec<SqlStatement>,
}

/// Materialized result of one SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
    pub rows_affected: u64,
}

#[derive(Clone, Copy)]
enum AccessMode {
    ReadOnly,
    ReadWrite,
}

/// Executes a bounded application batch inside the caller's command transaction.
///
/// Runtime and primitive tables, schema changes, connection configuration and
/// transaction control are unavailable through this interface. Any failure
/// leaves rollback of the surrounding application savepoint to the runtime.
pub fn sql_batch(transaction: &Transaction<'_>, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
    execute_batch(transaction, batch, AccessMode::ReadWrite)
}

/// Executes and materializes a bounded read-only application batch.
pub fn sql_query_batch(connection: &Connection, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
    execute_batch(connection, batch, AccessMode::ReadOnly)
}

fn execute_batch(
    connection: &Connection,
    batch: &SqlBatch,
    mode: AccessMode,
) -> Result<Vec<SqlResultSet>> {
    validate_batch(batch)?;
    let _authorizer = AuthorizerGuard::install(connection, mode);
    let mut result_sets = Vec::with_capacity(batch.statements.len());
    let mut row_count = 0_usize;
    let mut result_bytes = 0_usize;

    for item in &batch.statements {
        let values = item
            .parameters
            .iter()
            .map(to_sqlite_value)
            .collect::<Result<Vec<_>>>()?;
        let mut statement = connection.prepare(&item.sql)?;
        if statement.parameter_count() != values.len() {
            return Err(Error::Command(
                "SQL parameter count does not match statement",
            ));
        }
        let readonly = statement.readonly();
        if matches!(mode, AccessMode::ReadOnly) && !readonly {
            return Err(Error::Command("SQL query batch contains a mutation"));
        }

        if !readonly {
            if statement.column_count() != 0 {
                return Err(Error::Command("mutating SQL cannot use RETURNING"));
            }
            drop(statement);
            let changed = connection.execute(&item.sql, params_from_iter(values))?;
            let rows_affected = u64::try_from(changed)
                .map_err(|_| Error::Command("SQL affected-row count overflow"))?;
            add_size(
                &mut result_bytes,
                8,
                MAX_RESULT_BYTES,
                "SQL result exceeds 1 MiB",
            )?;
            result_sets.push(SqlResultSet {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected,
            });
            continue;
        }

        let column_count = statement.column_count();
        let columns = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for column in &columns {
            add_size(
                &mut result_bytes,
                encoded_bytes(column.len()),
                MAX_RESULT_BYTES,
                "SQL result exceeds 1 MiB",
            )?;
        }

        let mut rows = statement.query(params_from_iter(values))?;
        let mut materialized = Vec::new();
        while let Some(row) = rows.next()? {
            row_count = row_count
                .checked_add(1)
                .ok_or(Error::Command("SQL result row count overflow"))?;
            if row_count > MAX_ROWS {
                return Err(Error::Command("SQL result exceeds 1000 rows"));
            }
            let mut values = Vec::with_capacity(column_count);
            for column in 0..column_count {
                let value = from_sqlite_value(row.get_ref(column)?)?;
                add_size(
                    &mut result_bytes,
                    value.encoded_len()?,
                    MAX_RESULT_BYTES,
                    "SQL result exceeds 1 MiB",
                )?;
                values.push(value);
            }
            materialized.push(values);
        }
        result_sets.push(SqlResultSet {
            columns,
            rows: materialized,
            rows_affected: 0,
        });
    }
    Ok(result_sets)
}

fn validate_batch(batch: &SqlBatch) -> Result<()> {
    if !(1..=MAX_STATEMENTS).contains(&batch.statements.len()) {
        return Err(Error::Command("SQL batch must contain 1..=128 statements"));
    }
    let mut bytes = 0_usize;
    for statement in &batch.statements {
        if statement.sql.trim().is_empty() {
            return Err(Error::Command("SQL statement cannot be empty"));
        }
        if statement.parameters.len() > MAX_PARAMETERS {
            return Err(Error::Command(
                "SQL statement exceeds SQLite variable limit",
            ));
        }
        if has_unquoted_semicolon(&statement.sql) {
            return Err(Error::Command(
                "SQL statement cannot contain a statement separator",
            ));
        }
        add_size(
            &mut bytes,
            encoded_bytes(statement.sql.len()),
            MAX_OPERATION_BYTES,
            "SQL batch exceeds 1 MiB",
        )?;
        for value in &statement.parameters {
            add_size(
                &mut bytes,
                value.encoded_len()?,
                MAX_OPERATION_BYTES,
                "SQL batch exceeds 1 MiB",
            )?;
        }
    }
    Ok(())
}

impl SqlValue {
    fn encoded_len(&self) -> Result<usize> {
        match self {
            Self::Null => Ok(1),
            Self::Integer(_) | Self::Real(_) => Ok(9),
            Self::Text(value) => Ok(encoded_bytes(value.len())),
            Self::Blob(value) => Ok(encoded_bytes(value.len())),
        }
    }
}

fn encoded_bytes(payload: usize) -> usize {
    payload.saturating_add(5)
}

fn add_size(
    current: &mut usize,
    added: usize,
    maximum: usize,
    message: &'static str,
) -> Result<()> {
    *current = current
        .checked_add(added)
        .filter(|total| *total <= maximum)
        .ok_or(Error::Command(message))?;
    Ok(())
}

fn to_sqlite_value(value: &SqlValue) -> Result<Value> {
    Ok(match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(value) => Value::Integer(*value),
        SqlValue::Real(value) if value.is_finite() => Value::Real(*value),
        SqlValue::Real(_) => return Err(Error::Command("SQL real parameter must be finite")),
        SqlValue::Text(value) => Value::Text(value.clone()),
        SqlValue::Blob(value) => Value::Blob(value.clone()),
    })
}

fn from_sqlite_value(value: ValueRef<'_>) -> Result<SqlValue> {
    Ok(match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(value) => SqlValue::Integer(value),
        ValueRef::Real(value) if value.is_finite() => SqlValue::Real(value),
        ValueRef::Real(_) => return Err(Error::Command("SQL result contains a non-finite real")),
        ValueRef::Text(value) => SqlValue::Text(std::str::from_utf8(value)?.to_owned()),
        ValueRef::Blob(value) => SqlValue::Blob(value.to_vec()),
    })
}

struct AuthorizerGuard<'a> {
    connection: &'a Connection,
}

impl<'a> AuthorizerGuard<'a> {
    fn install(connection: &'a Connection, mode: AccessMode) -> Self {
        let authorizer: fn(AuthContext<'_>) -> Authorization = match mode {
            AccessMode::ReadOnly => authorize_read,
            AccessMode::ReadWrite => authorize_write,
        };
        connection.authorizer(Some(authorizer));
        Self { connection }
    }
}

impl Drop for AuthorizerGuard<'_> {
    fn drop(&mut self) {
        self.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    }
}

fn authorize_read(context: AuthContext<'_>) -> Authorization {
    authorize(context, AccessMode::ReadOnly)
}

fn authorize_write(context: AuthContext<'_>) -> Authorization {
    authorize(context, AccessMode::ReadWrite)
}

fn authorize(context: AuthContext<'_>, mode: AccessMode) -> Authorization {
    if context.database_name.is_some_and(|name| name != "main") {
        return Authorization::Deny;
    }
    if context.accessor.is_some_and(is_protected_name) {
        return Authorization::Deny;
    }

    match context.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read { table_name, .. } if !is_protected_name(table_name) => {
            Authorization::Allow
        }
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. }
            if matches!(mode, AccessMode::ReadWrite) && !is_protected_name(table_name) =>
        {
            Authorization::Allow
        }
        AuthAction::Function { function_name }
            if !function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

fn is_protected_name(name: &str) -> bool {
    ["sys_", "kv_", "queue_", "workflow_", "blob_", "cron_"]
        .iter()
        .any(|prefix| starts_with_ignore_ascii_case(name, prefix))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn has_unquoted_semicolon(sql: &str) -> bool {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut index = 0_usize;
    while index < bytes.len() {
        let current = bytes[index];
        let next = bytes.get(index + 1).copied();
        match state {
            State::Normal => match (current, next) {
                (b';', _) => return true,
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    index += 1;
                }
                _ => {}
            },
            State::Single if current == b'\'' => {
                if next == Some(b'\'') {
                    index += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::Double if current == b'"' => {
                if next == Some(b'"') {
                    index += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::Backtick if current == b'`' => {
                if next == Some(b'`') {
                    index += 1;
                } else {
                    state = State::Normal;
                }
            }
            State::Bracket if current == b']' => state = State::Normal,
            State::LineComment if current == b'\n' || current == b'\r' => state = State::Normal,
            State::BlockComment if current == b'*' && next == Some(b'/') => {
                state = State::Normal;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    false
}
