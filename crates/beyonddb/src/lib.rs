//! Account-scoped DynamoDB state hosted by a Crab Cell.

mod authorization;
mod backend;
mod catalog;
mod credentials;
mod expression_wire;
mod items;
mod partition;
mod provision;
mod routing;
mod server;
mod split;
mod table;
mod tags;
mod transaction_token;
mod ttl;

pub use expression_wire::WireCondition;
pub use items::*;
pub use partition::*;
pub use provision::*;
pub use routing::*;
pub use server::{
    BeyonddbPeerScope, NodeLeasePublisher, PublishedNodeLease, build_http_state, build_peer_client,
    peer_router,
};
pub use split::*;
pub use table::*;
pub use ttl::*;

pub use authorization::CellAuthorizationStore;
pub use backend::{CellStorage, InitialPartitionProvisioner};
pub use catalog::CellCatalogStore;
pub use credentials::{CellCredentialStore, credential_target, initialize_credentials};

use std::{collections::HashSet, sync::OnceLock};

use crab_cell_app::{ApplicationBuilder, CellApplication, CellType};
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};
use crab_cell_runtime::identity::{ApplicationId, CellTarget, Digest, NamespaceId, TenantId};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlResultSet, SqlStatement, SqlValue};
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, OperationDescriptor, Query, QueryContext, RegistryBuilder,
};
use crab_cell_runtime::{Error, Result, partition_for_shard};
use extenddb_core::limits::LimitsConfig;
use extenddb_core::types::{
    AttributeDefinition, BillingMode, CreateTableInput, Item, KeySchemaElement,
    ProvisionedThroughput, extract_key,
};
use extenddb_core::validation;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const MODULE: &str = "beyonddb-account";
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x42; 16]);
const DATA_MODULE: &str = "beyonddb-data";
const DATA_NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x43; 16]);
const APPLICATION: ApplicationId = ApplicationId::from_bytes([0x42; 16]);
/// Stable Cell application identity for BeyondDB storage layouts.
pub const APPLICATION_ID: ApplicationId = APPLICATION;
const SCHEMA: &str = include_str!("schema.sql");
const OPERATION_BYTES: u32 = 4 * 1024 * 1024 + 64 * 1024;

static NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: NAMESPACE,
    name: MODULE,
    role: CatalogRole::Sql,
    shards: 1,
    effect_targets: &[],
    dead_letter: None,
}];

const fn operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: OPERATION_BYTES,
        output_limit: OPERATION_BYTES,
    }
}

static COMMANDS: [OperationDescriptor; 15] = [
    operation(1),
    operation(2),
    operation(3),
    operation(5),
    operation(7),
    operation(8),
    operation(9),
    operation(10),
    operation(11),
    operation(12),
    operation(13),
    operation(14),
    operation(15),
    operation(16),
    operation(17),
];
static QUERIES: [OperationDescriptor; 17] = [
    operation(4),
    operation(6),
    operation(7),
    operation(8),
    operation(9),
    operation(10),
    operation(11),
    operation(12),
    operation(13),
    operation(14),
    operation(15),
    operation(16),
    operation(17),
    operation(18),
    operation(19),
    operation(20),
    operation(21),
];

/// Statically linked account application.
pub struct Beyonddb;

impl CellApplication for Beyonddb {
    const NAME: &'static str = "beyonddb";

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        builder.register(AccountModule)?;
        builder.register(partition::DataModule)?;
        builder.register(credentials::CredentialModule)?;
        builder.cell_type(
            CellType::new(MODULE, MODULE, NAMESPACE, CatalogRole::Sql, 1)?
                .with_limits(512 * 1024 * 1024, 64 * 1024 * 1024)?,
        )?;
        builder.cell_type(data_cell_type()?)?;
        builder.cell_type(credentials::cell_type()?)
    }
}

fn data_cell_type() -> Result<CellType> {
    CellType::new(
        DATA_MODULE,
        DATA_MODULE,
        DATA_NAMESPACE,
        CatalogRole::Sql,
        1,
    )?
    .with_entity_partitions()?
    .with_limits(512 * 1024 * 1024, 64 * 1024 * 1024)
}

fn data_partition_bytes(table_id: &str, partition_id: &[u8; 16]) -> Result<[u8; 33]> {
    let parsed = blake3::Hash::from_hex(table_id)
        .map_err(|_| Error::Identity("invalid BeyondDB table ID"))?;
    if parsed.to_hex().to_string() != table_id {
        return Err(Error::Identity("noncanonical BeyondDB table ID"));
    }
    let mut scope = Vec::with_capacity(80);
    scope.extend_from_slice(table_id.as_bytes());
    scope.extend_from_slice(partition_id);
    data_cell_type()?.entity_partition(&scope)
}

/// Resolves an account table range to its independently owned data Cell.
pub fn data_target(
    account_id: &str,
    table_id: &str,
    partition_id: &[u8; 16],
) -> Result<CellTarget> {
    let account = account_target(account_id)?;
    let parsed = blake3::Hash::from_hex(table_id)
        .map_err(|_| Error::Identity("invalid BeyondDB table ID"))?;
    if &parsed.as_bytes()[..16] != account.tenant().as_bytes() {
        return Err(Error::Identity("table does not belong to account"));
    }
    CellTarget::new(
        account.tenant(),
        APPLICATION,
        DATA_NAMESPACE,
        &data_partition_bytes(table_id, partition_id)?,
    )
}

