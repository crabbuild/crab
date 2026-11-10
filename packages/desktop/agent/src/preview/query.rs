use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use duckdb::arrow::{datatypes::DataType, util::display::array_value_to_string};
use rusqlite::{
    Connection, OpenFlags,
    hooks::{AuthAction, Authorization},
    limits::Limit,
    types::ValueRef,
};
use serde_json::{Map, Value, json};

use crate::{
    AgentError, Result,
    repository::{FileSource, ReadScope},
};

use super::parse_error;

const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_COLUMNS: usize = 256;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
static SNAPSHOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(crate) enum Job {
    SqlitePreview,
    Sqlite { sql: String, max_rows: usize },
    Parquet { sql: String, max_rows: usize },
}

impl Job {
    pub(crate) fn query(path: &str, sql: String, max_rows: Option<usize>) -> Result<Self> {
        if sql.trim().is_empty() || sql.len() > MAX_SQL_BYTES {
            return Err(AgentError::Other(
                "Enter a SQL statement of at most 64 KiB".into(),
            ));
        }
        let max_rows = max_rows.unwrap_or(500).clamp(1, 1000);
        let ext = Path::new(path).extension().and_then(|ext| ext.to_str());
        match ext.map(str::to_ascii_lowercase).as_deref() {
            Some("parquet" | "pq") => Ok(Self::Parquet { sql, max_rows }),
            Some("db" | "sqlite" | "sqlite3") => Ok(Self::Sqlite { sql, max_rows }),
            _ => Err(AgentError::Other(
                "SQL requires a Parquet or SQLite file".into(),
            )),
        }
    }
}

pub(crate) async fn run(source: Arc<FileSource>, job: Job) -> Result<Value> {
    let scope = source.scope();
    let interrupt = Arc::new(OnceLock::<Arc<duckdb::InterruptHandle>>::new());
    let work = execute(source, job, interrupt.clone());
    tokio::pin!(work);
    let timed_out = tokio::select! {
        result = &mut work => return result,
        _ = scope.cancelled() => false,
        () = tokio::time::sleep(QUERY_TIMEOUT) => true,
    };
    scope.cancel();
    // DuckDB clears an interrupt when execution starts. Keep signalling
    // through that transition and join the worker before releasing its file.
    let result = loop {
        if let Some(handle) = interrupt.get() {
            handle.interrupt();
        }
        tokio::select! {
            result = &mut work => break result,
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    };
    if matches!(result, Err(AgentError::Cleanup { .. } | AgentError::Io(_))) {
        return result;
    }
    if timed_out {
        return Err(AgentError::Other(
            "Database operation exceeded its 60-second budget".into(),
        ));
    }
    scope.check()?;
    result
}

async fn execute(
    source: Arc<FileSource>,
    job: Job,
    interrupt: Arc<OnceLock<Arc<duckdb::InterruptHandle>>>,
) -> Result<Value> {
    if source.size > MAX_SNAPSHOT_BYTES {
        return Err(AgentError::Other(
            "Database input exceeds the 8 GiB snapshot budget".into(),
        ));
    }
    let scope = source.scope();
    let _slot = tokio::select! {
        permit = SNAPSHOTS.acquire() => permit.map_err(std::io::Error::other)?,
        error = scope.cancelled() => return Err(error),
    };
    let directory = crab_cache::default_cache_root().join("desktop-content");
    crab_cache::ensure_private_cache_directory(&directory)?;
    let file = tempfile::Builder::new()
        .prefix("crab-query-")
        .tempfile_in(directory)?;
    let input = async {
        source
            .copy_complete_to_file(file.as_file().try_clone()?)
            .await
    }
    .await;
    let result = match input {
        Err(error) => Err(error),
        Ok(()) => {
            let path = file.path().to_owned();
            let size = source.size;
            tokio::task::spawn_blocking(move || {
                scope.check()?;
                match job {
                    Job::Parquet { sql, max_rows } => {
                        parquet_query(&path, &sql, max_rows, &scope, &interrupt)
                    }
                    Job::Sqlite { sql, max_rows } => {
                        let conn = sqlite(&path, &scope)?;
                        let (columns, rows) = sqlite_rows(&conn, &sql, max_rows, &scope)?;
                        Ok(rows.result(columns))
                    }
                    Job::SqlitePreview => sqlite_preview(&sqlite(&path, &scope)?, size, &scope),
                }
            })
            .await
            .map_err(|error| AgentError::Io(std::io::Error::other(error)))
            .and_then(|result| result)
        }
    };
    match (result, file.close()) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(cleanup.into()),
        (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup.into()),
        }),
    }
}

