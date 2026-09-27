//! Fenced transfer of projection versions, images, and deletion tombstones.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::Item;
use serde::{Deserialize, Serialize};

use super::{
    GlobalIndexApplyOutcome, GlobalIndexMutation, GlobalIndexPartitionSpec, MODULE,
    ProjectionVersion, apply, read_spec, target_for_tenant,
};
use crate::{
    Error, Json, Result, SqlBatch, SqlResultSet, SqlValue, item_storage::StoredValue,
    items::item_key, table::statement,
};

/// Exact source and replacement ranges recorded before an index split begins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexSplitPlan {
    pub source: GlobalIndexPartitionSpec,
    pub children: [GlobalIndexPartitionSpec; 2],
    pub expected_epoch: u64,
}

impl GlobalIndexSplitPlan {
    pub(crate) fn valid(&self) -> bool {
        let source = &self.source;
        let [left, right] = &self.children;
        let Some(next) = self.expected_epoch.checked_add(1) else {
            return false;
        };
        let Some(boundary) = left.upper else {
            return false;
        };
        source.epoch > 0
            && source.epoch <= self.expected_epoch
            && source
                .table
                .global_secondary_indexes
                .contains(&source.index)
            && left.table == source.table
            && right.table == source.table
            && left.index == source.index
            && right.index == source.index
            && left.epoch == next
            && right.epoch == next
            && left.lower == source.lower
            && right.upper == source.upper
            && right.lower == Some(boundary)
            && boundary > source.lower.unwrap_or([0; 16])
            && source.upper.is_none_or(|upper| boundary < upper)
            && left.partition_id != right.partition_id
            && left.partition_id != source.partition_id
            && right.partition_id != source.partition_id
    }
}

/// Complete transfer record, including entries no longer visible to index reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexEntry {
    pub key: Item,
    pub version: ProjectionVersion,
    pub item: Option<Item>,
}

/// Count and order-independent fingerprint of distinct index entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalIndexFingerprint {
    pub count: u64,
    pub digest: [u8; 32],
}

impl GlobalIndexFingerprint {
    /// Include one distinct entry from the sealed source, including tombstones.
    pub fn include(
        &mut self,
        entry: &GlobalIndexEntry,
        spec: &GlobalIndexPartitionSpec,
    ) -> Result<()> {
        let key = item_key(&entry.key, &spec.index.key_schema(&spec.table))?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"beyonddb.global-index.import.v1\0");
        hash.update(&(key.len() as u64).to_be_bytes());
        hash.update(&key);
        hash.update(&entry.version.bytes());
        hash.update(&serde_json::to_vec(&entry.item)?);
        for (current, added) in self.digest.iter_mut().zip(hash.finalize().as_bytes()) {
            *current ^= added;
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(Error::Command("index import count overflow"))?;
        Ok(())
    }
}

/// Durable range lifecycle; only Serving and Opened permit client traffic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GlobalIndexState {
    Serving,
    Importing {
        plan: GlobalIndexSplitPlan,
        summary: GlobalIndexFingerprint,
    },
    Activated {
        plan: GlobalIndexSplitPlan,
        summary: GlobalIndexFingerprint,
    },
    Opened {
        plan: GlobalIndexSplitPlan,
        summary: GlobalIndexFingerprint,
    },
    Sealed(GlobalIndexSplitPlan),
}

