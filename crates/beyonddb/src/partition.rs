//! Independently owned item range in one SQL Cell.

mod key;
mod query;
mod scan;
mod transaction;
mod ttl;

pub use key::data_key_hash;
pub use query::*;
pub use scan::*;
pub use transaction::*;
pub use ttl::*;

use std::sync::OnceLock;

use crab_cell_runtime::identity::Digest;
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, OperationDescriptor, Query, QueryContext, RegistryBuilder,
};
use extenddb_core::expression::{Expr, ExpressionMaps, UpdateAction};
use extenddb_core::types::{Item, KeySchemaElement};
use serde::{Deserialize, Serialize};

use crate::expression_wire::{WireCondition, WireUpdate};
use crate::items::{decode_item, item_key, valid_item, valid_key};
use crate::table::{TableRecord, statement};
use crate::{
    DATA_MODULE, DATA_NAMESPACE, Error, Json, OPERATION_BYTES, Result, SqlResultSet, SqlValue,
};
use key::index_key;

pub(crate) static SCHEMA: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "{}\n{}",
        crate::participant::SCHEMA,
        include_str!("partition_schema.sql")
    )
});

static NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: DATA_NAMESPACE,
    name: DATA_MODULE,
    role: crab_cell_runtime::cell::catalog::CatalogRole::Sql,
    shards: 1,
    effect_targets: &[],
    dead_letter: None,
}];
static COMMANDS: [OperationDescriptor; 13] = [
    operation(1),
    operation(2),
    operation(3),
    operation(4),
    operation(5),
    operation(6),
    operation(7),
    operation(8),
    operation(9),
    operation(10),
    operation(11),
    operation(12),
    operation(13),
];
static QUERIES: [OperationDescriptor; 11] = [
    operation(1),
    operation(2),
    operation(3),
    operation(4),
    operation(5),
    operation(6),
    operation(7),
    operation(8),
    operation(9),
    operation(10),
    operation(11),
];

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

pub(crate) struct DataModule;

impl crab_cell_runtime::registry::CellModule for DataModule {
    const NAME: &'static str = DATA_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: DATA_MODULE,
            source_digest: {
                let mut source = blake3::Hasher::new();
                source.update(include_bytes!("lib.rs"));
                source.update(include_bytes!("partition.rs"));
                source.update(include_bytes!("partition/key.rs"));
                source.update(include_bytes!("partition/query.rs"));
                source.update(include_bytes!("partition/scan.rs"));
                source.update(include_bytes!("partition/transaction.rs"));
                source.update(include_bytes!("partition/transaction/participant.rs"));
                source.update(include_bytes!("partition/ttl.rs"));
                source.update(include_bytes!("items.rs"));
                source.update(include_bytes!("participant.rs"));
                source.update(include_bytes!("table.rs"));
                source.update(include_bytes!("expression_wire.rs"));
                Digest::from_bytes(*source.finalize().as_bytes())
            },
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: &SCHEMA,
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
        registry.bind_command::<InstallPartition>()?;
        registry.bind_command::<PartitionPut>()?;
        registry.bind_command::<PartitionDelete>()?;
        registry.bind_command::<PartitionUpdate>()?;
        registry.bind_command::<SealPartition>()?;
        registry.bind_command::<ImportPartitionItem>()?;
        registry.bind_command::<ActivateImportedPartition>()?;
        registry.bind_command::<OpenPartition>()?;
        registry.bind_command::<PartitionTransactWrite>()?;
        registry.bind_command::<ConfigurePartitionTtl>()?;
        registry.bind_command::<BackfillPartitionTtl>()?;
        registry.bind_command::<PreparePartitionTransaction>()?;
        registry.bind_command::<ResolvePartitionTransaction>()?;
        registry.bind_query::<PartitionGet>()?;
        registry.bind_query::<PartitionScan>()?;
        registry.bind_query::<PartitionExport>()?;
        registry.bind_query::<ReadPartitionState>()?;
        registry.bind_query::<PartitionQuery>()?;
        registry.bind_query::<PartitionTransactGet>()?;
        registry.bind_query::<PartitionUsage>()?;
        registry.bind_query::<ReadExpiredPartition>()?;
        registry.bind_query::<ReadPartitionTtl>()?;
        registry.bind_query::<ReadPartitionTransaction>()?;
        registry.bind_query::<ReadPartitionTransactionResult>()
    }
}