struct Rows {
    rows: Vec<Value>,
    bytes: usize,
    limit: usize,
    truncated: bool,
}

impl Rows {
    fn new(limit: usize, columns: &[Value]) -> Result<Self> {
        let bytes = serde_json::to_vec(columns)?.len();
        if columns.len() > MAX_COLUMNS || bytes > MAX_RESULT_BYTES {
            return Err(AgentError::Other(
                "Query result has too many columns or oversized column names".into(),
            ));
        }
        let mut names = HashSet::with_capacity(columns.len());
        if columns
            .iter()
            .any(|column| !names.insert(column["name"].as_str()))
        {
            return Err(AgentError::Other(
                "Give result columns unique names using AS aliases".into(),
            ));
        }
        Ok(Self {
            rows: Vec::new(),
            bytes,
            limit,
            truncated: false,
        })
    }

    fn push(&mut self, row: Value) -> Result<bool> {
        let bytes = serde_json::to_vec(&row)?.len() + 1;
        if self.rows.len() >= self.limit || self.bytes + bytes > MAX_RESULT_BYTES {
            if self.rows.is_empty() {
                return Err(AgentError::Other(
                    "A query row exceeds the 2 MiB result budget".into(),
                ));
            }
            self.truncated = true;
            return Ok(false);
        }
        self.rows.push(row);
        self.bytes += bytes;
        Ok(true)
    }

    fn result(self, columns: Vec<Value>) -> Value {
        json!({
            "kind": "file_query", "columns": columns, "row_count": self.rows.len(),
            "rows": self.rows, "truncated": self.truncated, "max_rows": self.limit,
        })
    }
}

fn parquet_query(
    path: &Path,
    sql: &str,
    max_rows: usize,
    scope: &ReadScope,
    interrupt: &OnceLock<Arc<duckdb::InterruptHandle>>,
) -> Result<Value> {
    let error = |error| parse_error("Parquet SQL", error);
    let config = duckdb::Config::default()
        .enable_autoload_extension(false)
        .map_err(error)?
        .threads(1)
        .map_err(error)?
        .max_memory("256MiB")
        .map_err(error)?
        .with("max_temp_directory_size", "0B")
        .map_err(error)?;
    let conn = duckdb::Connection::open_in_memory_with_flags(config).map_err(error)?;
    let _ = interrupt.set(conn.interrupt_handle());
    let path = path
        .to_str()
        .ok_or_else(|| AgentError::Other("Query snapshot path is not UTF-8".into()))?;
    let literal = format!("'{}'", path.replace('\'', "''"));
    // Only the owned snapshot is in DuckDB's allow-list. Seal the engine
    // settings before accepting SQL so it cannot open other files or spill.
    conn.execute_batch(&format!(
        "SET allowed_paths = [{literal}]; SET enable_external_access = false; \
         SET allow_persistent_secrets = false; SET lock_configuration = true; \
         CREATE VIEW data AS SELECT * FROM read_parquet({literal});"
    ))
    .map_err(error)?;
    scope.check()?;
    let mut statement = conn.prepare(sql).map_err(error)?;
    scope.check()?;
    let mut stream = statement.stream_arrow([]).map_err(error)?;
    let schema = stream.get_schema();
    let columns = schema
        .fields()
        .iter()
        .map(|field| {
            json!({
                "name": field.name(), "type": field.data_type().to_string(),
            })
        })
        .collect::<Vec<_>>();
    let mut output = Rows::new(max_rows, &columns)?;
    'batches: for batch in &mut stream {
        scope.check()?;
        let batch = batch.map_err(error)?;
        if batch.get_array_memory_size() > MAX_BATCH_BYTES {
            return Err(AgentError::Other(
                "Decoded query batch exceeds 16 MiB".into(),
            ));
        }
        for index in 0..batch.num_rows() {
            let mut row = Map::new();
            for (field, array) in schema.fields().iter().zip(batch.columns()) {
                let value = if array.is_null(index) {
                    Value::Null
                } else {
                    let text = array_value_to_string(array.as_ref(), index)
                        .map_err(|error| parse_error("Query value", error))?;
                    match array.data_type() {
                        DataType::Float16
                        | DataType::Float32
                        | DataType::Float64
                        | DataType::Boolean => {
                            serde_json::from_str(&text).unwrap_or(Value::String(text))
                        }
                        kind if kind.is_integer() => match text.parse::<i64>() {
                            Ok(number) => integer(number),
                            Err(_) => Value::String(text),
                        },
                        _ => Value::String(text),
                    }
                };
                row.insert(field.name().clone(), value);
            }
            if !output.push(Value::Object(row))? {
                break 'batches;
            }
        }
    }
    Ok(output.result(columns))
}

