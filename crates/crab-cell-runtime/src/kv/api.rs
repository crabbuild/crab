use std::marker::PhantomData;

use crate::{
    ApplicationId, BoundedDecoder, BoundedEncoder, CatalogRole, CellClient, CellTarget, CodecError,
    Command, CommandContext, CommandResult, Committed, InvocationError, NamespaceId, Observed,
    Query, QueryContext, Receipt, RegistryBuilder, TenantId, WireValue, partition_for_shard,
    shard_for_scope,
};

use super::{
    KvAtomicOutcome, KvAtomicRequest, KvCheck, KvCondition, KvEntry, KvMutation, KvMutationResult,
    KvPage, MAX_ATOMIC_ITEMS, MAX_LIST_ITEMS, VERSION_BYTES, kv_atomic, kv_get, kv_list,
};

const ABSENT_TAG: u8 = 0;
const VERSION_TAG: u8 = 1;
const PUT_TAG: u8 = 0;
const DELETE_TAG: u8 = 1;
const APPLIED_TAG: u8 = 0;
const PRECONDITION_FAILED_TAG: u8 = 1;

/// Compile-time operation identifiers for one native KV module.
pub trait KvModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const ATOMIC_COMMAND_ID: u32;
    const GET_QUERY_ID: u32;
    const LIST_QUERY_ID: u32;
}

/// Registers the three typed KV bindings contributed by one compiled module.
pub fn register_kv<M: KvModule>(registry: &mut RegistryBuilder) -> crate::Result<()> {
    registry.bind_command::<KvAtomicCommand<M>>()?;
    registry.bind_query::<KvGetQuery<M>>()?;
    registry.bind_query::<KvListQuery<M>>()
}

/// Typed KV atomic command bound to its module's immutable operation IDs.
pub struct KvAtomicCommand<M>(PhantomData<fn() -> M>);

impl<M: KvModule> Command for KvAtomicCommand<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::ATOMIC_COMMAND_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = KvAtomicRequest;
    type Output = KvAtomicOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crate::Result<CommandResult<Self::Output>> {
        let outcome = kv_atomic(context.primitive_transaction(), context.now_ms(), &input)?;
        Ok(match outcome {
            KvAtomicOutcome::Applied(_) => CommandResult::Success(outcome),
            KvAtomicOutcome::PreconditionFailed { .. } => CommandResult::Rejected(outcome),
        })
    }
}

/// One typed KV point-read input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvGetRequest {
    pub scope: Vec<u8>,
    pub key: Vec<u8>,
}

/// Typed KV point query bound to its module's immutable operation IDs.
pub struct KvGetQuery<M>(PhantomData<fn() -> M>);

impl<M: KvModule> Query for KvGetQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::GET_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = KvGetRequest;
    type Output = Option<KvEntry>;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        kv_get(
            context.primitive_connection(),
            &input.scope,
            &input.key,
            context.now_ms(),
        )
    }
}

/// One typed bounded KV list input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvListRequest {
    pub scope: Vec<u8>,
    pub prefix: Vec<u8>,
    pub after_key: Option<Vec<u8>>,
    pub limit: u32,
}

/// Typed KV list query bound to its module's immutable operation IDs.
pub struct KvListQuery<M>(PhantomData<fn() -> M>);

impl<M: KvModule> Query for KvListQuery<M> {
    const MODULE: &'static str = M::MODULE;
    const ID: u32 = M::LIST_QUERY_ID;
    const CODEC_VERSION: u32 = M::CODEC_VERSION;
    type Input = KvListRequest;
    type Output = KvPage;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> crate::Result<Self::Output> {
        kv_list(
            context.primitive_connection(),
            &input.scope,
            &input.prefix,
            input.after_key.as_deref(),
            usize::try_from(input.limit)
                .map_err(|_| crate::Error::Command("KV list limit overflow"))?,
            context.now_ms(),
        )
    }
}

/// Authorized native KV capability that derives stable shard Cells from scope.
pub struct KvNamespace<M> {
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    namespace: NamespaceId,
    shards: u32,
    module: PhantomData<fn() -> M>,
}

impl<M: KvModule> Clone for KvNamespace<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            tenant: self.tenant,
            application: self.application,
            namespace: self.namespace,
            shards: self.shards,
            module: PhantomData,
        }
    }
}