/// Installs the immutable table key contract and data Cell epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionSpec {
    /// Account table record verified by the provisioning boundary.
    pub table: TableRecord,
    /// Stable ID of this partition within the table directory.
    pub partition_id: [u8; 16],
    /// Inclusive lower key-hash bound, absent for the first range.
    pub lower: Option<[u8; 16]>,
    /// Exclusive upper key-hash bound, absent for the last range.
    pub upper: Option<[u8; 16]>,
    /// This data Cell's epoch accepted by writes and reads.
    pub epoch: u64,
}

/// Initial serving state or an import-only split child.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionInstall {
    /// Install a freshly created table range for normal requests.
    Serving(PartitionSpec),
    /// Install a split child that may only accept source item imports.
    Importing {
        /// Child range and epoch.
        spec: PartitionSpec,
        /// Durable source fence that defines this child's range.
        source: PartitionSeal,
    },
}

impl PartitionInstall {
    fn spec(&self) -> &PartitionSpec {
        match self {
            Self::Serving(spec) | Self::Importing { spec, .. } => spec,
        }
    }
}

/// Item count and order-independent digest of one import range.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// Number of distinct primary keys imported.
    pub count: u64,
    /// XOR of domain-separated Blake3 item hashes.
    pub digest: [u8; 32],
}

impl ImportSummary {
    /// Include one distinct item in a source or destination fingerprint.
    pub fn include(&mut self, item: &Item, schema: &[KeySchemaElement]) -> Result<()> {
        let key = item_key(item, schema)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"beyonddb.item.import.v1\0");
        hasher.update(&key);
        hasher.update(&serde_json::to_vec(item)?);
        for (current, added) in self.digest.iter_mut().zip(hasher.finalize().as_bytes()) {
            *current ^= added;
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(Error::Command("import item count overflow"))?;
        Ok(())
    }
}

/// Durable lifecycle of a data Cell range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionState {
    /// An initial range serving ordinary requests.
    Serving,
    /// A split child accepting only copies from the sealed source.
    Importing {
        /// Source fence defining this import.
        source: PartitionSeal,
        /// Imported item fingerprint so far.
        summary: ImportSummary,
    },
    /// A verified split child awaiting directory publication.
    Activated {
        /// Source fence defining this child.
        source: PartitionSeal,
        /// Fingerprint verified at activation.
        summary: ImportSummary,
    },
    /// A split child serving requests after directory publication.
    Opened {
        /// Source fence defining this child.
        source: PartitionSeal,
        /// Fingerprint verified at activation.
        summary: ImportSummary,
    },
    /// A source range permanently fenced for a split.
    Sealed(PartitionSeal),
}

/// Installed partition contract and its durable lifecycle state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionStatus {
    /// Immutable key range and data Cell epoch.
    pub spec: PartitionSpec,
    /// Current lifecycle state.
    pub state: PartitionState,
}

/// Read a partition's installed contract and lifecycle from its Cell.
pub struct ReadPartitionState;

impl Query for ReadPartitionState {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<PartitionStatus>>;

    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT spec, state FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(None));
        };
        let [SqlValue::Blob(spec), SqlValue::Blob(state)] = row.as_slice() else {
            return Err(Error::Command("invalid partition status row"));
        };
        Ok(Json(Some(PartitionStatus {
            spec: serde_json::from_slice(spec)?,
            state: serde_json::from_slice(state)?,
        })))
    }
}

/// Report durable item storage in one data Cell for capacity planning.
pub struct PartitionUsage;

/// Logical item bytes and physical SQLite image size in one data Cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionUsageReport {
    /// Number of committed items in this Cell.
    pub item_count: u64,
    /// Stored JSON and key bytes, excluding SQLite page and index overhead.
    pub item_bytes: u64,
    /// SQLite pages including indexes and runtime tables; excludes WAL and LTX files.
    pub database_bytes: u64,
}

impl Query for PartitionUsage {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<PartitionUsageReport>;

    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(item) + LENGTH(item_key) + \
             LENGTH(partition_key) + LENGTH(sort_key)), 0) FROM ddb_partition_items",
            vec![],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Err(Error::Command("partition usage row missing"));
        };
        let [SqlValue::Integer(count), SqlValue::Integer(bytes)] = row.as_slice() else {
            return Err(Error::Command("invalid partition usage row"));
        };
        Ok(Json(PartitionUsageReport {
            item_count: u64::try_from(*count)
                .map_err(|_| Error::Command("negative partition item count"))?,
            item_bytes: u64::try_from(*bytes)
                .map_err(|_| Error::Command("negative partition item bytes"))?,
            database_bytes: context.database_bytes()?,
        }))
    }
}

