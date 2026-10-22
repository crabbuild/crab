use cellar::sdk::{
    Application, Cell, Context, HandlerFuture, Invocation, Output, Registry, Storage,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use super::{Address, Error, Result, directory, repository};

pub(crate) use cellar::sdk::storage::SqlValue;

const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 128 * 1024;
const MAX_RETRY_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
// Script and repository class names participate in persisted Cell identities.
// Keep them stable when adding authorities so existing Cells remain addressable.
pub(crate) const SCRIPT: &str = "crab-repository";
pub(crate) const VERSION: &str = "v1";
pub(crate) const INITIALIZE: u32 = 0;

pub(crate) struct SqlStatement {
    pub sql: String,
    pub parameters: Vec<SqlValue>,
}

pub(crate) struct SqlBatch {
    pub statements: Vec<SqlStatement>,
}

pub(crate) struct SqlResultSet {
    pub rows: Vec<Vec<SqlValue>>,
    pub rows_affected: u64,
}

pub(crate) struct Transaction<'a> {
    storage: &'a Storage,
    now_ms: i64,
}

impl Transaction<'_> {
    pub(crate) fn now_ms(&self) -> i64 {
        self.now_ms
    }

    pub(crate) fn document<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.storage
            .get(key)
            .map_err(Error::sdk)?
            .map(decode_document)
            .transpose()
    }

    pub(crate) fn put_document<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let value = serde_json::to_value(value)?;
        if serde_json::to_vec(&value)?.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Invalid("Cell document exceeds its byte limit"));
        }
        // Native KV and application SQL share this SDK transaction. Metadata,
        // indexes, and the command receipt must commit or roll back together.
        self.storage.put_json(key, &value).map_err(Error::sdk)
    }

    pub(crate) fn delete_document(&self, key: &str) -> Result<bool> {
        self.storage.delete(key).map_err(Error::sdk)
    }

    pub(crate) fn documents<T: DeserializeOwned>(
        &self,
        prefix: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, T)>> {
        if !(1..=51).contains(&limit) {
            return Err(Error::Invalid("Cell document page size is invalid"));
        }
        // Cellar's start_after remains a lower bound even in reverse order;
        // descending pagination needs the exclusive upper bound instead.
        let options = cellar::sdk::storage::ListOptions {
            prefix: Some(prefix.to_owned()),
            end: before.map(str::to_owned),
            limit: Some(limit),
            reverse: true,
            ..Default::default()
        };
        let mut bytes = 0usize;
        self.storage
            .list(&options)
            .map_err(Error::sdk)?
            .into_iter()
            .map(|(key, value)| {
                if let cellar::storage::StoredValue::Json(json) = &value {
                    bytes = bytes.saturating_add(json.len());
                }
                if bytes > MAX_RESULT_BYTES {
                    return Err(Error::Invalid("Cell document page exceeds its byte limit"));
                }
                decode_document(value).map(|value| (key, value))
            })
            .collect()
    }

    pub(crate) fn install_schema(&self, schema: &str) -> Result<()> {
        let (remaining, _, _) = self.storage.sql_ingest(schema).map_err(Error::sdk)?;
        if !remaining.trim().is_empty() {
            return Err(Error::Invalid("Cell schema contains incomplete SQL"));
        }
        Ok(())
    }

    pub(crate) fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        batch
            .statements
            .iter()
            .map(|statement| self.query(&statement.sql, &statement.parameters))
            .collect()
    }

    pub(crate) fn query(&self, sql: &str, parameters: &[SqlValue]) -> Result<SqlResultSet> {
        let mut cursor = self
            .storage
            .sql_cursor(sql, parameters)
            .map_err(Error::sdk)?;
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = cursor.next_raw().map_err(Error::sdk)? {
            bytes += row
                .iter()
                .map(|value| match value {
                    SqlValue::Text(value) => value.len(),
                    SqlValue::Blob(value) => value.len(),
                    _ => 8,
                })
                .sum::<usize>();
            if bytes > MAX_RESULT_BYTES {
                return Err(Error::Invalid("Cell SQL result exceeds its byte limit"));
            }
            rows.push(row);
        }
        let rows_affected = if cursor.rows_written() == 0 {
            0
        } else {
            drop(cursor);
            // Cellar reports total writes, including triggers. CAS callers
            // need SQLite's direct statement count on this same turn.
            let mut changes = self
                .storage
                .sql_cursor("SELECT changes()", &[])
                .map_err(Error::sdk)?;
            match changes.next_raw().map_err(Error::sdk)?.as_deref() {
                Some([SqlValue::Integer(count)]) => u64::try_from(*count)
                    .map_err(|_| Error::Invalid("SQL affected-row count is negative"))?,
                _ => return Err(Error::Invalid("SQL affected-row count is invalid")),
            }
        };
        Ok(SqlResultSet {
            rows,
            rows_affected,
        })
    }
}

