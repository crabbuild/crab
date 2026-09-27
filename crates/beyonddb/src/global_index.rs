//! Independently owned, versioned global-index projections.

pub(crate) mod outbox;
pub use outbox::*;
mod read;
mod routing;
mod transfer;
pub use read::*;
pub use routing::*;
pub use transfer::*;

use std::sync::OnceLock;

use crab_cell_app::CellType;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::identity::{CellTarget, Digest, NamespaceId, TenantId};
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, Query, QueryContext, RegistryBuilder,
};
use extenddb_core::types::{GsiInput, Item, KeySchemaElement, ProjectionType, extract_key};
use serde::{Deserialize, Serialize};

use crate::item_storage::StoredValue;
use crate::items::item_key;
use crate::partition::key::index_key;
use crate::table::{TableRecord, statement};
use crate::{APPLICATION, Error, Json, Result, SqlBatch, SqlResultSet, SqlValue, account_target};

pub(crate) const MODULE: &str = "beyonddb-global-index";
pub(crate) const NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x46; 16]);
const SCHEMA: &str = include_str!("global_index_schema.sql");
// Lifecycle and projection mutations only return status values. Reserving an
// item-sized response would reduce concurrency without carrying images back.
static COMMANDS: [crab_cell_runtime::registry::OperationDescriptor; 6] = [
    status_operation(1),
    status_operation(2),
    status_operation(3),
    status_operation(4),
    status_operation(5),
    status_operation(6),
];

const fn status_operation(id: u32) -> crab_cell_runtime::registry::OperationDescriptor {
    crab_cell_runtime::registry::OperationDescriptor {
        output_limit: 4096,
        ..crate::operation(id)
    }
}

static QUERIES: [crab_cell_runtime::registry::OperationDescriptor; 5] = [
    crate::operation(1),
    crate::operation(2),
    crate::operation(3),
    crate::operation(4),
    crate::operation(5),
];

/// One immutable index generation within a base table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexRecord {
    pub id: String,
    pub specification: GsiInput,
}

impl GlobalIndexRecord {
    pub(crate) fn create(table_id: &str, specification: GsiInput, generation: u64) -> Result<Self> {
        let table = blake3::Hash::from_hex(table_id)
            .map_err(|_| Error::Identity("invalid global-index table ID"))?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"beyonddb.global-index.v1\0");
        hash.update(table.as_bytes());
        hash.update(specification.index_name.as_bytes());
        hash.update(&generation.to_be_bytes());
        let mut id = *table.as_bytes();
        id[16..].copy_from_slice(&hash.finalize().as_bytes()[..16]);
        Ok(Self {
            id: blake3::Hash::from_bytes(id).to_hex().to_string(),
            specification,
        })
    }

    pub(crate) fn key_schema(&self, table: &TableRecord) -> Vec<KeySchemaElement> {
        let mut keys = table.key_schema.clone();
        for key in &self.specification.key_schema {
            if !keys
                .iter()
                .any(|existing| existing.attribute_name == key.attribute_name)
            {
                keys.push(key.clone());
            }
        }
        keys
    }

    pub(crate) fn project(&self, table: &TableRecord, item: &Item) -> Option<Item> {
        if self
            .specification
            .key_schema
            .iter()
            .any(|key| !item.contains_key(&key.attribute_name))
        {
            return None;
        }
        if self.specification.projection.projection_type == ProjectionType::All {
            return Some(item.clone());
        }
        let mut projected = extract_key(item, &self.key_schema(table));
        if let Some(attributes) = &self.specification.projection.non_key_attributes {
            for name in attributes {
                if let Some(value) = item.get(name) {
                    projected.insert(name.clone(), value.clone());
                }
            }
        }
        Some(projected)
    }
}

/// Immutable index range and its independently fenced routing generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexPartitionSpec {
    pub table: TableRecord,
    pub index: GlobalIndexRecord,
    pub partition_id: [u8; 16],
    pub lower: Option<[u8; 16]>,
    pub upper: Option<[u8; 16]>,
    pub epoch: u64,
}

impl GlobalIndexPartitionSpec {
    fn contains(&self, key: &Item) -> Result<bool> {
        let hash = crate::data_key_hash(&self.index.id, key, &self.index.specification.key_schema)?;
        Ok(self.lower.is_none_or(|lower| hash >= lower)
            && self.upper.is_none_or(|upper| hash < upper))
    }

    fn valid_key(&self, key: &Item) -> bool {
        let schema = self.index.key_schema(&self.table);
        key.len() == schema.len()
            && extenddb_core::validation::validate_item_keys(
                key,
                &schema,
                &self.table.attribute_definitions,
            )
            .is_ok()
            && extenddb_core::validation::validate_key_sizes(
                key,
                &self.index.specification.key_schema,
                &Default::default(),
            )
            .is_ok()
            && crate::items::valid_key(&extract_key(key, &self.table.key_schema), &self.table)
    }
}

