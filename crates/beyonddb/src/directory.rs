//! Bounded routing-directory nodes; serving-path extraction is in progress.

use crate::{
    APPLICATION, Error, Json, Result, RoutePagePartition, SqlBatch, SqlResultSet, SqlValue,
    account_target, table::statement,
};
use crab_cell_app::CellType;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::identity::{CellTarget, Digest, NamespaceId, TenantId};
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, Query, QueryContext, RegistryBuilder,
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

mod changes;
mod split;
pub use changes::*;
pub use split::*;

pub(crate) const MODULE: &str = "beyonddb-directory";
pub(crate) const NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x47; 16]);
const SCHEMA: &str = include_str!("directory/schema.sql");
const MAX_RANGES: usize = 1024;
const PAGE_SIZE: usize = 64;

static COMMANDS: [crab_cell_runtime::registry::OperationDescriptor; 7] = [
    crab_cell_runtime::registry::OperationDescriptor {
        input_limit: 1024 * 1024,
        ..crate::participant::phase_operation(1)
    },
    crate::participant::phase_operation(2),
    crate::participant::phase_operation(3),
    crate::participant::phase_operation(4),
    crate::participant::phase_operation(5),
    crate::participant::phase_operation(6),
    crate::participant::phase_operation(7),
];
static QUERIES: [crab_cell_runtime::registry::OperationDescriptor; 3] = [
    crate::participant::phase_operation(1),
    crab_cell_runtime::registry::OperationDescriptor {
        output_limit: 64 * 1024,
        ..crate::participant::phase_operation(2)
    },
    crab_cell_runtime::registry::OperationDescriptor {
        output_limit: 128 * 1024,
        ..crate::participant::phase_operation(3)
    },
];

/// Immutable scope and coverage of one node in a table generation's directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectorySpec {
    pub table_id: String,
    pub node_id: [u8; 16],
    pub lower: [u8; 16],
    pub upper: Option<[u8; 16]>,
    pub depth: u8,
}

impl DirectorySpec {
    fn valid(&self) -> bool {
        self.depth < 128 && self.upper.is_none_or(|upper| self.lower < upper)
    }

    fn contains(&self, hash: [u8; 16]) -> bool {
        hash >= self.lower && self.upper.is_none_or(|upper| hash < upper)
    }
}

/// A durable copy fence tying children to one frozen parent generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectorySplit {
    pub parent: DirectorySpec,
    pub version: u64,
    pub children: [DirectorySpec; 2],
    pub fingerprints: [[u8; 32]; 2],
}

/// Publication state of a directory node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DirectoryMode {
    Leaf,
    Importing,
    Frozen(DirectorySplit),
    Branch(DirectorySplit),
}

/// Current node version and its immutable scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryState {
    pub spec: DirectorySpec,
    pub version: u64,
    pub initial_fingerprint: [u8; 32],
    pub mode: DirectoryMode,
}

pub(crate) struct DirectoryModule;
impl crab_cell_runtime::registry::CellModule for DirectoryModule {
    const NAME: &'static str = MODULE;
    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            let mut source = blake3::Hasher::new();
            source.update(include_bytes!("directory.rs"));
            source.update(include_bytes!("directory/changes.rs"));
            source.update(include_bytes!("directory/split.rs"));
            ModuleDescriptor {
                name: MODULE,
                source_digest: Digest::from_bytes(*source.finalize().as_bytes()),
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
        registry.bind_command::<InstallDirectory>()?;
        registry.bind_command::<BeginDirectoryChange>()?;
        registry.bind_command::<PublishDirectoryChange>()?;
        registry.bind_command::<FinishDirectoryChange>()?;
        registry.bind_command::<FreezeDirectory>()?;
        registry.bind_command::<PublishDirectorySplit>()?;
        registry.bind_command::<OpenDirectory>()?;
        registry.bind_query::<ReadDirectory>()?;
        registry.bind_query::<ReadDirectoryPage>()?;
        registry.bind_query::<ReadDirectoryChanges>()
    }
}

pub(crate) fn cell_type() -> Result<CellType> {
    CellType::new(MODULE, MODULE, NAMESPACE, CatalogRole::Sql, 1)?
        .with_entity_partitions()?
        .with_limits(16 * 1024 * 1024, 4 * 1024 * 1024)
}