impl PartitionSpec {
    pub(super) fn contains(&self, hash: [u8; 16]) -> bool {
        self.lower.is_none_or(|bound| hash >= bound) && self.upper.is_none_or(|bound| hash < bound)
    }

    fn valid(&self) -> bool {
        self.epoch > 0
            && self
                .lower
                .zip(self.upper)
                .is_none_or(|(low, high)| low < high)
    }
}

/// Result of installing a data partition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InstallPartitionOutcome {
    /// The exact partition contract is durable.
    Installed,
    /// The target or range does not match the requested contract.
    InvalidSpec,
    /// The Cell already contains a different partition contract.
    Conflict,
}

/// Install one table data range in a Cell.
pub struct InstallPartition;

impl Command for InstallPartition {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionInstall>;
    type Output = Json<InstallPartitionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(install): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let spec = install.spec().clone();
        let (Ok(expected), Ok(table_id)) = (
            crate::data_partition_bytes(&spec.table.id, &spec.partition_id),
            blake3::Hash::from_hex(&spec.table.id),
        ) else {
            return Ok(CommandResult::Rejected(Json(
                InstallPartitionOutcome::InvalidSpec,
            )));
        };
        if !spec.valid()
            || expected.as_slice() != context.target().partition()
            || &table_id.as_bytes()[..16] != context.target().tenant().as_bytes()
            || matches!(&install, PartitionInstall::Importing { source, .. } if !source.valid_child(&spec))
        {
            return Ok(CommandResult::Rejected(Json(
                InstallPartitionOutcome::InvalidSpec,
            )));
        }
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        if let Some(existing) = decode_spec(&rows[0])? {
            let compatible = match (&install, command_state(context)?) {
                (PartitionInstall::Serving(_), Some(PartitionState::Serving)) => true,
                (
                    PartitionInstall::Importing { source, .. },
                    Some(PartitionState::Importing {
                        source: current, ..
                    })
                    | Some(PartitionState::Activated {
                        source: current, ..
                    })
                    | Some(PartitionState::Opened {
                        source: current, ..
                    }),
                ) => source == &current,
                _ => false,
            };
            let outcome = if existing == spec && compatible {
                InstallPartitionOutcome::Installed
            } else {
                InstallPartitionOutcome::Conflict
            };
            return Ok(if outcome == InstallPartitionOutcome::Installed {
                CommandResult::Success(Json(outcome))
            } else {
                CommandResult::Rejected(Json(outcome))
            });
        }
        let state = match install {
            PartitionInstall::Serving(_) => PartitionState::Serving,
            PartitionInstall::Importing { source, .. } => PartitionState::Importing {
                source,
                summary: ImportSummary::default(),
            },
        };
        context.sql(&statement(
            "INSERT INTO ddb_partition (singleton, spec, state) VALUES (1, ?1, ?2)",
            vec![
                SqlValue::Blob(serde_json::to_vec(&spec)?),
                SqlValue::Blob(serde_json::to_vec(&state)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(
            InstallPartitionOutcome::Installed,
        )))
    }
}

/// The destination contract that permanently fences one source partition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionSeal {
    /// Immutable table identity.
    pub table_id: String,
    /// Existing source partition identity.
    pub source_partition_id: [u8; 16],
    /// Source data Cell epoch.
    pub epoch: u64,
    /// Directory epoch assigned to both child data Cells.
    pub next_epoch: u64,
    /// Source range's inclusive lower bound.
    pub source_lower: Option<[u8; 16]>,
    /// Source range's exclusive upper bound.
    pub source_upper: Option<[u8; 16]>,
    /// First hash owned by the right child.
    pub boundary: [u8; 16],
    /// Fresh left child identity.
    pub left_partition_id: [u8; 16],
    /// Fresh right child identity.
    pub right_partition_id: [u8; 16],
}

impl PartitionSeal {
    fn valid_for(&self, source: &PartitionSpec) -> bool {
        self.table_id == source.table.id
            && self.source_partition_id == source.partition_id
            && self.epoch == source.epoch
            && self.next_epoch > source.epoch
            && self.source_lower == source.lower
            && self.source_upper == source.upper
            && self.boundary > source.lower.unwrap_or([0; 16])
            && source.upper.is_none_or(|upper| self.boundary < upper)
            && self.left_partition_id != source.partition_id
            && self.right_partition_id != source.partition_id
            && self.left_partition_id != self.right_partition_id
    }