impl<M: KvModule> KvNamespace<M> {
    /// Creates a scoped KV capability after validating the compiled namespace role.
    pub fn new(
        client: CellClient,
        tenant: TenantId,
        application: ApplicationId,
        namespace: NamespaceId,
        shards: u32,
    ) -> crate::Result<Self> {
        client.require_namespace(namespace, M::MODULE, CatalogRole::Kv)?;
        shard_for_scope(namespace, &[], shards)?;
        Ok(Self {
            client,
            tenant,
            application,
            namespace,
            shards,
            module: PhantomData,
        })
    }

    /// Atomically checks and mutates one scope-derived shard.
    pub async fn atomic(
        &self,
        identity: crate::MutationIdentity,
        request: KvAtomicRequest,
    ) -> std::result::Result<Committed<KvAtomicOutcome>, InvocationError<KvAtomicOutcome>> {
        let target = self
            .target(&request.scope)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .command::<KvAtomicCommand<M>>(&target, identity, request)
            .await
    }

    /// Reads one live key from its scope-derived shard.
    pub async fn get(
        &self,
        scope: Vec<u8>,
        key: Vec<u8>,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<Option<KvEntry>>, InvocationError<Option<KvEntry>>> {
        let target = self.target(&scope).map_err(InvocationError::NotStarted)?;
        self.client
            .query::<KvGetQuery<M>>(&target, minimum, KvGetRequest { scope, key })
            .await
    }

    /// Lists one bounded current-read page from its scope-derived shard.
    pub async fn list(
        &self,
        request: KvListRequest,
        minimum: Option<Receipt>,
    ) -> std::result::Result<Observed<KvPage>, InvocationError<KvPage>> {
        let target = self
            .target(&request.scope)
            .map_err(InvocationError::NotStarted)?;
        self.client
            .query::<KvListQuery<M>>(&target, minimum, request)
            .await
    }

    fn target(&self, scope: &[u8]) -> crate::Result<CellTarget> {
        let shard = shard_for_scope(self.namespace, scope, self.shards)?;
        CellTarget::new(
            self.tenant,
            self.application,
            self.namespace,
            &partition_for_shard(shard),
        )
    }
}

impl WireValue for KvAtomicRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.scope)?;
        encoder.write_count(self.checks.len())?;
        for check in &self.checks {
            check.encode(encoder)?;
        }
        encoder.write_count(self.mutations.len())?;
        for mutation in &self.mutations {
            mutation.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let scope = decoder.read_bytes()?.to_vec();
        let checks = decode_bounded(decoder, MAX_ATOMIC_ITEMS, KvCheck::decode)?;
        let remaining = MAX_ATOMIC_ITEMS
            .checked_sub(checks.len())
            .ok_or(CodecError::Invalid("too many KV checks"))?;
        let mutations = decode_bounded(decoder, remaining, KvMutation::decode)?;
        Ok(Self {
            scope,
            checks,
            mutations,
        })
    }
}

impl WireValue for KvCheck {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.key)?;
        match self.condition {
            KvCondition::Absent => encoder.write_u8(ABSENT_TAG),
            KvCondition::Version(version) => {
                encoder.write_u8(VERSION_TAG)?;
                encoder.write_bytes(&version)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let key = decoder.read_bytes()?.to_vec();
        let condition = match decoder.read_u8()? {
            ABSENT_TAG => KvCondition::Absent,
            VERSION_TAG => KvCondition::Version(read_version(decoder)?),
            _ => return Err(CodecError::Invalid("invalid KV condition tag")),
        };
        Ok(Self { key, condition })
    }
}

impl WireValue for KvMutation {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Put {
                key,
                value,
                expires_at_ms,
            } => {
                encoder.write_u8(PUT_TAG)?;
                encoder.write_bytes(key)?;
                encoder.write_bytes(value)?;
                expires_at_ms.encode(encoder)
            }
            Self::Delete { key } => {
                encoder.write_u8(DELETE_TAG)?;
                encoder.write_bytes(key)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            PUT_TAG => Ok(Self::Put {
                key: decoder.read_bytes()?.to_vec(),
                value: decoder.read_bytes()?.to_vec(),
                expires_at_ms: Option::<i64>::decode(decoder)?,
            }),
            DELETE_TAG => Ok(Self::Delete {
                key: decoder.read_bytes()?.to_vec(),
            }),
            _ => Err(CodecError::Invalid("invalid KV mutation tag")),
        }
    }
}