fn decode_document<T: DeserializeOwned>(value: cellar::storage::StoredValue) -> Result<T> {
    let cellar::storage::StoredValue::Json(json) = value else {
        return Err(Error::Invalid("Cell document is not JSON"));
    };
    if json.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Invalid("Cell document exceeds its byte limit"));
    }
    serde_json::from_str(&json).map_err(Error::sdk)
}

pub(crate) trait Mutation {
    const CLASS: &'static str;
    const ID: u32;
    type Input: DeserializeOwned + Serialize;
    type Output: DeserializeOwned + Serialize;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>>;
}

pub(crate) trait Read {
    const CLASS: &'static str;
    const ID: u32;
    type Input: DeserializeOwned + Serialize;
    type Output: DeserializeOwned + Serialize;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output>;
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum MutationResult<T> {
    Success(T),
    Rejected(T),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationIdentity {
    pub request_id: Uuid,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Address,
    pub operation: u32,
    pub mutation: Option<MutationIdentity>,
    pub input: Value,
    pub minimum: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub revision: u64,
    pub rejected: bool,
    pub output: Value,
}

async fn request_bytes(
    context: &Context,
    body: cellar::engine_api::RequestBody,
) -> Result<Vec<u8>> {
    let mut stream = context.take_body(body).map_err(Error::sdk)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(std::io::Error::other).map_err(Error::sdk)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_REQUEST_BYTES {
            return Err(Error::Invalid("Cell request exceeds its byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct ApplicationCell {
    class: &'static str,
}

impl Cell for ApplicationCell {
    fn invoke(self: Arc<Self>, context: Context, event: Invocation) -> HandlerFuture<Output> {
        Box::pin(async move {
            let Invocation::Fetch(request) = event else {
                return Ok(Output::Done);
            };
            let body = request_bytes(&context, request.body).await?;
            let request: Request = serde_json::from_slice(&body)?;
            let storage = context.storage()?;
            storage.sql_cursor("PRAGMA foreign_keys = ON", &[])?;
            let now_ms = super::unix_now_ms()?;
            let reply = storage.transaction_sync(|storage| {
                if request.target.class() != self.class
                    || context
                        .identity()
                        .and_then(|identity| identity.name.as_deref())
                        != Some(request.target.name().as_str())
                {
                    return Err(Error::Invalid("Cell route and request authority disagree").into());
                }
                let mut transaction = Transaction { storage, now_ms };
                let create = request.operation == INITIALIZE && request.mutation.is_none();
                match request.target {
                    Address::Repository(id) => repository::initialize(
                        &transaction,
                        id,
                        create,
                        request.operation,
                        request.mutation.is_some(),
                    )?,
                    Address::Directory => directory::initialize(
                        &transaction,
                        create,
                        request.operation,
                        request.mutation.is_some(),
                    )?,
                }
                Ok(execute(&mut transaction, &request)?)
            });
            let response = match reply {
                Ok(reply) => {
                    let bytes = serde_json::to_vec(&reply)?;
                    if bytes.len() > MAX_RESULT_BYTES {
                        return Err(Error::Invalid("Cell result exceeds its byte limit").into());
                    }
                    cellar::sdk::http::Response::new(
                        200,
                        bytes,
                        vec![("content-type".into(), "application/json".into())],
                    )
                }
                Err(error) => {
                    let status = match error.downcast_ref::<Error>() {
                        Some(Error::Invalid(_) | Error::Json(_)) => 400,
                        Some(Error::IndexUnavailable) => 424,
                        Some(Error::SchemaUnavailable) => 423,
                        Some(Error::NotInitialized) => 428,
                        Some(Error::StaleSearch) => 416,
                        Some(Error::RequestConflict) => 409,
                        Some(Error::Expired) => 410,
                        Some(Error::StaleRead) => 412,
                        _ => 503,
                    };
                    tracing::warn!(error = %error, status, "Cell invocation failed");
                    cellar::sdk::http::Response::new(
                        status,
                        "{\"error\":\"repository Cell request failed\"}",
                        vec![("content-type".into(), "application/json".into())],
                    )
                }
            };
            // SDK response construction retains Cellar's ownership and durability gate.
            Ok(Output::Http(response.into_http(&context)?))
        })
    }
}

fn execute(context: &mut Transaction<'_>, request: &Request) -> Result<Reply> {
    let Some(identity) = request.mutation else {
        if request.operation == INITIALIZE {
            return Ok(Reply {
                revision: revision(context, request.target)?,
                rejected: false,
                output: Value::Null,
            });
        }
        let current = revision(context, request.target)?;
        if request.minimum.is_some_and(|minimum| current < minimum) {
            return Err(Error::StaleRead);
        }
        let output = match request.target {
            Address::Repository(_) => {
                repository::dispatch_read(context, request.operation, request.input.clone())?
            }
            Address::Directory => {
                directory::dispatch_read(context, request.operation, request.input.clone())?
            }
        };
        return Ok(Reply {
            revision: revision(context, request.target)?,
            rejected: false,
            output,
        });
    };
    if identity.expires_at_ms <= identity.issued_at_ms
        || identity.expires_at_ms.saturating_sub(identity.issued_at_ms) > MAX_RETRY_WINDOW_MS
        || identity.issued_at_ms > context.now_ms.saturating_add(30_000)
    {
        return Err(Error::Invalid("Cell command lifetime is invalid"));
    }
    let receipts = match request.target {
        Address::Repository(_) => "repository_command_receipts",
        Address::Directory => "directory_command_receipts",
    };
    // Prune a bounded batch after the retry retention window so
    // receipts cannot grow forever or turn one command into an unbounded GC.
    context.storage.sql_cursor(
        &format!("DELETE FROM {receipts} WHERE request_id IN (SELECT request_id FROM {receipts} WHERE expires_at_ms <= ? ORDER BY expires_at_ms LIMIT 128)"),
        &[SqlValue::Integer(context.now_ms.saturating_sub(MAX_RETRY_WINDOW_MS))],
    ).map_err(Error::sdk)?;
    let payload = serde_json::to_vec(&(
        request.operation,
        identity.issued_at_ms,
        identity.expires_at_ms,
        &request.input,
    ))?;
    let digest = blake3::hash(&payload);
    let mut existing = context
        .storage
        .sql_cursor(
            &format!("SELECT digest, reply FROM {receipts} WHERE request_id = ?"),
            &[SqlValue::Blob(identity.request_id.as_bytes().to_vec())],
        )
        .map_err(Error::sdk)?;
    if let Some(row) = existing.next_raw().map_err(Error::sdk)? {
        let [SqlValue::Blob(stored_digest), SqlValue::Blob(reply)] = row.as_slice() else {
            return Err(Error::Invalid("Cell command receipt is invalid"));
        };
        if stored_digest.as_slice() != digest.as_bytes() {
            return Err(Error::RequestConflict);
        }
        return Ok(serde_json::from_slice(reply)?);
    }
    drop(existing);
    // Replay precedes expiry and archive checks; fresh expired IDs never execute.
    if identity.expires_at_ms <= context.now_ms {
        return Err(Error::Expired);
    }
    // Rejection rolls application writes back while its receipt commits.
    // Use the SDK's nested transaction API; raw SQL savepoints are forbidden.
    let result = context
        .storage
        .transaction_sync(|storage| {
            let mut application = Transaction {
                storage,
                now_ms: context.now_ms,
            };
            let result = match request.target {
                Address::Repository(_) => repository::dispatch_mutation(
                    &mut application,
                    request.operation,
                    request.input.clone(),
                )?,
                Address::Directory => directory::dispatch_mutation(
                    &mut application,
                    request.operation,
                    request.input.clone(),
                )?,
            };
            if matches!(result, MutationResult::Rejected(_)) {
                storage.rollback()?;
            }
            Ok(result)
        })
        .map_err(|error| error.downcast::<Error>().unwrap_or_else(Error::sdk))?;
    let (rejected, output) = match result {
        MutationResult::Success(value) => (false, value),
        MutationResult::Rejected(value) => (true, value),
    };
    let reply = Reply {
        revision: revision(context, request.target)?,
        rejected,
        output,
    };
    let bytes = serde_json::to_vec(&reply)?;
    if bytes.len() > MAX_RESULT_BYTES {
        return Err(Error::Invalid("Cell command output exceeds its byte limit"));
    }
    context.storage.sql_cursor(
        &format!("INSERT INTO {receipts}(request_id, digest, expires_at_ms, reply) VALUES (?, ?, ?, ?)"),
        &[SqlValue::Blob(identity.request_id.as_bytes().to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), SqlValue::Integer(identity.expires_at_ms), SqlValue::Blob(bytes)],
    ).map_err(Error::sdk)?;
    Ok(reply)
}

fn revision(context: &Transaction<'_>, target: Address) -> Result<u64> {
    let sql = match target {
        Address::Repository(_) => {
            "SELECT app_revision FROM repository_identity WHERE singleton = 1"
        }
        Address::Directory => "SELECT app_revision FROM directory_identity WHERE singleton = 1",
    };
    let result = context.sql(&SqlBatch {
        statements: vec![SqlStatement {
            sql: sql.into(),
            parameters: vec![],
        }],
    })?;
    match result
        .first()
        .and_then(|set| set.rows.first())
        .and_then(|row| row.first())
    {
        Some(SqlValue::Integer(value)) => {
            u64::try_from(*value).map_err(|_| Error::Invalid("Cell revision is negative"))
        }
        _ => Err(Error::Invalid("Cell revision is missing")),
    }
}

pub(crate) fn compiled() -> Result<Arc<dyn Application>> {
    let registry = Registry::new(|context, event| {
        Box::pin(async move {
            let Invocation::Fetch(mut request) = event else {
                return Ok(Output::Done);
            };
            let url = url::Url::parse(&request.url)?;
            // Cellar cannot safely redispatch streamed bodies. This bounded
            // command envelope must remain replayable when an owner route expires.
            let body = request_bytes(&context, request.body).await?;
            request.body = cellar::engine_api::RequestBody::Bytes(body.into());
            let target = Address::parse(url.path())?;
            let response = context
                .namespace(target.namespace())?
                .get_by_name(&target.name())
                .fetch(&context, request)
                .await?;
            Ok(Output::Http(response))
        })
    })
    .cell(repository::CLASS, |_, _| {
        Ok(Arc::new(ApplicationCell {
            class: repository::CLASS,
        }))
    })
    .map_err(Error::sdk)?
    .cell(directory::CLASS, |_, _| {
        Ok(Arc::new(ApplicationCell {
            class: directory::CLASS,
        }))
    })
    .map_err(Error::sdk)?;
    Ok(Arc::new(registry))
}

pub(in crate::cells) fn invoke_mutation<T: Mutation>(
    context: &mut Transaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<MutationResult<serde_json::Value>> {
    let input = serde_json::from_value(input)?;
    Ok(match T::execute(context, input)? {
        MutationResult::Success(value) => MutationResult::Success(serde_json::to_value(value)?),
        MutationResult::Rejected(value) => MutationResult::Rejected(serde_json::to_value(value)?),
    })
}

pub(in crate::cells) fn invoke_read<T: Read>(
    context: &mut Transaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<serde_json::Value> {
    Ok(serde_json::to_value(T::execute(
        context,
        serde_json::from_value(input)?,
    )?)?)
}