fn state(
    mut sql: impl FnMut(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<GlobalIndexState>> {
    let rows = sql(&statement(
        "SELECT state FROM ddb_global_index WHERE singleton = 1",
        vec![],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid global-index state")),
    }
}

pub(super) fn serving(sql: impl FnMut(&SqlBatch) -> Result<Vec<SqlResultSet>>) -> Result<bool> {
    Ok(matches!(
        state(sql)?,
        Some(GlobalIndexState::Serving | GlobalIndexState::Opened { .. })
    ))
}

fn update_state(context: &CommandContext<'_, '_>, state: &GlobalIndexState) -> Result<()> {
    context.sql(&statement(
        "UPDATE ddb_global_index SET state = ?1 WHERE singleton = 1",
        vec![SqlValue::Blob(serde_json::to_vec(state)?)],
    ))?;
    Ok(())
}

/// Read the durable lifecycle before resuming an interrupted index transfer.
pub struct ReadGlobalIndexState;
impl Query for ReadGlobalIndexState {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<GlobalIndexState>>;
    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        Ok(Json(state(|batch| context.sql(batch))?))
    }
}

/// Seal the plan's source or install an import-only child at the addressed Cell.
///
/// The trusted controller must durably record the plan before invoking this command.
pub struct PrepareGlobalIndexSplit;
impl Command for PrepareGlobalIndexSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !plan.valid() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let tenant = context.target().tenant();
        let source = target_for_tenant(tenant, &plan.source.index.id, &plan.source.partition_id)?;
        let existing = read_spec(|batch| context.sql(batch))?;
        let current = state(|batch| context.sql(batch))?;
        if source == *context.target() {
            if existing.as_ref() != Some(&plan.source) {
                return Ok(CommandResult::Rejected(Json(false)));
            }
            match current {
                Some(GlobalIndexState::Sealed(prior)) if prior == plan => {}
                Some(GlobalIndexState::Serving | GlobalIndexState::Opened { .. }) => {
                    update_state(context, &GlobalIndexState::Sealed(plan))?;
                }
                _ => return Ok(CommandResult::Rejected(Json(false))),
            }
        } else {
            let mut child = None;
            for spec in &plan.children {
                if target_for_tenant(tenant, &spec.index.id, &spec.partition_id)?
                    == *context.target()
                {
                    child = Some(spec);
                }
            }
            let Some(child) = child else {
                return Ok(CommandResult::Rejected(Json(false)));
            };
            if let Some(existing) = existing {
                let matches = matches!(current,
                    Some(GlobalIndexState::Importing { plan: prior, .. } | GlobalIndexState::Activated { plan: prior, .. } | GlobalIndexState::Opened { plan: prior, .. }) if prior == plan);
                if existing != *child || !matches {
                    return Ok(CommandResult::Rejected(Json(false)));
                }
            } else {
                let state = GlobalIndexState::Importing {
                    plan: plan.clone(),
                    summary: GlobalIndexFingerprint::default(),
                };
                context.sql(&statement(
                    "INSERT INTO ddb_global_index (singleton, spec, state) VALUES (1, ?1, ?2)",
                    vec![
                        SqlValue::Blob(serde_json::to_vec(child)?),
                        SqlValue::Blob(serde_json::to_vec(&state)?),
                    ],
                ))?;
            }
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// One sealed-source entry addressed to a replacement range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexImport {
    pub plan: GlobalIndexSplitPlan,
    pub entry: GlobalIndexEntry,
}

/// Import one immutable entry, retaining its original source version.
pub struct ImportGlobalIndexEntry;
impl Command for ImportGlobalIndexEntry {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexImport>;
    type Output = Json<GlobalIndexApplyOutcome>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        use GlobalIndexApplyOutcome as Outcome;
        let Some(GlobalIndexState::Importing { plan, mut summary }) =
            state(|batch| context.sql(batch))?
        else {
            return Ok(CommandResult::Rejected(Json(Outcome::StaleRoute)));
        };
        if plan != input.plan {
            return Ok(CommandResult::Rejected(Json(Outcome::StaleRoute)));
        }
        let spec = read_spec(|batch| context.sql(batch))?
            .ok_or(Error::Command("index import has no spec"))?;
        let outcome = apply(
            context,
            &spec,
            GlobalIndexMutation {
                index_id: spec.index.id.clone(),
                epoch: spec.epoch,
                key: input.entry.key.clone(),
                version: input.entry.version,
                item: input.entry.item.clone(),
            },
            true,
        )?;
        match outcome {
            Outcome::Applied => {
                summary.include(&input.entry, &spec)?;
                // The row and its fingerprint share the runtime's command savepoint.
                // Replays cannot double-count and rejection cannot leave a partial row.
                update_state(context, &GlobalIndexState::Importing { plan, summary })?;
                Ok(CommandResult::Success(Json(outcome)))
            }
            Outcome::Replay => Ok(CommandResult::Success(Json(outcome))),
            _ => Ok(CommandResult::Rejected(Json(outcome))),
        }
    }
}

/// Sealed export cursor in canonical full index/base-key order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexExport {
    pub plan: GlobalIndexSplitPlan,
    pub after: Option<Item>,
}

/// Bounded page of entries, retaining deleted entries and their versions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexExportPage {
    pub entries: Vec<GlobalIndexEntry>,
    pub next: Option<Item>,
}

