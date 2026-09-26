//! Bounded, expiring input transfers before atomic transaction phases.

use std::marker::PhantomData;

use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crab_cell_runtime::registry::{Command, CommandContext, CommandResult};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::table::statement;
use crate::{Error, Json, Result, SqlValue};

pub(crate) const CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_LIFETIME_MS: i64 = 60_000;

pub(crate) const fn upload_operation(id: u32) -> crab_cell_runtime::registry::OperationDescriptor {
    crab_cell_runtime::registry::OperationDescriptor {
        input_limit: CHUNK_BYTES as u32 + 4096,
        output_limit: 64,
        ..crate::operation(id)
    }
}

/// Immutable identity and deadline of one temporary transaction input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionPayloadRef {
    // Concurrent drivers consume independently even when bytes and deadlines match.
    pub upload_id: [u8; 16],
    pub digest: [u8; 32],
    pub bytes: u32,
    pub expires_at_ms: i64,
}

impl TransactionPayloadRef {
    /// Describe a serialized input; expiration is checked by the receiving Cell.
    pub fn new(bytes: &[u8], expires_at_ms: i64) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(Error::Command("transaction input exceeds transfer limit"));
        }
        Ok(Self {
            upload_id: *uuid::Uuid::now_v7().as_bytes(),
            digest: *blake3::hash(bytes).as_bytes(),
            bytes: bytes.len() as u32,
            expires_at_ms,
        })
    }

    /// Split this input into bounded command payloads.
    pub fn chunks<'a>(
        &'a self,
        bytes: &'a [u8],
    ) -> impl Iterator<Item = TransactionPayloadChunk> + 'a {
        bytes
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(chunk, bytes)| TransactionPayloadChunk {
                reference: self.clone(),
                chunk: chunk as u32,
                payload: bytes.to_vec(),
            })
    }

    fn validate(&self, now_ms: i64) -> Result<()> {
        if self.bytes == 0
            || self.bytes as usize > MAX_BYTES
            || self.expires_at_ms <= now_ms
            || self.expires_at_ms > now_ms.saturating_add(MAX_LIFETIME_MS)
        {
            return Err(Error::Command("invalid or expired transaction upload"));
        }
        Ok(())
    }
}

/// One immutable piece of a temporary transaction input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionPayloadChunk {
    pub reference: TransactionPayloadRef,
    pub chunk: u32,
    pub payload: Vec<u8>,
}