    fn valid_child(&self, child: &PartitionSpec) -> bool {
        if child.table.id != self.table_id
            || child.epoch != self.next_epoch
            || child.partition_id == self.source_partition_id
            || self.next_epoch <= self.epoch
            || self.left_partition_id == self.right_partition_id
            || self.left_partition_id == self.source_partition_id
            || self.right_partition_id == self.source_partition_id
            || self.boundary <= self.source_lower.unwrap_or([0; 16])
            || self
                .source_upper
                .is_some_and(|upper| self.boundary >= upper)
        {
            return false;
        }
        (child.partition_id == self.left_partition_id
            && child.lower == self.source_lower
            && child.upper == Some(self.boundary))
            || (child.partition_id == self.right_partition_id
                && child.lower == Some(self.boundary)
                && child.upper == self.source_upper)
    }
}

/// Outcome of fencing the source before a split copy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SealPartitionOutcome {
    /// The exact source fence is durable.
    Sealed,
    /// No partition contract is installed.
    NotInstalled,
    /// The source table or epoch is stale.
    StaleRoute,
    /// The child boundary or identities are invalid.
    InvalidSeal,
    /// The source is already sealed for a different split.
    Conflict,
    /// Prepared item intents must resolve before this source can be copied.
    InFlightTransaction,
}

/// Permanently fence ordinary reads and writes on a split source.
///
/// The caller must first read the durable account split plan and derive this
/// seal from it. A sealed source has no local unseal path; recovery must finish
/// the copy and directory switch before ordinary requests can proceed.
pub struct SealPartition;

impl Command for SealPartition {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionSeal>;
    type Output = Json<SealPartitionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(seal): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(source) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                SealPartitionOutcome::NotInstalled,
            )));
        };
        if source.table.id != seal.table_id || source.epoch != seal.epoch {
            return Ok(CommandResult::Rejected(Json(
                SealPartitionOutcome::StaleRoute,
            )));
        }
        if !seal.valid_for(&source) {
            return Ok(CommandResult::Rejected(Json(
                SealPartitionOutcome::InvalidSeal,
            )));
        }
        match command_state(context)? {
            Some(PartitionState::Sealed(existing)) if existing == seal => {
                return Ok(CommandResult::Success(Json(SealPartitionOutcome::Sealed)));
            }
            Some(PartitionState::Serving | PartitionState::Opened { .. }) => {}
            Some(_) => {
                return Ok(CommandResult::Rejected(Json(
                    SealPartitionOutcome::Conflict,
                )));
            }
            None => return Err(Error::Command("installed partition has no state")),
        }
        // The current split copies live rows only; sealing with an intent
        // would strand its decision on the old owner.
        if transaction::has_transaction_locks(context)? {
            return Ok(CommandResult::Rejected(Json(
                SealPartitionOutcome::InFlightTransaction,
            )));
        }
        update_state(context, &PartitionState::Sealed(seal))?;
        Ok(CommandResult::Success(Json(SealPartitionOutcome::Sealed)))
    }
}

/// One item copied from a sealed source into an import-only child.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionImportInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Destination data Cell epoch.
    pub epoch: u64,
    /// Full source item image.
    pub item: Item,
}

/// Outcome of an idempotent child item import.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionImportOutcome {
    /// This exact item image is durable in the child.
    Imported,
    /// No child partition contract is installed.
    NotInstalled,
    /// The table or destination epoch is stale.
    StaleRoute,
    /// The child no longer accepts source imports.
    NotImporting,
    /// The item violates the key or item contract.
    InvalidItem,
    /// The item hashes outside this child's range.
    WrongPartition,
    /// A different image already exists under this primary key.
    Conflict,
}

/// Import one source item without overwriting a different child image.
pub struct ImportPartitionItem;

impl Command for ImportPartitionItem {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionImportInput>;
    type Output = Json<PartitionImportOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                PartitionImportOutcome::NotInstalled,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                PartitionImportOutcome::StaleRoute,
            )));
        }
        let Some(PartitionState::Importing {
            source,
            mut summary,
        }) = command_state(context)?
        else {
            return Ok(CommandResult::Rejected(Json(
                PartitionImportOutcome::NotImporting,
            )));
        };
        if !valid_item(&input.item, &spec.table) {
            return Ok(CommandResult::Rejected(Json(
                PartitionImportOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.item, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.item,
            &spec.table.key_schema,
        )?) {
            return Ok(CommandResult::Rejected(Json(
                PartitionImportOutcome::WrongPartition,
            )));
        }
        if let Some(existing) = command_item(context, &key)? {
            return Ok(if existing == input.item {
                CommandResult::Success(Json(PartitionImportOutcome::Imported))
            } else {
                CommandResult::Rejected(Json(PartitionImportOutcome::Conflict))
            });
        }
        summary.include(&input.item, &spec.table.key_schema)?;
        write_item(context, key, &input.item, &spec.table.key_schema)?;
        update_state(context, &PartitionState::Importing { source, summary })?;
        Ok(CommandResult::Success(Json(
            PartitionImportOutcome::Imported,
        )))
    }
}