/// Export only the exact immutable source identified by a split plan.
pub struct ExportGlobalIndexEntries;
impl Query for ExportGlobalIndexEntries {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexExport>;
    type Output = Json<Option<GlobalIndexExportPage>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if state(|batch| context.sql(batch))? != Some(GlobalIndexState::Sealed(input.plan.clone()))
        {
            return Ok(Json(None));
        }
        let spec = &input.plan.source;
        let schema = spec.index.key_schema(&spec.table);
        let after = match input.after {
            Some(key) if spec.valid_key(&key) && spec.contains(&key)? => item_key(&key, &schema)?,
            Some(_) => return Ok(Json(None)),
            None => Vec::new(),
        };
        let mut cursor = after;
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut last = None;
        loop {
            // Escaped full keys can exceed 16 KiB. Fetch one metadata row at a
            // time so the SQL result stays below its independent 1-MiB limit.
            let rows = context.sql(&statement("SELECT item_key, version, digest FROM ddb_global_index_items WHERE item_key > ?1 ORDER BY item_key LIMIT 1", vec![SqlValue::Blob(cursor)]))?;
            let Some(row) = rows[0].rows.first() else {
                break;
            };
            let [
                SqlValue::Blob(key),
                SqlValue::Blob(version),
                SqlValue::Blob(digest),
            ] = row.as_slice()
            else {
                return Err(Error::Command("invalid global-index export row"));
            };
            if entries.len() == 64 {
                return Ok(Json(Some(GlobalIndexExportPage {
                    entries,
                    next: last,
                })));
            }
            let version: [u8; 16] = version
                .as_slice()
                .try_into()
                .map_err(|_| Error::Command("invalid global-index export version"))?;
            let entry = GlobalIndexEntry {
                key: serde_json::from_slice(key)?,
                version: ProjectionVersion {
                    source_epoch: u64::from_be_bytes(
                        version[..8]
                            .try_into()
                            .map_err(|_| Error::Command("invalid source epoch"))?,
                    ),
                    sequence: u64::from_be_bytes(
                        version[8..]
                            .try_into()
                            .map_err(|_| Error::Command("invalid source sequence"))?,
                    ),
                },
                item: StoredValue::GlobalIndex(key).read(|batch| context.sql(batch))?,
            };
            if blake3::hash(&serde_json::to_vec(&entry.item)?).as_bytes() != digest.as_slice() {
                return Err(Error::Command("global-index export digest mismatch"));
            }
            let size = serde_json::to_vec(&entry)?.len();
            if !entries.is_empty() && bytes + size > 900_000 {
                return Ok(Json(Some(GlobalIndexExportPage {
                    entries,
                    next: last,
                })));
            }
            bytes += size;
            last = Some(entry.key.clone());
            cursor = key.clone();
            entries.push(entry);
        }
        Ok(Json(Some(GlobalIndexExportPage {
            entries,
            next: None,
        })))
    }
}

/// Expected complete child fingerprint computed from a sealed source export.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexImportComplete {
    pub plan: GlobalIndexSplitPlan,
    pub expected: GlobalIndexFingerprint,
}

/// Freeze a completely verified child before its directory route is published.
pub struct ActivateGlobalIndexImport;
impl Command for ActivateGlobalIndexImport {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexImportComplete>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        match state(|batch| context.sql(batch))? {
            Some(GlobalIndexState::Importing { plan, summary })
                if plan == input.plan && summary == input.expected =>
            {
                update_state(context, &GlobalIndexState::Activated { plan, summary })?;
            }
            Some(
                GlobalIndexState::Activated { plan, summary }
                | GlobalIndexState::Opened { plan, summary },
            ) if plan == input.plan && summary == input.expected => {}
            _ => return Ok(CommandResult::Rejected(Json(false))),
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Open a verified child after the trusted controller observes its published route.
pub struct OpenGlobalIndexImport;
impl Command for OpenGlobalIndexImport {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexImportComplete>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        match state(|batch| context.sql(batch))? {
            Some(GlobalIndexState::Activated { plan, summary })
                if plan == input.plan && summary == input.expected =>
            {
                update_state(context, &GlobalIndexState::Opened { plan, summary })?;
            }
            Some(GlobalIndexState::Opened { plan, summary })
                if plan == input.plan && summary == input.expected => {}
            _ => return Ok(CommandResult::Rejected(Json(false))),
        }
        Ok(CommandResult::Success(Json(true)))
    }
}
