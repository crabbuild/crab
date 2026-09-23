use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crate::identity::{ApplicationId, CellTarget, NamespaceId, TenantId, shard_for_scope};
use crate::primitives::maintenance::{MaintenanceModule, register_maintenance};
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

use super::{
    BlobArtifactStore, BlobCondition, BlobMetadata, BlobMutation, BlobMutationOutcome, BlobPage,
    BlobPart, BlobQuery, BlobQueryResult, BlobRead, MAX_BLOB_PART_BYTES, MAX_BLOB_READ_BYTES,
    MAX_BLOB_READ_PARTS, blob_mutate, blob_query,
};

/// Compile-time namespace and operation identifiers for one Blob module.
pub trait BlobModule: MaintenanceModule {
    const NAMESPACE: NamespaceId;
    const MUTATE_COMMAND_ID: u32;
    const QUERY_ID: u32;
}

/// Registers typed Blob command and query bindings.
pub fn register_blob<M: BlobModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    registry.bind_blob_module(M::MODULE, M::NAMESPACE)?;
    registry.bind_command::<BlobCommand<M>>()?;
    registry.bind_query::<BlobQueryCommand<M>>()?;
    register_maintenance::<M>(registry)
}

/// Typed Blob mutation command.
pub struct BlobCommand<M>(PhantomData<fn() -> M>);

impl<M: BlobModule> Command for BlobCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::MUTATE_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = BlobMutation;
    type Output = BlobMutationOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = blob_mutate(
            context.primitive_transaction(),
            context.now_ms(),
            context.issued_at_ms(),
            &input,
        )?;
        Ok(match outcome {
            BlobMutationOutcome::NotFound | BlobMutationOutcome::Conflict => {
                CommandResult::Rejected(outcome)
            }
            _ => CommandResult::Success(outcome),
        })
    }
}

/// Typed Blob query.
pub struct BlobQueryCommand<M>(PhantomData<fn() -> M>);

impl<M: BlobModule> Query for BlobQueryCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = BlobQuery;
    type Output = BlobQueryResult;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        blob_query(context.primitive_connection(), &input)
    }
}

/// Authorized Blob capability with deterministic key sharding.
pub struct BlobNamespace<M> {
    client: CellClient,
    artifact_store: BlobArtifactStore,
    tenant: TenantId,
    application: ApplicationId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M> Clone for BlobNamespace<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            artifact_store: self.artifact_store.clone(),
            tenant: self.tenant,
            application: self.application,
            shards: self.shards,
            module: PhantomData,
        }
    }
}