/// Verified final fingerprint expected for an import-only child.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivateImportedPartitionInput {
    /// Source seal used when installing the child.
    pub source: PartitionSeal,
    /// Complete fingerprint computed from the sealed source export.
    pub expected: ImportSummary,
}

/// Outcome of moving one verified child into serving state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActivateImportedPartitionOutcome {
    /// This exact imported image is now serving.
    Activated,
    /// No child partition contract is installed.
    NotInstalled,
    /// The child range or source seal does not match.
    StaleRoute,
    /// The child has a different lifecycle state or source.
    Conflict,
    /// The imported count or digest does not match the sealed source.
    Incomplete,
}

/// Close imports and durably open one verified split child for routed traffic.
///
/// The caller must compute `expected` from a complete sealed source export.
/// Once activated, delayed import commands cannot overwrite serving writes.
pub struct ActivateImportedPartition;

impl Command for ActivateImportedPartition {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ActivateImportedPartitionInput>;
    type Output = Json<ActivateImportedPartitionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                ActivateImportedPartitionOutcome::NotInstalled,
            )));
        };
        if !input.source.valid_child(&spec) {
            return Ok(CommandResult::Rejected(Json(
                ActivateImportedPartitionOutcome::StaleRoute,
            )));
        }
        match command_state(context)? {
            Some(PartitionState::Activated { source, summary })
            | Some(PartitionState::Opened { source, summary })
                if source == input.source && summary == input.expected =>
            {
                return Ok(CommandResult::Success(Json(
                    ActivateImportedPartitionOutcome::Activated,
                )));
            }
            Some(PartitionState::Importing { source, summary }) if source == input.source => {
                if summary != input.expected {
                    return Ok(CommandResult::Rejected(Json(
                        ActivateImportedPartitionOutcome::Incomplete,
                    )));
                }
                update_state(context, &PartitionState::Activated { source, summary })?;
            }
            Some(_) => {
                return Ok(CommandResult::Rejected(Json(
                    ActivateImportedPartitionOutcome::Conflict,
                )));
            }
            None => return Err(Error::Command("installed partition has no state")),
        }
        Ok(CommandResult::Success(Json(
            ActivateImportedPartitionOutcome::Activated,
        )))
    }
}

/// Outcome of opening a verified child after its route is published.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OpenPartitionOutcome {
    /// The exact child is open for ordinary requests.
    Opened,
    /// No partition contract is installed.
    NotInstalled,
    /// The child does not match the expected source seal.
    StaleRoute,
    /// The child was not verified or has a different fingerprint.
    NotActivated,
}

/// Open a verified child once its route has been published by the account Cell.
///
/// The trusted caller must observe the published directory route first.
pub struct OpenPartition;

impl Command for OpenPartition {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ActivateImportedPartitionInput>;
    type Output = Json<OpenPartitionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                OpenPartitionOutcome::NotInstalled,
            )));
        };
        if !input.source.valid_child(&spec) {
            return Ok(CommandResult::Rejected(Json(
                OpenPartitionOutcome::StaleRoute,
            )));
        }
        match command_state(context)? {
            Some(PartitionState::Opened { source, summary })
                if source == input.source && summary == input.expected =>
            {
                return Ok(CommandResult::Success(Json(OpenPartitionOutcome::Opened)));
            }
            Some(PartitionState::Activated { source, summary })
                if source == input.source && summary == input.expected =>
            {
                update_state(context, &PartitionState::Opened { source, summary })?;
            }
            Some(_) => {
                return Ok(CommandResult::Rejected(Json(
                    OpenPartitionOutcome::NotActivated,
                )));
            }
            None => return Err(Error::Command("installed partition has no state")),
        }
        Ok(CommandResult::Success(Json(OpenPartitionOutcome::Opened)))
    }
}