impl WireValue for KvAtomicOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied(results) => {
                encoder.write_u8(APPLIED_TAG)?;
                encoder.write_count(results.len())?;
                for result in results {
                    result.encode(encoder)?;
                }
                Ok(())
            }
            Self::PreconditionFailed { key } => {
                encoder.write_u8(PRECONDITION_FAILED_TAG)?;
                encoder.write_bytes(key)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            APPLIED_TAG => Ok(Self::Applied(decode_bounded(
                decoder,
                MAX_ATOMIC_ITEMS,
                KvMutationResult::decode,
            )?)),
            PRECONDITION_FAILED_TAG => Ok(Self::PreconditionFailed {
                key: decoder.read_bytes()?.to_vec(),
            }),
            _ => Err(CodecError::Invalid("invalid KV atomic outcome tag")),
        }
    }
}

impl WireValue for KvMutationResult {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.key)?;
        match self.version {
            None => encoder.write_u8(0)?,
            Some(version) => {
                encoder.write_u8(1)?;
                encoder.write_bytes(&version)?;
            }
        }
        encoder.write_bool(self.deleted)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let key = decoder.read_bytes()?.to_vec();
        let version = match decoder.read_u8()? {
            0 => None,
            1 => Some(read_version(decoder)?),
            _ => return Err(CodecError::Invalid("invalid KV result version tag")),
        };
        let deleted = decoder.read_bool()?;
        if deleted == version.is_some() {
            return Err(CodecError::Invalid("inconsistent KV mutation result"));
        }
        Ok(Self {
            key,
            version,
            deleted,
        })
    }
}

impl WireValue for KvGetRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.scope)?;
        encoder.write_bytes(&self.key)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            scope: decoder.read_bytes()?.to_vec(),
            key: decoder.read_bytes()?.to_vec(),
        })
    }
}

impl WireValue for KvListRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.scope)?;
        encoder.write_bytes(&self.prefix)?;
        self.after_key.encode(encoder)?;
        encoder.write_u32(self.limit)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            scope: decoder.read_bytes()?.to_vec(),
            prefix: decoder.read_bytes()?.to_vec(),
            after_key: Option::<Vec<u8>>::decode(decoder)?,
            limit: decoder.read_u32()?,
        })
    }
}

impl WireValue for KvEntry {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.key)?;
        encoder.write_bytes(&self.value)?;
        encoder.write_bytes(&self.version)?;
        self.expires_at_ms.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            key: decoder.read_bytes()?.to_vec(),
            value: decoder.read_bytes()?.to_vec(),
            version: read_version(decoder)?,
            expires_at_ms: Option::<i64>::decode(decoder)?,
        })
    }
}

impl WireValue for KvPage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_count(self.entries.len())?;
        for entry in &self.entries {
            entry.encode(encoder)?;
        }
        self.next_after.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            entries: decode_bounded(decoder, MAX_LIST_ITEMS, KvEntry::decode)?,
            next_after: Option::<Vec<u8>>::decode(decoder)?,
        })
    }
}

fn decode_bounded<T>(
    decoder: &mut BoundedDecoder<'_>,
    maximum: usize,
    mut decode: impl FnMut(&mut BoundedDecoder<'_>) -> Result<T, CodecError>,
) -> Result<Vec<T>, CodecError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(CodecError::Invalid("KV collection count exceeds limit"));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decode(decoder)?);
    }
    Ok(values)
}

fn read_version(decoder: &mut BoundedDecoder<'_>) -> Result<[u8; VERSION_BYTES], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid("invalid KV version length"))
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
    fn kv_command_and_page_codecs_roundtrip_exactly() {
        roundtrip(KvAtomicRequest {
            scope: b"scope".to_vec(),
            checks: vec![KvCheck {
                key: b"old".to_vec(),
                condition: KvCondition::Version([7; VERSION_BYTES]),
            }],
            mutations: vec![
                KvMutation::Put {
                    key: b"new".to_vec(),
                    value: b"value".to_vec(),
                    expires_at_ms: Some(99),
                },
                KvMutation::Delete {
                    key: b"old".to_vec(),
                },
            ],
        });
        roundtrip(KvPage {
            entries: vec![KvEntry {
                key: b"key".to_vec(),
                value: b"value".to_vec(),
                version: [8; VERSION_BYTES],
                expires_at_ms: None,
            }],
            next_after: Some(b"key".to_vec()),
        });
    }

    #[test]
    fn kv_decoder_rejects_oversized_collections_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&129_u32.to_be_bytes());
        let mut decoder = BoundedDecoder::new(&bytes, bytes.len() as u32).unwrap();
        assert!(matches!(
            KvAtomicRequest::decode(&mut decoder),
            Err(CodecError::Invalid("KV collection count exceeds limit"))
        ));
    }
}