impl<M: BlobModule> BlobNamespace<M> {
    /// Creates a Blob capability after validating its compiled namespace role.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
    ) -> crate::Result<Self> {
        let shards = client.require_namespace(M::NAMESPACE, M::MODULE, CatalogRole::Blob)?;
        let artifact_store = client.blob_artifact_store().ok_or(crate::Error::Control(
            "Blob artifact store is not configured",
        ))?;
        Ok(Self {
            client,
            artifact_store,
            tenant,
            application,
            shards,
            module: PhantomData,
        })
    }

    /// Applies one multipart or conditional mutation on the key's shard.
    pub async fn mutate(
        &self,
        identity: crate::cell::executor::MutationIdentity,
        mutation: BlobMutation,
    ) -> std::result::Result<Committed<BlobMutationOutcome>, InvocationError<BlobMutationOutcome>>
    {
        let target = self
            .target(mutation_key(&mutation))
            .map_err(InvocationError::NotStarted)?;
        let mutation = match mutation {
            BlobMutation::PutPart {
                key,
                upload_id,
                part_number,
                payload,
            } => {
                if payload.len() > MAX_BLOB_PART_BYTES {
                    return Err(InvocationError::NotStarted(crate::Error::Command(
                        "blob part exceeds 256 KiB",
                    )));
                }
                let digest = super::part_digest(&payload);
                self.artifact_store
                    .put_part(digest, &payload)
                    .await
                    .map_err(InvocationError::NotStarted)?;
                BlobMutation::PutPartRef {
                    key,
                    upload_id,
                    part_number,
                    digest,
                    size: payload.len() as u32,
                }
            }
            mutation => mutation,
        };
        self.client
            .command::<BlobCommand<M>>(&target, identity, mutation)
            .await
    }

    /// Reads metadata or a bounded range from one key shard.
    pub async fn query(
        &self,
        query: BlobQuery,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<BlobQueryResult>, InvocationError<BlobQueryResult>> {
        let key = query_key(&query).ok_or_else(|| {
            InvocationError::NotStarted(crate::Error::Identity(
                "blob list requires explicit shard query",
            ))
        })?;
        let target = self.target(key).map_err(InvocationError::NotStarted)?;
        let mut observed = self
            .client
            .query::<BlobQueryCommand<M>>(&target, minimum, query)
            .await?;
        if let BlobQueryResult::Read(Some(read)) = &mut observed.output {
            hydrate_read(&self.artifact_store, read)
                .await
                .map_err(InvocationError::NotStarted)?;
        }
        Ok(observed)
    }

    /// Lists one shard explicitly; global listing is a bounded fan-out concern.
    pub async fn list_shard(
        &self,
        shard: u32,
        prefix: Vec<u8>,
        after: Option<Vec<u8>>,
        limit: u32,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<BlobQueryResult>, InvocationError<BlobQueryResult>> {
        let target = self
            .shard_target(shard)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<BlobQueryCommand<M>>(
                &target,
                minimum,
                BlobQuery::List {
                    prefix,
                    after,
                    limit,
                },
            )
            .await
    }

    fn target(&self, key: &[u8]) -> crate::Result<CellTarget> {
        let shard = shard_for_scope(M::NAMESPACE, key, self.shards)?;
        self.shard_target(shard)
    }

    fn shard_target(&self, shard: u32) -> crate::Result<CellTarget> {
        if shard >= self.shards {
            return Err(crate::Error::Identity("blob shard outside namespace"));
        }
        CellTarget::new(
            self.tenant,
            self.application,
            M::NAMESPACE,
            &crate::partition_for_shard(shard),
        )
    }
}

fn mutation_key(mutation: &BlobMutation) -> &[u8] {
    match mutation {
        BlobMutation::Begin { key, .. }
        | BlobMutation::PutPart { key, .. }
        | BlobMutation::PutPartRef { key, .. }
        | BlobMutation::Complete { key, .. }
        | BlobMutation::Abort { key, .. }
        | BlobMutation::Delete { key, .. } => key,
    }
}

fn query_key(query: &BlobQuery) -> Option<&[u8]> {
    match query {
        BlobQuery::Head { key } | BlobQuery::Read { key, .. } => Some(key),
        BlobQuery::List { .. } => None,
    }
}

impl WireValue for BlobMutation {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Begin {
                key,
                upload_id,
                condition,
                content_type,
                metadata,
                expires_at_ms,
            } => {
                encoder.write_u8(0)?;
                encoder.write_bytes(key)?;
                encoder.write_bytes(upload_id)?;
                condition.encode(encoder)?;
                content_type.encode(encoder)?;
                encoder.write_bytes(metadata)?;
                encoder.write_i64(*expires_at_ms)
            }
            Self::PutPart { .. } => Err(CodecError::Invalid(
                "blob part payload must be uploaded to object store",
            )),
            Self::PutPartRef {
                key,
                upload_id,
                part_number,
                digest,
                size,
            } => {
                encoder.write_u8(1)?;
                encoder.write_bytes(key)?;
                encoder.write_bytes(upload_id)?;
                encoder.write_u32(*part_number)?;
                encoder.write_bytes(digest)?;
                encoder.write_u32(*size)
            }
            Self::Complete {
                key,
                upload_id,
                part_count,
            } => {
                encoder.write_u8(2)?;
                encoder.write_bytes(key)?;
                encoder.write_bytes(upload_id)?;
                encoder.write_u32(*part_count)
            }
            Self::Abort { key, upload_id } => {
                encoder.write_u8(3)?;
                encoder.write_bytes(key)?;
                encoder.write_bytes(upload_id)
            }
            Self::Delete { key, condition } => {
                encoder.write_u8(4)?;
                encoder.write_bytes(key)?;
                condition.encode(encoder)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Begin {
                key: decoder.read_bytes()?.to_vec(),
                upload_id: read_fixed(decoder, "blob upload ID length")?,
                condition: BlobCondition::decode(decoder)?,
                content_type: Option::<String>::decode(decoder)?,
                metadata: decoder.read_bytes()?.to_vec(),
                expires_at_ms: decoder.read_i64()?,
            }),
            1 => {
                let key = decoder.read_bytes()?.to_vec();
                let upload_id = read_fixed(decoder, "blob upload ID length")?;
                let part_number = decoder.read_u32()?;
                let digest = read_fixed(decoder, "blob part digest length")?;
                let size = decoder.read_u32()?;
                if size as usize > MAX_BLOB_PART_BYTES {
                    return Err(CodecError::Invalid("blob part exceeds 256 KiB"));
                }
                Ok(Self::PutPartRef {
                    key,
                    upload_id,
                    part_number,
                    digest,
                    size,
                })
            }
            2 => Ok(Self::Complete {
                key: decoder.read_bytes()?.to_vec(),
                upload_id: read_fixed(decoder, "blob upload ID length")?,
                part_count: decoder.read_u32()?,
            }),
            3 => Ok(Self::Abort {
                key: decoder.read_bytes()?.to_vec(),
                upload_id: read_fixed(decoder, "blob upload ID length")?,
            }),
            4 => Ok(Self::Delete {
                key: decoder.read_bytes()?.to_vec(),
                condition: BlobCondition::decode(decoder)?,
            }),
            _ => Err(CodecError::Invalid("invalid blob mutation tag")),
        }
    }
}