/// Put one item under a verified data Cell epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionPutInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell epoch used for routing.
    pub epoch: u64,
    /// Full item.
    pub item: Item,
    /// Optional condition against the previous image.
    pub condition: Option<WireCondition>,
}

/// Result of a partition-local PutItem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionPutOutcome {
    /// The replacement committed with its previous image.
    Applied(Option<Item>),
    /// No partition contract is installed.
    NotInstalled,
    /// The table ID or data Cell epoch is stale.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// This split child is still importing its source range.
    NotReady,
    /// The key hashes outside this partition's range.
    WrongPartition,
    /// The item violates the table key or item contract.
    InvalidItem,
    /// The condition was false against the previous image.
    ConditionFailed(Option<Item>),
    /// The expression failed during evaluation.
    InvalidExpression(String),
    /// A prepared transaction owns this item key.
    TransactionConflict,
}

/// Replace one item in its owning data Cell.
pub struct PartitionPut;

impl Command for PartitionPut {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionPutInput>;
    type Output = Json<PartitionPutOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                PartitionPutOutcome::NotInstalled,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                PartitionPutOutcome::StaleRoute,
            )));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => {
                return Ok(CommandResult::Rejected(Json(PartitionPutOutcome::Sealed)));
            }
            AccessState::Importing => {
                return Ok(CommandResult::Rejected(Json(PartitionPutOutcome::NotReady)));
            }
        }
        if !valid_item(&input.item, &spec.table) {
            return Ok(CommandResult::Rejected(Json(
                PartitionPutOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.item, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.item,
            &spec.table.key_schema,
        )?) {
            return Ok(CommandResult::Rejected(Json(
                PartitionPutOutcome::WrongPartition,
            )));
        }
        if transaction::key_locked(context, &key)? {
            return Ok(CommandResult::Rejected(Json(
                PartitionPutOutcome::TransactionConflict,
            )));
        }
        let old = command_item(context, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionPutOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionPutOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        write_item(context, key, &input.item, &spec.table.key_schema)?;
        Ok(CommandResult::Success(Json(PartitionPutOutcome::Applied(
            old,
        ))))
    }
}

/// Delete one item under a verified data Cell epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionDeleteInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell epoch used for routing.
    pub epoch: u64,
    /// Complete primary key.
    pub key: Item,
    /// Optional condition against the previous image.
    pub condition: Option<WireCondition>,
}

/// Result of a partition-local DeleteItem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionDeleteOutcome {
    /// The deletion committed with its previous image.
    Applied(Option<Item>),
    /// No partition contract is installed.
    NotInstalled,
    /// The table ID or data Cell epoch is stale.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// This split child is still importing its source range.
    NotReady,
    /// The key hashes outside this partition's range.
    WrongPartition,
    /// The key violates the table key contract.
    InvalidKey,
    /// The condition was false against the previous image.
    ConditionFailed(Option<Item>),
    /// The expression failed during evaluation.
    InvalidExpression(String),
    /// A prepared transaction owns this item key.
    TransactionConflict,
}

/// Delete one item in its owning data Cell.
pub struct PartitionDelete;

impl Command for PartitionDelete {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionDeleteInput>;
    type Output = Json<PartitionDeleteOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                PartitionDeleteOutcome::NotInstalled,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                PartitionDeleteOutcome::StaleRoute,
            )));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => {
                return Ok(CommandResult::Rejected(Json(
                    PartitionDeleteOutcome::Sealed,
                )));
            }
            AccessState::Importing => {
                return Ok(CommandResult::Rejected(Json(
                    PartitionDeleteOutcome::NotReady,
                )));
            }
        }
        if !valid_key(&input.key, &spec.table) {
            return Ok(CommandResult::Rejected(Json(
                PartitionDeleteOutcome::InvalidKey,
            )));
        }
        let key = item_key(&input.key, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.key,
            &spec.table.key_schema,
        )?) {
            return Ok(CommandResult::Rejected(Json(
                PartitionDeleteOutcome::WrongPartition,
            )));
        }
        if transaction::key_locked(context, &key)? {
            return Ok(CommandResult::Rejected(Json(
                PartitionDeleteOutcome::TransactionConflict,
            )));
        }
        let old = command_item(context, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionDeleteOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionDeleteOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        context.sql(&statement(
            "DELETE FROM ddb_partition_items WHERE item_key = ?1",
            vec![SqlValue::Blob(key)],
        ))?;
        Ok(CommandResult::Success(Json(
            PartitionDeleteOutcome::Applied(old),
        )))
    }
}