fn target_for_tenant(tenant: TenantId, spec: &DirectorySpec) -> Result<CellTarget> {
    let id = blake3::Hash::from_hex(&spec.table_id)
        .map_err(|_| Error::Identity("invalid directory table ID"))?;
    if id.to_hex().as_str() != spec.table_id || &id.as_bytes()[..16] != tenant.as_bytes() {
        return Err(Error::Identity("directory does not belong to account"));
    }
    let mut scope = id.as_bytes().to_vec();
    scope.extend_from_slice(&spec.node_id);
    CellTarget::new(
        tenant,
        APPLICATION,
        NAMESPACE,
        &cell_type()?.entity_partition(&scope)?,
    )
}

/// Resolve a directory node without an account-owned node inventory.
pub fn directory_target(account_id: &str, spec: &DirectorySpec) -> Result<CellTarget> {
    target_for_tenant(account_target(account_id)?.tenant(), spec)
}

/// Install the directory schema during Cell bootstrap.
pub fn initialize_directory(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA)?;
    Ok(())
}

fn state(
    sql: impl FnOnce(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<DirectoryState>> {
    let rows = sql(&statement(
        "SELECT state FROM ddb_directory WHERE singleton = 1",
        vec![],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(value)]) => Ok(Some(serde_json::from_slice(value)?)),
        _ => Err(Error::Command("invalid directory state")),
    }
}

fn save(context: &CommandContext<'_, '_>, state: &DirectoryState) -> Result<()> {
    context.sql(&statement("INSERT INTO ddb_directory VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET state = excluded.state",
        vec![SqlValue::Blob(serde_json::to_vec(state)?)],))?;
    Ok(())
}

fn valid_ranges(spec: &DirectorySpec, ranges: &[RoutePagePartition]) -> bool {
    if !spec.valid() || ranges.is_empty() || ranges.len() > MAX_RANGES {
        return false;
    }
    let mut lower = spec.lower;
    let mut ids = std::collections::HashSet::new();
    for (position, range) in ranges.iter().enumerate() {
        if range.lower != lower
            || range.epoch == 0
            || !ids.insert(range.partition_id)
            || range.upper.is_some_and(|upper| upper <= range.lower)
        {
            return false;
        }
        if position + 1 == ranges.len() {
            return range.upper == spec.upper;
        }
        let Some(next) = range.upper else {
            return false;
        };
        lower = next;
    }
    false
}

fn insert_range(context: &CommandContext<'_, '_>, range: &RoutePagePartition) -> Result<()> {
    context.sql(&statement(
        "INSERT INTO ddb_directory_ranges (lower_bound, record) VALUES (?1, ?2)",
        vec![
            SqlValue::Blob(range.lower.to_vec()),
            SqlValue::Blob(serde_json::to_vec(range)?),
        ],
    ))?;
    Ok(())
}

fn ranges(
    sql: impl FnOnce(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    lower: [u8; 16],
    limit: usize,
) -> Result<Vec<RoutePagePartition>> {
    let rows = sql(&statement(
        "SELECT record FROM ddb_directory_ranges WHERE lower_bound >= ?1 ORDER BY lower_bound LIMIT ?2",
        vec![
            SqlValue::Blob(lower.to_vec()),
            SqlValue::Integer(limit as i64),
        ],
    ))?;
    rows[0]
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [SqlValue::Blob(bytes)] => Ok(serde_json::from_slice(bytes)?),
            _ => Err(Error::Command("invalid directory range")),
        })
        .collect()
}

fn all_ranges(context: &CommandContext<'_, '_>) -> Result<Vec<RoutePagePartition>> {
    let mut result = Vec::new();
    let mut lower = [0; 16];
    loop {
        let page = ranges(|batch| context.sql(batch), lower, PAGE_SIZE)?;
        if page.is_empty() {
            break;
        }
        let next = page.last().and_then(|range| range.upper);
        result.extend(page);
        if result.len() > MAX_RANGES {
            return Err(Error::Command("directory exceeds range bound"));
        }
        let Some(next) = next else {
            break;
        };
        lower = next;
    }
    Ok(result)
}

fn fingerprint(spec: &DirectorySpec, ranges: &[RoutePagePartition]) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"beyonddb.directory-copy.v1\0");
    hash.update(&serde_json::to_vec(spec)?);
    hash.update(&serde_json::to_vec(ranges)?);
    Ok(*hash.finalize().as_bytes())
}