impl WireValue for BlobCondition {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Any => encoder.write_u8(0),
            Self::Missing => encoder.write_u8(1),
            Self::Etag(etag) => {
                encoder.write_u8(2)?;
                encoder.write_bytes(etag)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Any),
            1 => Ok(Self::Missing),
            2 => Ok(Self::Etag(read_fixed(decoder, "blob ETag length")?)),
            _ => Err(CodecError::Invalid("invalid blob condition tag")),
        }
    }
}

impl WireValue for BlobMutationOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Begun => encoder.write_u8(0),
            Self::PartStored { digest } => {
                encoder.write_u8(1)?;
                encoder.write_bytes(digest)
            }
            Self::Committed { etag, size } => {
                encoder.write_u8(2)?;
                encoder.write_bytes(etag)?;
                encoder.write_u64(*size)
            }
            Self::Aborted => encoder.write_u8(3),
            Self::Deleted => encoder.write_u8(4),
            Self::NotFound => encoder.write_u8(5),
            Self::Conflict => encoder.write_u8(6),
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Begun),
            1 => Ok(Self::PartStored {
                digest: read_fixed(decoder, "blob part digest length")?,
            }),
            2 => Ok(Self::Committed {
                etag: read_fixed(decoder, "blob ETag length")?,
                size: decoder.read_u64()?,
            }),
            3 => Ok(Self::Aborted),
            4 => Ok(Self::Deleted),
            5 => Ok(Self::NotFound),
            6 => Ok(Self::Conflict),
            _ => Err(CodecError::Invalid("invalid blob outcome tag")),
        }
    }
}

impl WireValue for BlobQuery {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Head { key } => {
                encoder.write_u8(0)?;
                encoder.write_bytes(key)
            }
            Self::Read { key, offset, limit } => {
                if *limit > MAX_BLOB_READ_BYTES {
                    return Err(CodecError::Invalid("blob read exceeds 512 KiB"));
                }
                encoder.write_u8(1)?;
                encoder.write_bytes(key)?;
                encoder.write_u64(*offset)?;
                encoder.write_u32(*limit)
            }
            Self::List {
                prefix,
                after,
                limit,
            } => {
                encoder.write_u8(2)?;
                encoder.write_bytes(prefix)?;
                after.encode(encoder)?;
                encoder.write_u32(*limit)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Head {
                key: decoder.read_bytes()?.to_vec(),
            }),
            1 => {
                let key = decoder.read_bytes()?.to_vec();
                let offset = decoder.read_u64()?;
                let limit = decoder.read_u32()?;
                if limit > MAX_BLOB_READ_BYTES {
                    return Err(CodecError::Invalid("blob read exceeds 512 KiB"));
                }
                Ok(Self::Read { key, offset, limit })
            }
            2 => Ok(Self::List {
                prefix: decoder.read_bytes()?.to_vec(),
                after: Option::<Vec<u8>>::decode(decoder)?,
                limit: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid blob query tag")),
        }
    }
}

impl WireValue for BlobQueryResult {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Head(value) => {
                encoder.write_u8(0)?;
                value.encode(encoder)
            }
            Self::Read(value) => {
                encoder.write_u8(1)?;
                value.encode(encoder)
            }
            Self::List(value) => {
                encoder.write_u8(2)?;
                value.encode(encoder)
            }
        }
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(Self::Head(Option::<BlobMetadata>::decode(decoder)?)),
            1 => Ok(Self::Read(Option::<BlobRead>::decode(decoder)?)),
            2 => Ok(Self::List(BlobPage::decode(decoder)?)),
            _ => Err(CodecError::Invalid("invalid blob query result tag")),
        }
    }
}

impl WireValue for BlobMetadata {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.key)?;
        encoder.write_bytes(&self.etag)?;
        encoder.write_u64(self.size)?;
        encoder.write_u32(self.part_count)?;
        self.content_type.encode(encoder)?;
        encoder.write_bytes(&self.metadata)?;
        encoder.write_i64(self.created_at_ms)?;
        encoder.write_i64(self.updated_at_ms)
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            key: decoder.read_bytes()?.to_vec(),
            etag: read_fixed(decoder, "blob ETag length")?,
            size: decoder.read_u64()?,
            part_count: decoder.read_u32()?,
            content_type: Option::<String>::decode(decoder)?,
            metadata: decoder.read_bytes()?.to_vec(),
            created_at_ms: decoder.read_i64()?,
            updated_at_ms: decoder.read_i64()?,
        })
    }
}