/// Update one item under a verified data Cell epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionUpdateInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell epoch used for routing.
    pub epoch: u64,
    /// Complete primary key.
    pub key: Item,
    pub(crate) update: WireUpdate,
    pub(crate) condition: Option<WireCondition>,
}

impl PartitionUpdateInput {
    /// Bind validated ExtendDB expressions to a routed item update.
    pub fn from_expression(
        table_id: String,
        epoch: u64,
        key: Item,
        actions: &[UpdateAction],
        condition: Option<&Expr>,
        maps: &ExpressionMaps,
    ) -> Self {
        Self {
            table_id,
            epoch,
            key,
            update: WireUpdate::from_core(actions, maps),
            condition: condition.map(|expr| WireCondition::from_core(expr, maps)),
        }
    }
}

/// Result of a partition-local UpdateItem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionUpdateOutcome {
    /// The update committed with both item images.
    Applied { old: Option<Item>, new: Item },
    /// No partition contract is installed.
    NotInstalled,
    /// The table ID or data Cell epoch is stale.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// This split child is still importing its source range.
    NotReady,
    /// The key hashes outside this partition's range.
    WrongPartition,
    /// The key or resulting item violates the table contract.
    InvalidItem,
    /// The condition was false against the previous image.
    ConditionFailed(Option<Item>),
    /// The expression failed during evaluation.
    InvalidExpression(String),
    /// A prepared transaction owns this item key.
    TransactionConflict,
}

/// Apply one ExtendDB update expression in its owning data Cell.
pub struct PartitionUpdate;

impl Command for PartitionUpdate {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionUpdateInput>;
    type Output = Json<PartitionUpdateOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::NotInstalled,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::StaleRoute,
            )));
        }
        match command_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => {
                return Ok(CommandResult::Rejected(Json(
                    PartitionUpdateOutcome::Sealed,
                )));
            }
            AccessState::Importing => {
                return Ok(CommandResult::Rejected(Json(
                    PartitionUpdateOutcome::NotReady,
                )));
            }
        }
        if !valid_key(&input.key, &spec.table) {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::InvalidItem,
            )));
        }
        let key = item_key(&input.key, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.key,
            &spec.table.key_schema,
        )?) {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::WrongPartition,
            )));
        }
        if transaction::key_locked(context, &key)? {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::TransactionConflict,
            )));
        }
        let old = command_item(context, &key)?;
        if let Some(condition) = input.condition {
            let empty = Item::new();
            match condition.evaluate(old.as_ref().unwrap_or(&empty)) {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionUpdateOutcome::ConditionFailed(old),
                    )));
                }
                Err(message) => {
                    return Ok(CommandResult::Rejected(Json(
                        PartitionUpdateOutcome::InvalidExpression(message),
                    )));
                }
            }
        }
        let mut new = old.clone().unwrap_or_else(|| input.key.clone());
        if let Err(message) = input
            .update
            .apply(&mut new, &spec.table.attribute_definitions)
        {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::InvalidExpression(message),
            )));
        }
        if !valid_item(&new, &spec.table) || item_key(&new, &spec.table.key_schema)? != key {
            return Ok(CommandResult::Rejected(Json(
                PartitionUpdateOutcome::InvalidItem,
            )));
        }
        write_item(context, key, &new, &spec.table.key_schema)?;
        Ok(CommandResult::Success(Json(
            PartitionUpdateOutcome::Applied { old, new },
        )))
    }
}

/// Read one item from a routed partition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionGetInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell epoch used for routing.
    pub epoch: u64,
    /// Complete primary key.
    pub key: Item,
}

/// Result of one partition-local keyed read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionGetOutcome {
    /// The item may be absent.
    Found(Option<Item>),
    /// No partition contract is installed.
    NotInstalled,
    /// The table ID or data Cell epoch is stale.
    StaleRoute,
    /// The source has been sealed for a split.
    Sealed,
    /// This split child is still importing its source range.
    NotReady,
    /// The key hashes outside this partition's range.
    WrongPartition,
    /// The key does not match the table schema.
    InvalidKey,
    /// The key has an unresolved transaction intent.
    Conflict,
}

/// Read from the current data Cell owner.
pub struct PartitionGet;