/// Initial contiguous membership, optionally copied from a frozen parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryInstall {
    pub spec: DirectorySpec,
    pub ranges: Vec<RoutePagePartition>,
    pub source: Option<DirectorySplit>,
}

/// Install a root leaf or a verified, initially fenced child copy.
pub struct InstallDirectory;
impl Command for InstallDirectory {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryInstall>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if target_for_tenant(context.target().tenant(), &input.spec)? != *context.target()
            || !valid_ranges(&input.spec, &input.ranges)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let mode = match &input.source {
            None if input.spec.depth == 0
                && input.spec.node_id == [0; 16]
                && input.spec.lower == [0; 16]
                && input.spec.upper.is_none() =>
            {
                DirectoryMode::Leaf
            }
            Some(source) if source.valid() => {
                let Some(position) = source
                    .children
                    .iter()
                    .position(|child| child == &input.spec)
                else {
                    return Ok(CommandResult::Rejected(Json(false)));
                };
                let digest = fingerprint(&input.spec, &input.ranges)?;
                if digest != source.fingerprints[position] {
                    return Ok(CommandResult::Rejected(Json(false)));
                }
                DirectoryMode::Importing
            }
            _ => return Ok(CommandResult::Rejected(Json(false))),
        };
        let initial_fingerprint = fingerprint(&input.spec, &input.ranges)?;
        let installed = DirectoryState {
            spec: input.spec,
            version: 1,
            initial_fingerprint,
            mode,
        };
        if let Some(existing) = state(|batch| context.sql(batch))? {
            // The birth digest survives mutations and splitting. A delayed install
            // can acknowledge the original copy without replacing newer membership.
            let same = existing.spec == installed.spec
                && existing.initial_fingerprint == installed.initial_fingerprint;
            return Ok(if same {
                CommandResult::Success(Json(true))
            } else {
                CommandResult::Rejected(Json(false))
            });
        }
        for range in &input.ranges {
            insert_range(context, range)?;
        }
        save(context, &installed)?;
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Read the immutable node identity and its current routing fence.
pub struct ReadDirectory;
impl Query for ReadDirectory {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<DirectoryState>>;
    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        Ok(Json(state(|batch| context.sql(batch))?))
    }
}

/// Logical starting position and optional leaf-version fence for a route page.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryPageInput {
    pub hash: [u8; 16],
    pub expected_version: Option<u64>,
}

/// Bounded route page or the children that now own its logical position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DirectoryPage {
    Unavailable,
    Changed,
    Redirect([DirectorySpec; 2]),
    Leaf {
        version: u64,
        ranges: Vec<RoutePagePartition>,
    },
}

/// Read at most 64 entries, including the owner containing the logical position.
pub struct ReadDirectoryPage;
impl Query for ReadDirectoryPage {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryPageInput>;
    type Output = Json<DirectoryPage>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let Some(state) = state(|batch| context.sql(batch))? else {
            return Ok(Json(DirectoryPage::Unavailable));
        };
        if !state.spec.contains(input.hash) {
            return Ok(Json(DirectoryPage::Changed));
        }
        match state.mode {
            DirectoryMode::Branch(split) => {
                return Ok(Json(DirectoryPage::Redirect(split.children)));
            }
            DirectoryMode::Importing => return Ok(Json(DirectoryPage::Unavailable)),
            DirectoryMode::Leaf | DirectoryMode::Frozen(_) => {}
        }
        if input
            .expected_version
            .is_some_and(|version| version != state.version)
        {
            return Ok(Json(DirectoryPage::Changed));
        }
        let rows = context.sql(&statement("SELECT lower_bound FROM ddb_directory_ranges WHERE lower_bound <= ?1 ORDER BY lower_bound DESC LIMIT 1",vec![SqlValue::Blob(input.hash.to_vec())]))?;
        let Some([SqlValue::Blob(lower)]) = rows[0].rows.first().map(Vec::as_slice) else {
            return Err(Error::Command("directory coverage is missing"));
        };
        let lower = lower
            .as_slice()
            .try_into()
            .map_err(|_| Error::Command("invalid directory bound"))?;
        Ok(Json(DirectoryPage::Leaf {
            version: state.version,
            ranges: ranges(|batch| context.sql(batch), lower, PAGE_SIZE)?,
        }))
    }
}