impl WireValue for TransactionPayloadChunk {
    fn encode(&self, encoder: &mut BoundedEncoder) -> std::result::Result<(), CodecError> {
        Json(self.reference.clone()).encode(encoder)?;
        encoder.write_u32(self.chunk)?;
        encoder.write_bytes(&self.payload)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> std::result::Result<Self, CodecError> {
        Ok(Self {
            reference: Json::decode(decoder)?.0,
            chunk: decoder.read_u32()?,
            payload: decoder.read_bytes()?.to_vec(),
        })
    }
}

/// Transaction phase whose input is assembled atomically from uploaded pieces.
pub trait MultipartTransactionCommand: Command<Input = Json<TransactionPayloadRef>> {
    type Payload: Serialize + DeserializeOwned;
    const UPLOAD_COMMAND_ID: u32;
}

/// Publish one input piece without locking keys or admitting a transaction.
pub struct UploadTransactionPayload<C>(PhantomData<fn() -> C>);

impl<C: MultipartTransactionCommand> Command for UploadTransactionPayload<C> {
    const MODULE: &'static str = C::MODULE;
    const ID: u32 = C::UPLOAD_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = TransactionPayloadChunk;
    type Output = Json<()>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let reference = input.reference;
        reference.validate(context.now_ms())?;
        let offset = (input.chunk as usize)
            .checked_mul(CHUNK_BYTES)
            .filter(|offset| *offset < reference.bytes as usize)
            .ok_or(Error::Command("invalid transaction upload position"))?;
        if input.payload.len() != CHUNK_BYTES.min(reference.bytes as usize - offset) {
            return Err(Error::Command("invalid transaction upload chunk length"));
        }
        // Reclamation is bounded per arrival. Deadlines remain part of the key,
        // so delayed messages cannot recreate a collected expired upload.
        context.sql(&statement(
            "DELETE FROM ddb_transaction_uploads WHERE (digest, expires_at_ms, upload_id, chunk) IN \
             (SELECT digest, expires_at_ms, upload_id, chunk FROM ddb_transaction_uploads \
             WHERE expires_at_ms <= ?1 ORDER BY expires_at_ms LIMIT 8)",
            vec![SqlValue::Integer(context.now_ms())],
        ))?;
        let key = vec![
            SqlValue::Blob(reference.digest.to_vec()),
            SqlValue::Integer(reference.expires_at_ms),
            SqlValue::Blob(reference.upload_id.to_vec()),
            SqlValue::Integer(i64::from(input.chunk)),
        ];
        let rows = context.sql(&statement(
            "SELECT bytes, payload FROM ddb_transaction_uploads WHERE digest = ?1 AND expires_at_ms = ?2 AND upload_id = ?3 AND chunk = ?4",
            key.clone(),
        ))?;
        if let Some(row) = rows[0].rows.first() {
            if row
                != &vec![
                    SqlValue::Integer(i64::from(reference.bytes)),
                    SqlValue::Blob(input.payload),
                ]
            {
                return Err(Error::Command("transaction upload chunk changed"));
            }
            return Ok(CommandResult::Success(Json(())));
        }
        let usage = context.sql(&statement(
            "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM ddb_transaction_uploads",
            vec![],
        ))?;
        let Some([SqlValue::Integer(bytes)]) = usage[0].rows.first().map(Vec::as_slice) else {
            return Err(Error::Command("invalid transaction upload usage"));
        };
        if *bytes < 0 || *bytes + input.payload.len() as i64 > MAX_BYTES as i64 {
            return Err(Error::Command("transaction upload capacity exhausted"));
        }
        let mut values = key;
        values.extend([
            SqlValue::Integer(i64::from(reference.bytes)),
            SqlValue::Blob(input.payload),
        ]);
        context.sql(&statement(
            "INSERT INTO ddb_transaction_uploads (digest, expires_at_ms, upload_id, chunk, bytes, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", values,
        ))?;
        Ok(CommandResult::Success(Json(())))
    }
}

pub(crate) fn consume<C: MultipartTransactionCommand>(
    context: &CommandContext<'_, '_>,
    reference: TransactionPayloadRef,
) -> Result<C::Payload> {
    reference.validate(context.now_ms())?;
    let mut bytes = Vec::with_capacity(reference.bytes as usize);
    for chunk in 0..(reference.bytes as usize).div_ceil(CHUNK_BYTES) {
        let rows = context.sql(&statement(
            "SELECT bytes, payload FROM ddb_transaction_uploads WHERE digest = ?1 AND expires_at_ms = ?2 AND upload_id = ?3 AND chunk = ?4",
            vec![SqlValue::Blob(reference.digest.to_vec()), SqlValue::Integer(reference.expires_at_ms), SqlValue::Blob(reference.upload_id.to_vec()), SqlValue::Integer(chunk as i64)],
        ))?;
        let Some([SqlValue::Integer(length), SqlValue::Blob(payload)]) =
            rows[0].rows.first().map(Vec::as_slice)
        else {
            return Err(Error::Command("transaction upload is incomplete"));
        };
        if *length != i64::from(reference.bytes)
            || bytes.len() + payload.len() > reference.bytes as usize
        {
            return Err(Error::Command("transaction upload length changed"));
        }
        bytes.extend_from_slice(payload);
    }
    if bytes.len() != reference.bytes as usize
        || blake3::hash(&bytes).as_bytes() != &reference.digest
    {
        return Err(Error::Command("transaction upload digest mismatch"));
    }
    let input = serde_json::from_slice(&bytes)?;
    // Consumption shares the phase savepoint. Failed/rejected phases retain
    // only expiring upload rows; successful phases own their durable recovery data.
    context.sql(&statement(
        "DELETE FROM ddb_transaction_uploads WHERE digest = ?1 AND expires_at_ms = ?2 AND upload_id = ?3",
        vec![
            SqlValue::Blob(reference.digest.to_vec()),
            SqlValue::Integer(reference.expires_at_ms),
            SqlValue::Blob(reference.upload_id.to_vec()),
        ],
    ))?;
    Ok(input)
}