impl Query for PartitionGet {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionGetInput>;
    type Output = Json<PartitionGetOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(Json(PartitionGetOutcome::NotInstalled));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(Json(PartitionGetOutcome::StaleRoute));
        }
        match query_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => return Ok(Json(PartitionGetOutcome::Sealed)),
            AccessState::Importing => return Ok(Json(PartitionGetOutcome::NotReady)),
        }
        if !valid_key(&input.key, &spec.table) {
            return Ok(Json(PartitionGetOutcome::InvalidKey));
        }
        let key = item_key(&input.key, &spec.table.key_schema)?;
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.key,
            &spec.table.key_schema,
        )?) {
            return Ok(Json(PartitionGetOutcome::WrongPartition));
        }
        // The coordinator may have committed before this participant applies.
        // Returning the old image here would violate strong read visibility.
        if transaction::read_key_locked(context, &key)? {
            return Ok(Json(PartitionGetOutcome::Conflict));
        }
        let item_rows = context.sql(&statement(
            "SELECT item FROM ddb_partition_items WHERE item_key = ?1",
            vec![SqlValue::Blob(key)],
        ))?;
        Ok(Json(PartitionGetOutcome::Found(decode_item(
            &item_rows[0],
        )?)))
    }
}

pub(super) fn decode_spec(rows: &SqlResultSet) -> Result<Option<PartitionSpec>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(bytes)] = row.as_slice() else {
        return Err(Error::Command("invalid partition contract row"));
    };
    Ok(Some(serde_json::from_slice(bytes)?))
}

fn decode_state(rows: &SqlResultSet) -> Result<Option<PartitionState>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(bytes)] = row.as_slice() else {
        return Err(Error::Command("invalid partition state row"));
    };
    Ok(Some(serde_json::from_slice(bytes)?))
}

fn command_state(context: &mut CommandContext<'_, '_>) -> Result<Option<PartitionState>> {
    decode_state(
        &context.sql(&statement(
            "SELECT state FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?[0],
    )
}

fn update_state(context: &mut CommandContext<'_, '_>, state: &PartitionState) -> Result<()> {
    context.sql(&statement(
        "UPDATE ddb_partition SET state = ?1 WHERE singleton = 1",
        vec![SqlValue::Blob(serde_json::to_vec(state)?)],
    ))?;
    Ok(())
}

fn query_state(context: &mut QueryContext<'_>) -> Result<Option<PartitionState>> {
    decode_state(
        &context.sql(&statement(
            "SELECT state FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?[0],
    )
}

#[derive(Clone, Copy)]
pub(super) enum AccessState {
    Serving,
    Importing,
    Sealed,
}

fn access_state(state: Option<PartitionState>) -> Result<AccessState> {
    match state {
        Some(PartitionState::Serving | PartitionState::Opened { .. }) => Ok(AccessState::Serving),
        Some(PartitionState::Importing { .. } | PartitionState::Activated { .. }) => {
            Ok(AccessState::Importing)
        }
        Some(PartitionState::Sealed(_)) => Ok(AccessState::Sealed),
        None => Err(Error::Command("installed partition has no state")),
    }
}

fn command_access(context: &mut CommandContext<'_, '_>) -> Result<AccessState> {
    access_state(command_state(context)?)
}

pub(super) fn query_access(context: &mut QueryContext<'_>) -> Result<AccessState> {
    access_state(query_state(context)?)
}

fn command_item(context: &mut CommandContext<'_, '_>, key: &[u8]) -> Result<Option<Item>> {
    let rows = context.sql(&statement(
        "SELECT item FROM ddb_partition_items WHERE item_key = ?1",
        vec![SqlValue::Blob(key.to_vec())],
    ))?;
    decode_item(&rows[0])
}

fn write_item(
    context: &mut CommandContext<'_, '_>,
    key: Vec<u8>,
    item: &Item,
    schema: &[KeySchemaElement],
) -> Result<()> {
    let (partition_key, sort_key) = index_key(item, schema)?;
    let (ttl_generation, ttl_epoch) = ttl::write_values(context, item)?;
    context.sql(&statement(
        "INSERT INTO ddb_partition_items \
         (item_key, partition_key, sort_key, item, ttl_generation, ttl_epoch) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(item_key) DO UPDATE SET \
         partition_key = excluded.partition_key, sort_key = excluded.sort_key, \
         item = excluded.item, ttl_generation = excluded.ttl_generation, \
         ttl_epoch = excluded.ttl_epoch",
        vec![
            SqlValue::Blob(key),
            SqlValue::Blob(partition_key),
            SqlValue::Blob(sort_key),
            SqlValue::Blob(serde_json::to_vec(item)?),
            ttl_generation,
            ttl_epoch,
        ],
    ))?;
    Ok(())
}
