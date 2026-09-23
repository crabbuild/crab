use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crate::identity::CellTarget;
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

use super::{
    MAX_PARAMETERS, MAX_ROWS, MAX_STATEMENTS, SqlBatch, SqlResultSet, SqlStatement, SqlValue,
};

const NULL_TAG: u8 = 0;
const INTEGER_TAG: u8 = 1;
const REAL_TAG: u8 = 2;
const TEXT_TAG: u8 = 3;
const BLOB_TAG: u8 = 4;
const MAX_COLUMNS: usize = 2_000;

/// Compile-time operation identifiers for one native SQL module.
pub trait SqlModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const BATCH_COMMAND_ID: u32;
    const BATCH_QUERY_ID: u32;
}

/// Registers the typed read-write and read-only SQL bindings for one module.
pub fn register_sql<M: SqlModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    registry.bind_command::<SqlBatchCommand<M>>()?;
    registry.bind_query::<SqlBatchQuery<M>>()
}

/// Typed read-write SQL batch bound to immutable module operation IDs.
pub struct SqlBatchCommand<M>(PhantomData<fn() -> M>);

impl<M: SqlModule> Command for SqlBatchCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::BATCH_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = SqlBatch;
    type Output = Vec<SqlResultSet>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(context.sql(&input)?))
    }
}

/// Typed read-only SQL batch bound to immutable module operation IDs.
pub struct SqlBatchQuery<M>(PhantomData<fn() -> M>);

impl<M: SqlModule> Query for SqlBatchQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::BATCH_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = SqlBatch;
    type Output = Vec<SqlResultSet>;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        context.sql(&input)
    }
}

/// Authorized native SQL capability for one explicit-key Cell.
#[derive(Clone)]
pub struct SqlCell<M> {
    client: CellClient,
    target: CellTarget,
    module: PhantomData<fn() -> M>,
}

impl<M: SqlModule> SqlCell<M> {
    /// Creates a SQL capability after validating its compiled namespace role.
    pub fn new(client: CellClient, target: CellTarget) -> crate::Result<Self> {
        let _ = client.require_namespace(target.namespace(), M::MODULE, CatalogRole::Sql)?;
        Ok(Self {
            client,
            target,
            module: PhantomData,
        })
    }

    /// Executes one bounded parameterized batch and publishes its receipt.
    pub async fn batch(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        batch: SqlBatch,
    ) -> std::result::Result<Committed<Vec<SqlResultSet>>, InvocationError<Vec<SqlResultSet>>> {
        self.client
            .command::<SqlBatchCommand<M>>(&self.target, identity, batch)
            .await
    }

    /// Executes one bounded read-only batch at an optional minimum receipt.
    pub async fn query(
        &self,
        minimum: Option<Receipt>,
        batch: SqlBatch,
    ) -> std::result::Result<Observed<Vec<SqlResultSet>>, InvocationError<Vec<SqlResultSet>>> {
        self.client
            .query::<SqlBatchQuery<M>>(&self.target, minimum, batch)
            .await
    }
}

impl WireValue for SqlValue {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Null => encoder.write_u8(NULL_TAG),
            Self::Integer(value) => {
                encoder.write_u8(INTEGER_TAG)?;
                encoder.write_i64(*value)
            }
            Self::Real(value) => {
                encoder.write_u8(REAL_TAG)?;
                encoder.write_f64(*value)
            }
            Self::Text(value) => {
                encoder.write_u8(TEXT_TAG)?;
                encoder.write_text(value)
            }
            Self::Blob(value) => {
                encoder.write_u8(BLOB_TAG)?;
                encoder.write_bytes(value)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            NULL_TAG => Ok(Self::Null),
            INTEGER_TAG => Ok(Self::Integer(decoder.read_i64()?)),
            REAL_TAG => Ok(Self::Real(decoder.read_f64()?)),
            TEXT_TAG => Ok(Self::Text(decoder.read_text()?.to_owned())),
            BLOB_TAG => Ok(Self::Blob(decoder.read_bytes()?.to_vec())),
            _ => Err(CodecError::Invalid("invalid SQL value tag")),
        }
    }
}

impl WireValue for SqlStatement {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.sql)?;
        encoder.write_count(self.parameters.len())?;
        for parameter in &self.parameters {
            parameter.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let sql = decoder.read_text()?.to_owned();
        let count = bounded_count(decoder, MAX_PARAMETERS, "too many SQL parameters")?;
        let mut parameters = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            parameters.push(SqlValue::decode(decoder)?);
        }
        Ok(Self { sql, parameters })
    }
}

impl WireValue for SqlBatch {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_count(self.statements.len())?;
        for statement in &self.statements {
            statement.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = bounded_count(decoder, MAX_STATEMENTS, "too many SQL statements")?;
        let mut statements = Vec::with_capacity(count);
        for _ in 0..count {
            statements.push(SqlStatement::decode(decoder)?);
        }
        Ok(Self { statements })
    }
}

