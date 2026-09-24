use std::marker::PhantomData;

use crate::cell::catalog::CatalogRole;
use crate::client::{CellClient, Committed, InvocationError, Observed, Receipt};
use crate::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue, read_fixed};
use crate::identity::{ApplicationId, CellTarget, NamespaceId, TenantId, shard_for_scope};
use crate::primitives::maintenance::{MaintenanceModule, register_maintenance};
use crate::registry::{Command, Query, RegistryBuilder};
use crate::registry::{CommandContext, CommandResult, QueryContext};

use super::{
    BlobArtifactStore, BlobCondition, BlobMetadata, BlobMutation, BlobMutationOutcome, BlobPage,
    BlobPart, BlobQuery, BlobQueryResult, BlobRead, MAX_BLOB_PART_BYTES, MAX_BLOB_READ_BYTES,
    MAX_BLOB_READ_PARTS, blob_mutate, blob_query,
};

mod codec;

/// Compile-time namespace and operation identifiers for one Blob module.
pub trait BlobModule: MaintenanceModule {
    /// Namespace that owns this Blob module.
    const NAMESPACE: NamespaceId;
    /// Command id that mutates blobs and uploads.
    const MUTATE_COMMAND_ID: u32;
    /// Query id that reads objects and uploads.
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