/// Source range epoch and actor sequence that produced an index change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProjectionVersion {
    pub source_epoch: u64,
    pub sequence: u64,
}

impl ProjectionVersion {
    fn bytes(self) -> Vec<u8> {
        [self.source_epoch.to_be_bytes(), self.sequence.to_be_bytes()].concat()
    }
}

/// Replace or tombstone one projected entry through its current index range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexMutation {
    pub index_id: String,
    pub epoch: u64,
    pub key: Item,
    pub version: ProjectionVersion,
    pub item: Option<Item>,
}

/// Result of an idempotent index projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GlobalIndexApplyOutcome {
    Applied,
    Replay,
    Superseded,
    StaleRoute,
    InvalidItem,
    VersionConflict,
}

pub(crate) struct GlobalIndexModule;

impl crab_cell_runtime::registry::CellModule for GlobalIndexModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            let mut hash = blake3::Hasher::new();
            hash.update(include_bytes!("global_index.rs"));
            hash.update(include_bytes!("global_index/read.rs"));
            hash.update(include_bytes!("global_index/transfer.rs"));
            hash.update(include_bytes!("table.rs"));
            hash.update(include_bytes!("item_storage.rs"));
            hash.update(include_bytes!("items.rs"));
            hash.update(include_bytes!("partition/key.rs"));
            hash.update(include_bytes!("partition/query.rs"));
            ModuleDescriptor {
                name: MODULE,
                source_digest: Digest::from_bytes(*hash.finalize().as_bytes()),
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
                namespaces: &[NamespaceDescriptor {
                    id: NAMESPACE,
                    name: MODULE,
                    role: CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                }],
            }
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        registry.bind_command::<InstallGlobalIndexPartition>()?;
        registry.bind_command::<ApplyGlobalIndexMutation>()?;
        registry.bind_command::<PrepareGlobalIndexSplit>()?;
        registry.bind_command::<ImportGlobalIndexEntry>()?;
        registry.bind_command::<ActivateGlobalIndexImport>()?;
        registry.bind_command::<OpenGlobalIndexImport>()?;
        registry.bind_query::<ReadGlobalIndexState>()?;
        registry.bind_query::<ExportGlobalIndexEntries>()?;
        registry.bind_query::<ReadGlobalIndexPartition>()?;
        registry.bind_query::<GlobalIndexQuery>()?;
        registry.bind_query::<GlobalIndexScan>()
    }
}

pub(crate) fn cell_type() -> Result<CellType> {
    CellType::new(MODULE, MODULE, NAMESPACE, CatalogRole::Sql, 1)?
        .with_entity_partitions()?
        .with_limits(512 * 1024 * 1024, 64 * 1024 * 1024)
}

fn target_for_tenant(
    tenant: TenantId,
    index_id: &str,
    partition_id: &[u8; 16],
) -> Result<CellTarget> {
    let id =
        blake3::Hash::from_hex(index_id).map_err(|_| Error::Identity("invalid global index ID"))?;
    if id.to_hex().as_str() != index_id || &id.as_bytes()[..16] != tenant.as_bytes() {
        return Err(Error::Identity("global index does not belong to account"));
    }
    let mut scope = index_id.as_bytes().to_vec();
    scope.extend_from_slice(partition_id);
    CellTarget::new(
        tenant,
        APPLICATION,
        NAMESPACE,
        &cell_type()?.entity_partition(&scope)?,
    )
}

/// Resolve an independently owned range of one global index generation.
pub fn global_index_target(
    account_id: &str,
    index_id: &str,
    partition_id: &[u8; 16],
) -> Result<CellTarget> {
    target_for_tenant(account_target(account_id)?.tenant(), index_id, partition_id)
}

/// Install the global-index SQL schema during Cell bootstrap.
pub fn initialize_global_index(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA)?;
    Ok(())
}

/// Install one immutable global-index range before publishing its route.
pub struct InstallGlobalIndexPartition;