impl WireValue for Vec<SqlResultSet> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_count(self.len())?;
        for result in self {
            encode_result(result, encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = bounded_count(decoder, MAX_STATEMENTS, "too many SQL result sets")?;
        let mut results = Vec::with_capacity(count);
        let mut remaining_rows = MAX_ROWS;
        for _ in 0..count {
            let result = decode_result(decoder, remaining_rows)?;
            remaining_rows = remaining_rows
                .checked_sub(result.rows.len())
                .ok_or(CodecError::Invalid("too many SQL result rows"))?;
            results.push(result);
        }
        Ok(results)
    }
}

fn encode_result(result: &SqlResultSet, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_count(result.columns.len())?;
    for column in &result.columns {
        encoder.write_text(column)?;
    }
    encoder.write_count(result.rows.len())?;
    for row in &result.rows {
        encoder.write_count(row.len())?;
        for value in row {
            value.encode(encoder)?;
        }
    }
    encoder.write_u64(result.rows_affected)
}

fn decode_result(
    decoder: &mut BoundedDecoder<'_>,
    remaining_rows: usize,
) -> Result<SqlResultSet, CodecError> {
    let column_count = bounded_count(decoder, MAX_COLUMNS, "too many SQL result columns")?;
    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        columns.push(decoder.read_text()?.to_owned());
    }
    let row_count = bounded_count(decoder, remaining_rows, "too many SQL result rows")?;
    let mut rows = Vec::with_capacity(row_count.min(64));
    for _ in 0..row_count {
        let value_count = bounded_count(decoder, MAX_COLUMNS, "too many SQL row values")?;
        if value_count != column_count {
            return Err(CodecError::Invalid("SQL row width does not match columns"));
        }
        let mut row = Vec::with_capacity(value_count);
        for _ in 0..value_count {
            row.push(SqlValue::decode(decoder)?);
        }
        rows.push(row);
    }
    let rows_affected = decoder.read_u64()?;
    if (columns.is_empty() && !rows.is_empty()) || (!columns.is_empty() && rows_affected != 0) {
        return Err(CodecError::Invalid("inconsistent SQL result shape"));
    }
    Ok(SqlResultSet {
        columns,
        rows,
        rows_affected,
    })
}

fn bounded_count(
    decoder: &mut BoundedDecoder<'_>,
    maximum: usize,
    message: &'static str,
) -> Result<usize, CodecError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(CodecError::Invalid(message));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: WireValue + PartialEq + std::fmt::Debug>(value: T) {
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        assert_eq!(T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    #[test]
    fn sql_batch_and_results_roundtrip_every_value_kind() {
        roundtrip(SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT ?1, ?2, ?3, ?4, ?5".into(),
                parameters: vec![
                    SqlValue::Null,
                    SqlValue::Integer(i64::MIN),
                    SqlValue::Real(1.5),
                    SqlValue::Text("crab".into()),
                    SqlValue::Blob(vec![0, 255]),
                ],
            }],
        });
        roundtrip(vec![SqlResultSet {
            columns: vec!["value".into()],
            rows: vec![vec![SqlValue::Integer(i64::MAX)]],
            rows_affected: 0,
        }]);
    }

    #[test]
    fn sql_decoder_rejects_counts_and_inconsistent_rows_before_allocation() {
        let mut too_many = BoundedEncoder::new(16).unwrap();
        too_many.write_count(MAX_STATEMENTS + 1).unwrap();
        let bytes = too_many.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 16).unwrap();
        assert!(matches!(
            SqlBatch::decode(&mut decoder),
            Err(CodecError::Invalid("too many SQL statements"))
        ));

        let mut too_many_parameters = BoundedEncoder::new(32).unwrap();
        too_many_parameters.write_text("SELECT 1").unwrap();
        too_many_parameters.write_count(MAX_PARAMETERS + 1).unwrap();
        let bytes = too_many_parameters.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 32).unwrap();
        assert!(matches!(
            SqlStatement::decode(&mut decoder),
            Err(CodecError::Invalid("too many SQL parameters"))
        ));

        let mut inconsistent = BoundedEncoder::new(64).unwrap();
        inconsistent.write_count(1).unwrap();
        inconsistent.write_count(1).unwrap();
        inconsistent.write_text("one").unwrap();
        inconsistent.write_count(1).unwrap();
        inconsistent.write_count(0).unwrap();
        inconsistent.write_u64(0).unwrap();
        let bytes = inconsistent.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 64).unwrap();
        assert!(matches!(
            Vec::<SqlResultSet>::decode(&mut decoder),
            Err(CodecError::Invalid("SQL row width does not match columns"))
        ));
    }
}