fn integer(number: i64) -> Value {
    if number.unsigned_abs() <= MAX_SAFE_INTEGER {
        json!(number)
    } else {
        Value::String(number.to_string())
    }
}

fn sqlite(path: &Path, scope: &ReadScope) -> Result<Connection> {
    let mut uri = url::Url::from_file_path(path)
        .map_err(|()| AgentError::Other("Invalid SQLite snapshot path".into()))?;
    uri.set_query(Some("immutable=1"));
    let error = |error| parse_error("SQLite", error);
    let conn = Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(error)?;
    conn.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-8192;",
    )
    .map_err(error)?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_RESULT_BYTES as i32)
        .map_err(error)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS as i32)
        .map_err(error)?;
    let scope = scope.clone();
    conn.progress_handler(1000, Some(move || scope.check().is_err()))
        .map_err(error)?;
    conn.authorizer(Some(
        |context: rusqlite::hooks::AuthContext<'_>| match context.action {
            AuthAction::Read { .. }
            | AuthAction::Select
            | AuthAction::Function { .. }
            | AuthAction::Recursive => Authorization::Allow,
            AuthAction::Pragma { pragma_name, .. }
                if matches!(
                    pragma_name.to_ascii_lowercase().as_str(),
                    "table_info" | "index_list"
                ) =>
            {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },
    ))
    .map_err(error)?;
    Ok(conn)
}

fn sqlite_rows(
    conn: &Connection,
    sql: &str,
    limit: usize,
    scope: &ReadScope,
) -> Result<(Vec<Value>, Rows)> {
    let error = |error| parse_error("SQLite SQL", error);
    let mut statement = conn.prepare(sql).map_err(error)?;
    let names = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let columns = names
        .iter()
        .map(|name| json!({ "name": name, "type": "SQLite" }))
        .collect::<Vec<_>>();
    let mut output = Rows::new(limit, &columns)?;
    let mut rows = statement.query([]).map_err(error)?;
    while let Some(row) = rows.next().map_err(error)? {
        scope.check()?;
        let mut value = Map::new();
        for (index, name) in names.iter().enumerate() {
            let cell = match row.get_ref(index).map_err(error)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(number) => integer(number),
                ValueRef::Real(number) => json!(number),
                ValueRef::Text(bytes) => json!(String::from_utf8_lossy(bytes)),
                ValueRef::Blob(bytes) => json!(format!("<blob {} bytes>", bytes.len())),
            };
            value.insert(name.clone(), cell);
        }
        if !output.push(Value::Object(value))? {
            break;
        }
    }
    Ok((columns, output))
}

fn sqlite_preview(conn: &Connection, file_size: u64, scope: &ReadScope) -> Result<Value> {
    let error = |error| parse_error("SQLite preview", error);
    let filter = "FROM sqlite_schema WHERE type='table'";
    let table_count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) {filter}"), [], |row| row.get(0))
        .map_err(error)?;
    let mut statement = conn
        .prepare(&format!("SELECT name {filter} ORDER BY name LIMIT 10"))
        .map_err(error)?;
    let mut names = statement.query([]).map_err(error)?;
    let mut tables = Vec::new();
    let mut bytes = 0;
    while let Some(name) = names.next().map_err(error)? {
        scope.check()?;
        let name: String = name.get(0).map_err(error)?;
        let literal = name.replace('\'', "''");
        let identifier = name.replace('"', "\"\"");
        let (_, columns) = sqlite_rows(
            conn,
            &format!("PRAGMA table_info('{literal}')"),
            MAX_COLUMNS,
            scope,
        )?;
        let columns = columns.rows.into_iter().map(|row| json!({
            "name": row["name"], "type": row["type"],
            "notnull": row["notnull"].as_i64() == Some(1), "pk": row["pk"].as_i64().is_some_and(|n| n > 0),
        })).collect::<Vec<_>>();
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{identifier}\""),
                [],
                |row| row.get(0),
            )
            .map_err(error)?;
        let (_, sample) = sqlite_rows(
            conn,
            &format!("SELECT * FROM \"{identifier}\" LIMIT 50"),
            50,
            scope,
        )?;
        let table = json!({ "name": name, "columns": columns, "row_count": count, "preview_rows": sample.rows });
        bytes += serde_json::to_vec(&table)?.len();
        if bytes > MAX_RESULT_BYTES {
            return Err(AgentError::Other(
                "SQLite preview exceeds its 2 MiB result budget".into(),
            ));
        }
        tables.push(table);
    }
    Ok(
        json!({ "kind": "sqlite", "file_size": file_size, "table_count": table_count, "tables": tables }),
    )
}