impl WireValue for BlobRead {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.bytes.len() > MAX_BLOB_READ_BYTES as usize {
            return Err(CodecError::Invalid("blob read exceeds 512 KiB"));
        }
        self.metadata.encode(encoder)?;
        encoder.write_u64(self.offset)?;
        encoder.write_u64(self.end)?;
        if self.parts.len() > MAX_BLOB_READ_PARTS {
            return Err(CodecError::Invalid("blob range references too many parts"));
        }
        encoder.write_count(self.parts.len())?;
        for part in &self.parts {
            encoder.write_bytes(&part.digest)?;
            encoder.write_u64(part.offset)?;
            encoder.write_u32(part.size)?;
        }
        encoder.write_bytes(&self.bytes)
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let metadata = BlobMetadata::decode(decoder)?;
        let offset = decoder.read_u64()?;
        let end = decoder.read_u64()?;
        if end < offset {
            return Err(CodecError::Invalid("blob range end precedes offset"));
        }
        let count = decoder.read_count()?;
        if count > MAX_BLOB_READ_PARTS {
            return Err(CodecError::Invalid("blob range references too many parts"));
        }
        let mut parts = Vec::with_capacity(count);
        for _ in 0..count {
            let digest = read_fixed(decoder, "blob part digest length")?;
            let part_offset = decoder.read_u64()?;
            let size = decoder.read_u32()?;
            if size as usize > MAX_BLOB_PART_BYTES {
                return Err(CodecError::Invalid("blob part exceeds 256 KiB"));
            }
            parts.push(BlobPart {
                digest,
                offset: part_offset,
                size,
            });
        }
        let bytes = decoder.read_bytes()?.to_vec();
        if bytes.len() > MAX_BLOB_READ_BYTES as usize {
            return Err(CodecError::Invalid("blob read exceeds 512 KiB"));
        }
        Ok(Self {
            metadata,
            offset,
            bytes,
            parts,
            end,
        })
    }
}

impl WireValue for BlobPage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.objects.len() > 128 {
            return Err(CodecError::Invalid("blob page exceeds 128 objects"));
        }
        encoder.write_count(self.objects.len())?;
        for object in &self.objects {
            object.encode(encoder)?;
        }
        self.next.encode(encoder)
    }
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = decoder.read_count()?;
        if count > 128 {
            return Err(CodecError::Invalid("blob page exceeds 128 objects"));
        }
        let mut objects = Vec::with_capacity(count);
        for _ in 0..count {
            objects.push(BlobMetadata::decode(decoder)?);
        }
        Ok(Self {
            objects,
            next: Option::<Vec<u8>>::decode(decoder)?,
        })
    }
}

fn read_fixed<const N: usize>(
    decoder: &mut BoundedDecoder<'_>,
    message: &'static str,
) -> Result<[u8; N], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid(message))
}

async fn hydrate_read(store: &BlobArtifactStore, read: &mut BlobRead) -> crate::Result<()> {
    if read.parts.is_empty() || read.bytes.len() > MAX_BLOB_READ_BYTES as usize {
        return Ok(());
    }
    let end = read.end.min(read.metadata.size);
    let mut bytes = Vec::with_capacity((end.saturating_sub(read.offset)) as usize);
    for part in &read.parts {
        let payload = store.read_part(part.digest, part.size).await?;
        let part_end = part.offset.saturating_add(u64::from(part.size));
        let start = read.offset.saturating_sub(part.offset) as usize;
        let take_end = end.saturating_sub(part.offset).min(u64::from(part.size)) as usize;
        if part_end <= read.offset || part.offset >= end || start > take_end {
            return Err(crate::Error::Command("blob range is incomplete"));
        }
        bytes.extend_from_slice(&payload[start..take_end]);
    }
    if bytes.len() != end.saturating_sub(read.offset) as usize {
        return Err(crate::Error::Command("blob range is incomplete"));
    }
    read.bytes = bytes;
    read.parts.clear();
    Ok(())
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
    fn blob_codecs_roundtrip_mutations_and_queries() {
        roundtrip(BlobMutation::Begin {
            key: b"logs/a".to_vec(),
            upload_id: [1; 16],
            condition: BlobCondition::Missing,
            content_type: Some("text/plain".into()),
            metadata: b"owner=a".to_vec(),
            expires_at_ms: 100_000,
        });
        roundtrip(BlobMutation::PutPartRef {
            key: b"logs/a".to_vec(),
            upload_id: [1; 16],
            part_number: 1,
            digest: [2; 32],
            size: 4,
        });
        roundtrip(BlobQuery::Read {
            key: b"logs/a".to_vec(),
            offset: 2,
            limit: MAX_BLOB_READ_BYTES,
        });
        roundtrip(BlobMutationOutcome::Committed {
            etag: [2; 32],
            size: 4,
        });
    }
}