/// Resolves an ExtendDB account to its sole account Cell.
pub fn account_target(account_id: &str) -> Result<CellTarget> {
    if account_id.is_empty() || account_id.len() > 128 {
        return Err(Error::Identity("invalid ExtendDB account ID"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"beyonddb.account.v1\0");
    hasher.update(account_id.as_bytes());
    let hash = hasher.finalize();
    let mut tenant = [0_u8; 16];
    tenant.copy_from_slice(&hash.as_bytes()[..16]);
    CellTarget::new(
        TenantId::from_bytes(tenant),
        APPLICATION,
        NAMESPACE,
        &partition_for_shard(0),
    )
}

/// Installs the initial account schema during Cell bootstrap.
pub fn initialize_account(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA)?;
    Ok(())
}

/// Installs a data partition's SQL schema during Cell bootstrap.
pub fn initialize_partition(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(include_str!("partition_schema.sql"))?;
    Ok(())
}

struct AccountModule;

impl crab_cell_runtime::registry::CellModule for AccountModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: {
                let mut source = blake3::Hasher::new();
                source.update(include_bytes!("lib.rs"));
                source.update(include_bytes!("table.rs"));
                source.update(include_bytes!("items.rs"));
                source.update(include_bytes!("items/scan.rs"));
                source.update(include_bytes!("expression_wire.rs"));
                source.update(include_bytes!("routing.rs"));
                source.update(include_bytes!("routing/split_state.rs"));
                source.update(include_bytes!("authorization.rs"));
                source.update(include_bytes!("tags.rs"));
                source.update(include_bytes!("ttl.rs"));
                source.update(include_bytes!("transaction_token.rs"));
                Digest::from_bytes(*source.finalize().as_bytes())
            },
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: SCHEMA,
                digest: Digest::from_bytes(*blake3::hash(SCHEMA.as_bytes()).as_bytes()),
            }])),
            commands: &COMMANDS,
            queries: &QUERIES,
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        registry.bind_command::<CreateTable>()?;
        registry.bind_command::<PutItem>()?;
        registry.bind_command::<DeleteItem>()?;
        registry.bind_command::<TransactWrite>()?;
        registry.bind_command::<DeleteTable>()?;
        registry.bind_command::<UpdateTable>()?;
        registry.bind_command::<UpdateItem>()?;
        registry.bind_command::<ActivateTableRoute>()?;
        registry.bind_command::<BeginSplit>()?;
        registry.bind_command::<CommitSplit>()?;
        registry.bind_command::<authorization::PutUserPolicy>()?;
        registry.bind_command::<authorization::DeleteUserPolicy>()?;
        registry.bind_command::<tags::UpdateTags>()?;
        registry.bind_command::<ttl::UpdateTtl>()?;
        registry.bind_command::<transaction_token::ClaimTransactionToken>()?;
        registry.bind_query::<GetItem>()?;
        registry.bind_query::<TransactGet>()?;
        registry.bind_query::<DescribeTable>()?;
        registry.bind_query::<ListTables>()?;
        registry.bind_query::<DescribeTableById>()?;
        registry.bind_query::<ScanItems>()?;
        registry.bind_query::<ReadTableRoute>()?;
        registry.bind_query::<ReadSplitPlan>()?;
        registry.bind_query::<ReadPartitionRoute>()?;
        registry.bind_query::<ReadRoutePage>()?;
        registry.bind_query::<ReadPublishedPartition>()?;
        registry.bind_query::<ReadSplitRoute>()?;
        registry.bind_query::<authorization::ReadUserPolicies>()?;
        registry.bind_query::<tags::ReadTags>()?;
        registry.bind_query::<ttl::ReadTtl>()?;
        registry.bind_query::<ttl::ListTtlTables>()?;
        registry.bind_query::<transaction_token::ReadTransactionClaim>()
    }
}

/// JSON backed value with a canonical Cell wire encoding.
#[derive(Clone, Debug, PartialEq)]
pub struct Json<T>(pub T);

impl<T> WireValue for Json<T>
where
    T: Serialize + DeserializeOwned + Send + 'static,
{
    fn encode(&self, encoder: &mut BoundedEncoder) -> std::result::Result<(), CodecError> {
        let bytes = serde_json::to_vec(&self.0)
            .map_err(|_| CodecError::Invalid("DynamoDB value failed to encode"))?;
        encoder.write_bytes(&bytes)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> std::result::Result<Self, CodecError> {
        let bytes = decoder.read_bytes()?;
        let value: T = serde_json::from_slice(bytes)
            .map_err(|_| CodecError::Invalid("invalid DynamoDB JSON value"))?;
        let canonical = serde_json::to_vec(&value)
            .map_err(|_| CodecError::Invalid("DynamoDB value failed to encode"))?;
        if canonical != bytes {
            return Err(CodecError::Invalid("noncanonical DynamoDB JSON value"));
        }
        Ok(Self(value))
    }
}