impl Command for InstallGlobalIndexPartition {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexPartitionSpec>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(spec): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let target = target_for_tenant(
            context.target().tenant(),
            &spec.index.id,
            &spec.partition_id,
        )?;
        if target != *context.target()
            || spec.epoch == 0
            || spec
                .lower
                .zip(spec.upper)
                .is_some_and(|(low, high)| low >= high)
            || !spec.table.global_secondary_indexes.contains(&spec.index)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if let Some(existing) = read_spec(|batch| context.sql(batch))? {
            return Ok(
                if existing == spec && transfer::serving(|batch| context.sql(batch))? {
                    CommandResult::Success(Json(true))
                } else {
                    CommandResult::Rejected(Json(false))
                },
            );
        }
        context.sql(&statement(
            "INSERT INTO ddb_global_index (singleton, spec, state) VALUES (1, ?1, ?2)",
            vec![
                SqlValue::Blob(serde_json::to_vec(&spec)?),
                SqlValue::Blob(serde_json::to_vec(&GlobalIndexState::Serving)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

fn read_spec(
    mut sql: impl FnMut(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<GlobalIndexPartitionSpec>> {
    let rows = sql(&statement(
        "SELECT spec FROM ddb_global_index WHERE singleton = 1",
        vec![],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid global-index spec")),
    }
}

/// Read the installed index range before owner recovery or route publication.
pub struct ReadGlobalIndexPartition;

impl Query for ReadGlobalIndexPartition {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<GlobalIndexPartitionSpec>>;
    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        Ok(Json(read_spec(|batch| context.sql(batch))?))
    }
}

/// Apply a projection only when its source version supersedes this entry.
pub struct ApplyGlobalIndexMutation;

impl Command for ApplyGlobalIndexMutation {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexMutation>;
    type Output = Json<GlobalIndexApplyOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        use GlobalIndexApplyOutcome as Outcome;
        let rejected = |outcome| Ok(CommandResult::Rejected(Json(outcome)));
        let Some(spec) = read_spec(|batch| context.sql(batch))? else {
            return rejected(Outcome::StaleRoute);
        };
        if input.index_id != spec.index.id
            || input.epoch != spec.epoch
            || !transfer::serving(|batch| context.sql(batch))?
        {
            return rejected(Outcome::StaleRoute);
        }
        Ok(match apply(context, &spec, input, false)? {
            outcome @ (Outcome::Applied | Outcome::Replay | Outcome::Superseded) => {
                CommandResult::Success(Json(outcome))
            }
            outcome => CommandResult::Rejected(Json(outcome)),
        })
    }
}

fn apply(
    context: &mut CommandContext<'_, '_>,
    spec: &GlobalIndexPartitionSpec,
    input: GlobalIndexMutation,
    importing: bool,
) -> Result<GlobalIndexApplyOutcome> {
    use GlobalIndexApplyOutcome as Outcome;
    if input.version.sequence == 0 || !spec.valid_key(&input.key) {
        return Ok(Outcome::InvalidItem);
    }
    if !spec.contains(&input.key)? {
        return Ok(Outcome::StaleRoute);
    }
    let schema = spec.index.key_schema(&spec.table);
    if input.item.as_ref().is_some_and(|item| {
        extract_key(item, &schema) != input.key
            || spec.index.project(&spec.table, item).as_ref() != Some(item)
            || extenddb_core::types::item_size_bytes(item) > 400 * 1024
    }) {
        return Ok(Outcome::InvalidItem);
    }
    let key = item_key(&input.key, &schema)?;
    let version = input.version.bytes();
    let digest = blake3::hash(&serde_json::to_vec(&input.item)?);
    let rows = context.sql(&statement(
        "SELECT version, digest FROM ddb_global_index_items WHERE item_key = ?1",
        vec![SqlValue::Blob(key.clone())],
    ))?;
    if let Some(row) = rows[0].rows.first() {
        let [SqlValue::Blob(prior), SqlValue::Blob(prior_digest)] = row.as_slice() else {
            return Err(Error::Command("invalid global-index version"));
        };
        if prior == &version {
            return if prior_digest == digest.as_bytes() {
                Ok(Outcome::Replay)
            } else {
                Ok(Outcome::VersionConflict)
            };
        }
        // A sealed source is immutable. A second import for the same key
        // must match exactly, or the destination fingerprint is invalid.
        if importing {
            return Ok(Outcome::VersionConflict);
        }
        if prior > &version {
            return Ok(Outcome::Superseded);
        }
    }
    let (partition, sort) = index_key(&input.key, &spec.index.specification.key_schema)?;
    // Retain tombstones: an older delivery may arrive after deletion or a
    // key move. The full index/base key also separates both arms of a move.
    context.sql(&statement("INSERT INTO ddb_global_index_items (item_key, partition_key, sort_key, version, digest, item) VALUES (?1, ?2, ?3, ?4, ?5, NULL) ON CONFLICT(item_key) DO UPDATE SET version = excluded.version, digest = excluded.digest, item = NULL", vec![
            SqlValue::Blob(key.clone()), SqlValue::Blob(partition), SqlValue::Blob(sort),
            SqlValue::Blob(version), SqlValue::Blob(digest.as_bytes().to_vec()),
        ]))?;
    if let Some(item) = input.item {
        StoredValue::GlobalIndex(&key).write(context, &item)?;
    }
    Ok(Outcome::Applied)
}
