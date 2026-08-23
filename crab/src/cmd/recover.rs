//! `crab recover` command namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use clap::{Args, Subcommand};
use crab_staging::StagingArea;
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer};
use crab_workflow::StageState;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::parser::XorbParser;
use fs4::fs_std::FileExt as LockFileExt;
use rusqlite::{Connection, OpenFlags};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
#[cfg(test)]
use crate::cmd::plan_apply::PlanApplyOperation;
use crate::cmd::plan_apply::PlanApplyResult;
use crate::cmd::stream_stage::{StreamStageProgress, stage_file_streaming_as};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::release::{ReleaseLargeFile, ReleaseManifest, ReleaseWorkflowOutput};

pub const RECOVER_PLAN_SCHEMA: &str = "recover.plan";
pub const RECOVER_APPLY_SCHEMA: &str = "recover.apply";
pub const RECOVER_STATUS_SCHEMA: &str = "recover.status";
pub const RECOVER_SCHEMA_VERSION: &str = "1.0";
pub type RecoverStatusPayload = PlanApplyResult;

#[derive(Debug, Clone, Subcommand)]
pub enum RecoverCmd {
    /// Plan repository repair actions.
    Plan(RecoverPlanArgs),
    /// Apply a repository repair plan.
    Apply(RecoverApplyArgs),
    /// Show a saved repository repair plan.
    Show(RecoverShowArgs),
    /// Inspect, verify, or restore immutable historical repository roots.
    History {
        #[command(subcommand)]
        command: crate::cmd::history_recovery::HistoryCmd,
    },
}

#[derive(Debug, Clone, Args)]
pub struct RecoverPlanArgs {
    /// Release manifest JSON to use as the expected inventory.
    #[arg(long, value_name = "PATH")]
    pub manifest: PathBuf,
    /// Local source file or directory to search for original bytes.
    #[arg(long, value_name = "PATH")]
    pub source: Vec<PathBuf>,
    /// Workflow cache root containing plain cached output bytes.
    #[arg(long = "cache-root", value_name = "PATH")]
    pub cache_root: Vec<PathBuf>,
    /// Local export or mounted path for a configured replica.
    #[arg(long = "replica-source", value_name = "PATH")]
    pub replica_source: Vec<PathBuf>,
    /// Import workspace root or import-journal.db to read staged identities from.
    #[arg(long = "import-journal", value_name = "PATH")]
    pub import_journal: Vec<PathBuf>,
    /// Workflow journal.db to read hashed output identities from.
    #[arg(long = "workflow-journal", value_name = "PATH")]
    pub workflow_journal: Vec<PathBuf>,
    /// Directory tree containing Crab pointer files to add to recovery inventory.
    #[arg(long = "pointer-root", value_name = "PATH")]
    pub pointer_root: Vec<PathBuf>,
    /// Newline or JSON shard-list inventory to include in the recovery plan.
    #[arg(long = "shard-list", value_name = "PATH")]
    pub shard_list: Vec<PathBuf>,
    /// Newline or JSON xorb-list inventory to include in the recovery plan.
    #[arg(long = "xorb-list", value_name = "PATH")]
    pub xorb_list: Vec<PathBuf>,
    /// JSONL output from `crab fsck --jsonl` to include missing shard/xorb inventory.
    #[arg(long = "fsck-jsonl", value_name = "PATH")]
    pub fsck_jsonl: Vec<PathBuf>,
    /// JSONL or JSON pack-list inventory to include in the recovery plan.
    #[arg(long = "pack-list", value_name = "PATH")]
    pub pack_list: Vec<PathBuf>,
    /// JSON or JSONL file-index entries to include in the recovery plan.
    #[arg(long = "file-index", value_name = "PATH")]
    pub file_index: Vec<PathBuf>,
    /// Write the recovery plan to this JSON file.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct RecoverApplyArgs {
    /// Recovery plan JSON produced by `crab recover plan`.
    #[arg(long, value_name = "PATH")]
    pub plan: PathBuf,
    /// Directory where verified recovered files are materialized.
    #[arg(long, value_name = "PATH")]
    pub restore_to: PathBuf,
    /// Rebuild file_index_db from durable shard objects and verify planned mappings.
    #[arg(long = "rebuild-file-index")]
    pub rebuild_file_index: bool,
    /// Restore verified shard objects from backup candidates to the configured Crab remote.
    #[arg(long = "restore-shards")]
    pub restore_shards: bool,
    /// Restore verified xorb objects from backup candidates to the configured Crab remote.
    #[arg(long = "restore-xorbs")]
    pub restore_xorbs: bool,
    /// Restore verified Git pack objects from backup candidates to the configured Crab remote.
    #[arg(long = "restore-packs")]
    pub restore_packs: bool,
    /// Stage verified file bytes and push manifest-selected refs to repair remote content.
    #[arg(long = "repair-remote")]
    pub repair_remote: bool,
    /// Remote name to use with remote repair actions.
    #[arg(long, value_name = "REMOTE")]
    pub remote: Option<String>,
    /// Explicit refspec to push with `--repair-remote`; repeat for multiple refs.
    #[arg(
        long = "repair-refspec",
        value_name = "REFSPEC",
        requires = "repair_remote"
    )]
    pub repair_refspec: Vec<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct RecoverShowArgs {
    /// Recovery plan JSON produced by `crab recover plan`.
    #[arg(long, value_name = "PATH")]
    pub plan: PathBuf,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RecoverPlanPayload {
    pub plan_id: String,
    pub manifest_path: String,
    pub sources: Vec<String>,
    pub repairable: u64,
    pub unrecoverable: u64,
    #[serde(default)]
    pub inventory_only: u64,
    pub items: Vec<RecoverPlanItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RecoverPlanItem {
    #[serde(default)]
    pub item_kind: RecoverItemKind,
    pub path: String,
    pub file_hash: String,
    pub size: u64,
    pub state: RecoverItemState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<RecoverCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    pub action: String,
}

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum RecoverItemKind {
    #[default]
    File,
    Shard,
    Xorb,
    Pack,
    FileIndex,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoverItemState {
    Repairable,
    Unrecoverable,
    InventoryOnly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoverCandidateKind {
    #[default]
    ExplicitPath,
    LocalCache,
    ConfiguredReplica,
    ImportJournal,
    WorkflowJournal,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RecoverCandidate {
    #[serde(default)]
    pub source_kind: RecoverCandidateKind,
    pub source_path: String,
    pub file_hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RecoverApplyPayload {
    pub plan_id: String,
    pub restore_root: String,
    pub restored: u64,
    pub already_present: u64,
    pub failed: u64,
    pub inventory_only: u64,
    pub metadata_repaired: u64,
    pub shards_repaired: u64,
    pub shard_bytes_repaired: u64,
    pub xorbs_repaired: u64,
    pub xorb_bytes_repaired: u64,
    pub packs_repaired: u64,
    pub pack_bytes_repaired: u64,
    pub remote_repaired: u64,
    pub remote_bytes_repaired: u64,
    pub remote_refspecs: Vec<String>,
    pub items: Vec<RecoverApplyItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RecoverApplyItem {
    pub path: String,
    pub state: RecoverApplyState,
    #[serde(default, skip_serializing_if = "is_false")]
    pub remote_repaired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoverApplyState {
    Restored,
    AlreadyPresent,
    Failed,
    Skipped,
    InventoryOnly,
    MetadataRepaired,
    ShardRepaired,
    XorbRepaired,
    PackRepaired,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl RecoverCmd {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json(), false)
    }

    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::Plan(_) | Self::Show(_) => RECOVER_PLAN_SCHEMA,
            Self::Apply(_) => RECOVER_APPLY_SCHEMA,
            Self::History { command } => command.schema_name(),
        }
    }

    #[cfg(test)]
    fn command_name(&self) -> &'static str {
        match self {
            Self::Plan(_) => "recover.plan",
            Self::Apply(_) => "recover.apply",
            Self::Show(_) => "recover.show",
            Self::History { command } => match command {
                crate::cmd::history_recovery::HistoryCmd::List(_) => "recover.history.list",
                crate::cmd::history_recovery::HistoryCmd::Prune(_) => "recover.history.prune",
                crate::cmd::history_recovery::HistoryCmd::Verify(_) => "recover.history.verify",
                crate::cmd::history_recovery::HistoryCmd::Restore(_) => "recover.history.restore",
            },
        }
    }

    #[cfg(test)]
    fn operation(&self) -> PlanApplyOperation {
        match self {
            Self::Plan(_) => PlanApplyOperation::Preview,
            Self::Apply(_) => PlanApplyOperation::Apply,
            Self::Show(_) => PlanApplyOperation::Inspect,
            Self::History { command } => match command {
                crate::cmd::history_recovery::HistoryCmd::List(_)
                | crate::cmd::history_recovery::HistoryCmd::Verify(_) => {
                    PlanApplyOperation::Inspect
                }
                crate::cmd::history_recovery::HistoryCmd::Prune(args) if args.apply => {
                    PlanApplyOperation::Apply
                }
                crate::cmd::history_recovery::HistoryCmd::Prune(_) => PlanApplyOperation::Preview,
                crate::cmd::history_recovery::HistoryCmd::Restore(args) if args.apply => {
                    PlanApplyOperation::Apply
                }
                crate::cmd::history_recovery::HistoryCmd::Restore(_) => PlanApplyOperation::Preview,
            },
        }
    }

    #[cfg(test)]
    fn idempotent_apply(&self) -> bool {
        matches!(
            self,
            Self::Apply(_)
                | Self::History {
                    command: crate::cmd::history_recovery::HistoryCmd::Prune(
                        crate::cmd::history_recovery::HistoryPruneArgs { apply: true, .. }
                    )
                }
        )
    }

    fn json(&self) -> bool {
        match self {
            Self::Plan(args) => args.json,
            Self::Apply(args) => args.json,
            Self::Show(args) => args.json,
            Self::History { command } => {
                return command.output_mode() != OutputMode::Text;
            }
        }
    }
}

pub async fn run(cmd: &RecoverCmd, cancel: &CancellationToken) -> Result<()> {
    match cmd {
        RecoverCmd::Plan(args) => run_plan(args, cmd.output_mode()),
        RecoverCmd::Apply(args) => run_apply(args, cmd.output_mode(), cancel).await,
        RecoverCmd::Show(args) => run_show(args, cmd.output_mode()),
        RecoverCmd::History { .. } => Err(CrabError::Internal(
            "historical recovery requires a configured remote store".to_owned(),
        )),
    }
}

fn run_plan(args: &RecoverPlanArgs, mode: OutputMode) -> Result<()> {
    let sources = RecoverySourceSpec {
        explicit_paths: args.source.clone(),
        cache_roots: args.cache_root.clone(),
        replica_sources: args.replica_source.clone(),
        import_journals: args.import_journal.clone(),
        workflow_journals: args.workflow_journal.clone(),
        pointer_roots: args.pointer_root.clone(),
        shard_lists: args.shard_list.clone(),
        xorb_lists: args.xorb_list.clone(),
        fsck_jsonl: args.fsck_jsonl.clone(),
        pack_lists: args.pack_list.clone(),
        file_indexes: args.file_index.clone(),
    };
    let plan = build_plan_with_sources(&args.manifest, &sources)?;
    if let Some(output) = &args.output {
        write_plan(output, &plan)?;
    }
    emit_plan(&plan, mode);
    Ok(())
}

fn run_show(args: &RecoverShowArgs, mode: OutputMode) -> Result<()> {
    let plan = read_plan(&args.plan)?;
    emit_plan(&plan, mode);
    Ok(())
}

async fn run_apply(
    args: &RecoverApplyArgs,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<()> {
    let plan = read_plan(&args.plan)?;
    validate_apply_remote_args(args)?;
    let shard_repairs = if args.restore_shards {
        Some(restore_shards_from_plan(&plan, args, cancel).await?)
    } else {
        None
    };
    let xorb_repairs = if args.restore_xorbs {
        Some(restore_xorbs_from_plan(&plan, args, cancel).await?)
    } else {
        None
    };
    let pack_repairs = if args.restore_packs {
        Some(restore_packs_from_plan(&plan, args, cancel).await?)
    } else {
        None
    };
    let metadata_repairs = if args.rebuild_file_index {
        file_index_metadata_repairs(&plan).await?
    } else {
        vec![None; plan.items.len()]
    };
    let remote_repair = if args.repair_remote {
        Some(repair_remote_from_plan(&plan, args, cancel).await?)
    } else {
        None
    };
    let payload = apply_plan_with_repairs(
        &plan,
        &args.restore_to,
        &metadata_repairs,
        shard_repairs.as_ref(),
        xorb_repairs.as_ref(),
        pack_repairs.as_ref(),
        remote_repair.as_ref(),
    )?;
    if let Err(err) = record_recover_apply_audit(&payload) {
        warn!(%err, "failed to append recovery apply audit event");
    }
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RECOVER_APPLY_SCHEMA, RECOVER_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            println!(
                "recover apply {}: restored={} already_present={} failed={} inventory_only={} metadata_repaired={} shards_repaired={} xorbs_repaired={} packs_repaired={} remote_repaired={}",
                payload.plan_id,
                payload.restored,
                payload.already_present,
                payload.failed,
                payload.inventory_only,
                payload.metadata_repaired,
                payload.shards_repaired,
                payload.xorbs_repaired,
                payload.packs_repaired,
                payload.remote_repaired
            );
        }
    }
    Ok(())
}

fn emit_plan(plan: &RecoverPlanPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RECOVER_PLAN_SCHEMA, RECOVER_SCHEMA_VERSION, plan);
        }
        OutputMode::Text => {
            println!(
                "recover plan {}: repairable={} unrecoverable={} inventory_only={}",
                plan.plan_id, plan.repairable, plan.unrecoverable, plan.inventory_only
            );
        }
    }
}

#[cfg(test)]
fn build_plan(manifest_path: &Path, sources: &[PathBuf]) -> Result<RecoverPlanPayload> {
    build_plan_with_sources(
        manifest_path,
        &RecoverySourceSpec {
            explicit_paths: sources.to_vec(),
            ..RecoverySourceSpec::default()
        },
    )
}

#[derive(Debug, Clone, Default)]
struct RecoverySourceSpec {
    explicit_paths: Vec<PathBuf>,
    cache_roots: Vec<PathBuf>,
    replica_sources: Vec<PathBuf>,
    import_journals: Vec<PathBuf>,
    workflow_journals: Vec<PathBuf>,
    pointer_roots: Vec<PathBuf>,
    shard_lists: Vec<PathBuf>,
    xorb_lists: Vec<PathBuf>,
    fsck_jsonl: Vec<PathBuf>,
    pack_lists: Vec<PathBuf>,
    file_indexes: Vec<PathBuf>,
}

impl RecoverySourceSpec {
    fn labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        labels.extend(
            self.explicit_paths
                .iter()
                .map(|path| format!("source:{}", path.display())),
        );
        labels.extend(
            self.cache_roots
                .iter()
                .map(|path| format!("cache:{}", path.display())),
        );
        labels.extend(
            self.replica_sources
                .iter()
                .map(|path| format!("replica:{}", path.display())),
        );
        labels.extend(
            self.import_journals
                .iter()
                .map(|path| format!("import-journal:{}", path.display())),
        );
        labels.extend(
            self.workflow_journals
                .iter()
                .map(|path| format!("workflow-journal:{}", path.display())),
        );
        labels.extend(
            self.pointer_roots
                .iter()
                .map(|path| format!("pointer-root:{}", path.display())),
        );
        labels.extend(
            self.shard_lists
                .iter()
                .map(|path| format!("shard-list:{}", path.display())),
        );
        labels.extend(
            self.xorb_lists
                .iter()
                .map(|path| format!("xorb-list:{}", path.display())),
        );
        labels.extend(
            self.fsck_jsonl
                .iter()
                .map(|path| format!("fsck-jsonl:{}", path.display())),
        );
        labels.extend(
            self.pack_lists
                .iter()
                .map(|path| format!("pack-list:{}", path.display())),
        );
        labels.extend(
            self.file_indexes
                .iter()
                .map(|path| format!("file-index:{}", path.display())),
        );
        labels
    }
}

fn build_plan_with_sources(
    manifest_path: &Path,
    sources: &RecoverySourceSpec,
) -> Result<RecoverPlanPayload> {
    let manifest = read_manifest(manifest_path)?;
    let mut items = Vec::new();
    for inventory_item in recovery_inventory_items(&manifest, sources)? {
        let candidate = find_recovery_candidate(sources, &inventory_item)?;
        let state = recovery_item_state(inventory_item.item_kind, candidate.as_ref());
        let size = if matches!(
            inventory_item.item_kind,
            RecoverItemKind::Shard | RecoverItemKind::Xorb
        ) {
            candidate
                .as_ref()
                .map_or(inventory_item.size, |candidate| candidate.size)
        } else {
            inventory_item.size
        };
        items.push(RecoverPlanItem {
            item_kind: inventory_item.item_kind,
            path: inventory_item.path,
            file_hash: inventory_item.file_hash,
            size,
            state,
            candidate,
            metadata: inventory_item.metadata,
            action: match state {
                RecoverItemState::Repairable => {
                    repairable_action(inventory_item.item_kind).to_owned()
                }
                RecoverItemState::Unrecoverable => "manual_source_required".to_owned(),
                RecoverItemState::InventoryOnly => {
                    inventory_only_action(inventory_item.item_kind).to_owned()
                }
            },
        });
    }

    let repairable = items
        .iter()
        .filter(|item| item.state == RecoverItemState::Repairable)
        .count() as u64;
    let unrecoverable = items
        .iter()
        .filter(|item| item.state == RecoverItemState::Unrecoverable)
        .count() as u64;
    let inventory_only = items
        .iter()
        .filter(|item| item.state == RecoverItemState::InventoryOnly)
        .count() as u64;
    let mut plan = RecoverPlanPayload {
        plan_id: String::new(),
        manifest_path: manifest_path.display().to_string(),
        sources: sources.labels(),
        repairable,
        unrecoverable,
        inventory_only,
        items,
    };
    plan.plan_id = plan_digest(&plan)?;
    Ok(plan)
}

fn find_recovery_candidate(
    sources: &RecoverySourceSpec,
    item: &RecoveryInventoryItem,
) -> Result<Option<RecoverCandidate>> {
    match item.item_kind {
        RecoverItemKind::File => find_candidate(sources, item),
        RecoverItemKind::Shard => find_shard_candidate(sources, item),
        RecoverItemKind::Xorb => find_xorb_candidate(sources, item),
        RecoverItemKind::Pack => find_pack_candidate(sources, item),
        RecoverItemKind::FileIndex => Ok(None),
    }
}

fn recovery_item_state(
    kind: RecoverItemKind,
    candidate: Option<&RecoverCandidate>,
) -> RecoverItemState {
    match kind {
        RecoverItemKind::File => {
            if candidate.is_some() {
                RecoverItemState::Repairable
            } else {
                RecoverItemState::Unrecoverable
            }
        }
        RecoverItemKind::Shard => {
            if candidate.is_some() {
                RecoverItemState::Repairable
            } else {
                RecoverItemState::InventoryOnly
            }
        }
        RecoverItemKind::Xorb => {
            if candidate.is_some() {
                RecoverItemState::Repairable
            } else {
                RecoverItemState::Unrecoverable
            }
        }
        RecoverItemKind::Pack => {
            if candidate.is_some() {
                RecoverItemState::Repairable
            } else {
                RecoverItemState::InventoryOnly
            }
        }
        RecoverItemKind::FileIndex => RecoverItemState::InventoryOnly,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RecoveryInventoryItem {
    item_kind: RecoverItemKind,
    path: String,
    file_hash: String,
    size: u64,
    metadata: Option<BTreeMap<String, String>>,
}

fn recovery_inventory_items(
    manifest: &ReleaseManifest,
    sources: &RecoverySourceSpec,
) -> Result<Vec<RecoveryInventoryItem>> {
    let mut seen = BTreeSet::new();
    for large_file in &manifest.crab.large_files {
        seen.insert(inventory_item_from_large_file(large_file));
    }
    for stage in &manifest.workflow.stages {
        for out in &stage.outs {
            seen.insert(inventory_item_from_workflow_out(out));
        }
    }
    for pointer_root in &sources.pointer_roots {
        for item in inventory_items_from_pointer_root(pointer_root)? {
            seen.insert(item);
        }
    }
    for import_journal in &sources.import_journals {
        for item in inventory_items_from_import_journal(import_journal)? {
            seen.insert(item);
        }
    }
    for workflow_journal in &sources.workflow_journals {
        for item in inventory_items_from_workflow_journal(workflow_journal)? {
            seen.insert(item);
        }
    }
    for shard_list in &sources.shard_lists {
        for item in inventory_items_from_shard_list(shard_list)? {
            seen.insert(item);
        }
    }
    for xorb_list in &sources.xorb_lists {
        for item in inventory_items_from_xorb_list(xorb_list)? {
            seen.insert(item);
        }
    }
    for fsck_jsonl in &sources.fsck_jsonl {
        for item in inventory_items_from_fsck_jsonl(fsck_jsonl)? {
            seen.insert(item);
        }
    }
    for pack_list in &sources.pack_lists {
        for item in inventory_items_from_pack_list(pack_list)? {
            seen.insert(item);
        }
    }
    for file_index in &sources.file_indexes {
        for item in inventory_items_from_file_index(file_index)? {
            seen.insert(item);
        }
    }
    Ok(seen.into_iter().collect())
}

fn inventory_item_from_large_file(file: &ReleaseLargeFile) -> RecoveryInventoryItem {
    RecoveryInventoryItem {
        item_kind: RecoverItemKind::File,
        path: file.path.clone(),
        file_hash: file.file_hash.clone(),
        size: file.size,
        metadata: None,
    }
}

fn inventory_item_from_workflow_out(out: &ReleaseWorkflowOutput) -> RecoveryInventoryItem {
    RecoveryInventoryItem {
        item_kind: RecoverItemKind::File,
        path: out.path.clone(),
        file_hash: out.file_hash.clone(),
        size: out.size,
        metadata: None,
    }
}

fn inventory_items_from_pointer_root(root: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let mut items = Vec::new();
    collect_pointer_inventory(root, root, &mut items)?;
    Ok(items)
}

fn collect_pointer_inventory(
    root: &Path,
    current: &Path,
    items: &mut Vec<RecoveryInventoryItem>,
) -> Result<()> {
    for entry in std::fs::read_dir(current).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if file_type.is_dir() {
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".git" | ".crab")
            ) {
                continue;
            }
            collect_pointer_inventory(root, &path, items)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(CrabError::Io)?;
        if metadata.len() > MAX_POINTER_SIZE as u64 {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(CrabError::Io)?;
        let Ok(pointer) = Pointer::parse(&bytes) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path);
        items.push(RecoveryInventoryItem {
            item_kind: RecoverItemKind::File,
            path: rel.to_string_lossy().to_string(),
            file_hash: format_b3_digest(pointer.file_hash),
            size: pointer.size,
            metadata: Some(metadata_map([
                ("source", "pointer_metadata".to_owned()),
                ("pointer_path", rel.to_string_lossy().to_string()),
            ])),
        });
    }
    Ok(())
}

fn inventory_items_from_import_journal(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let db_path = import_journal_db_path(path);
    let conn = open_readonly_sqlite(&db_path, "import journal")?;
    let mut stmt = conn
        .prepare(
            "SELECT e.relative_path,
                    COALESCE(r.size, e.size) AS recovery_size,
                    e.file_hash
               FROM entries e
               LEFT JOIN lfs_resolutions r
                 ON r.relative_path = e.relative_path
                AND r.version_id = e.version_id
              WHERE e.state = 1
                AND e.is_delete_marker = 0
                AND e.file_hash IS NOT NULL
              ORDER BY e.relative_path ASC, e.version_id ASC",
        )
        .map_err(|err| sqlite_config_error(&db_path, "read staged import entries", err))?;
    let rows = stmt
        .query_map([], |row| {
            let path: String = row.get(0)?;
            let size_i64: i64 = row.get(1)?;
            let file_hash: Vec<u8> = row.get(2)?;
            Ok((path, size_i64, file_hash))
        })
        .map_err(|err| sqlite_config_error(&db_path, "query staged import entries", err))?;

    let mut items = Vec::new();
    for row in rows {
        let (path, size_i64, file_hash) =
            row.map_err(|err| sqlite_config_error(&db_path, "decode staged import entry", err))?;
        let size = u64::try_from(size_i64).map_err(|err| CrabError::Configuration {
            key: db_path.display().to_string(),
            origin: format!("staged import entry {path:?} has invalid size: {err}"),
        })?;
        items.push(RecoveryInventoryItem {
            item_kind: RecoverItemKind::File,
            path,
            file_hash: format_b3_digest(decode_32_blob(&file_hash, &db_path)?),
            size,
            metadata: Some(metadata_map([("source", "import_journal".to_owned())])),
        });
    }
    Ok(items)
}

fn inventory_items_from_workflow_journal(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let db_path = workflow_journal_db_path(path);
    let conn = open_readonly_sqlite(&db_path, "workflow journal")?;
    let mut stmt = conn
        .prepare(
            "SELECT payload_json
               FROM stage_runs
              WHERE state = ?1
              ORDER BY stage_name ASC, attempt ASC",
        )
        .map_err(|err| sqlite_config_error(&db_path, "read hashed workflow rows", err))?;
    let rows = stmt
        .query_map([i64::from(StageState::Hashed.sql_tag())], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| sqlite_config_error(&db_path, "query hashed workflow rows", err))?;

    let mut items = Vec::new();
    for row in rows {
        let payload =
            row.map_err(|err| sqlite_config_error(&db_path, "decode workflow row", err))?;
        let payload: WorkflowHashedPayload =
            serde_json::from_str(&payload).map_err(|err| CrabError::Configuration {
                key: db_path.display().to_string(),
                origin: format!("invalid workflow hashed payload: {err}"),
            })?;
        for out in payload.outs {
            items.push(RecoveryInventoryItem {
                item_kind: RecoverItemKind::File,
                path: out.path,
                file_hash: out.hash,
                size: out.size,
                metadata: Some(metadata_map([("source", "workflow_journal".to_owned())])),
            });
        }
    }
    Ok(items)
}

#[derive(Debug, Deserialize)]
struct WorkflowHashedPayload {
    outs: Vec<WorkflowHashedOut>,
}

#[derive(Debug, Deserialize)]
struct WorkflowHashedOut {
    path: String,
    hash: String,
    size: u64,
}

fn inventory_items_from_shard_list(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    let hashes = parse_shard_inventory(path, &bytes)?;
    let mut items = Vec::new();
    for hash in hashes {
        items.push(shard_inventory_item_from_hash(&hash, "shard_list", path)?);
    }
    Ok(items)
}

fn inventory_items_from_xorb_list(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    let hashes = parse_hash_inventory(path, &bytes, "xorb-list")?;
    let mut items = Vec::new();
    for hash in hashes {
        items.push(xorb_inventory_item_from_hash(&hash, "xorb_list", path)?);
    }
    Ok(items)
}

fn inventory_items_from_fsck_jsonl(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let content = std::fs::read_to_string(path).map_err(CrabError::Io)?;
    let mut items = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: FsckJsonlEvent =
            serde_json::from_str(line).map_err(|err| CrabError::Configuration {
                key: format!("{}:{}", path.display(), line_index + 1),
                origin: format!("invalid fsck JSONL event: {err}"),
            })?;
        if event.schema.as_deref() != Some("fsck.event")
            || event.event_type.as_deref() != Some("warning")
        {
            continue;
        }
        let Some(data) = event.data else {
            continue;
        };
        let Some(code) = data.code.as_deref() else {
            continue;
        };
        match code {
            "fsck-missing-xorb" => {
                let identity = fsck_warning_path(path, line_index, code, &data)?;
                items.push(xorb_inventory_item_from_hash(identity, "fsck_jsonl", path)?);
            }
            "fsck-shard-list-divergence" => {
                let identity = fsck_warning_path(path, line_index, code, &data)?;
                items.push(shard_inventory_item_from_hash(
                    identity,
                    "fsck_jsonl",
                    path,
                )?);
            }
            _ => {}
        }
    }
    Ok(items)
}

#[derive(Debug, Deserialize)]
struct FsckJsonlEvent {
    schema: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    data: Option<FsckJsonlWarning>,
}

#[derive(Debug, Deserialize)]
struct FsckJsonlWarning {
    code: Option<String>,
    path: Option<String>,
}

fn fsck_warning_path<'a>(
    path: &Path,
    line_index: usize,
    code: &str,
    data: &'a FsckJsonlWarning,
) -> Result<&'a str> {
    data.path
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: format!("{}:{}", path.display(), line_index + 1),
            origin: format!("{code} warning is missing path"),
        })
}

fn shard_inventory_item_from_hash(
    hash: &str,
    source: &str,
    source_path: &Path,
) -> Result<RecoveryInventoryItem> {
    let normalized = normalize_verified_b3_digest(hash)?;
    let item_id = normalized.trim_start_matches("b3:").to_owned();
    Ok(RecoveryInventoryItem {
        item_kind: RecoverItemKind::Shard,
        path: format!("shard:{item_id}"),
        file_hash: normalized.clone(),
        size: 0,
        metadata: Some(metadata_map([
            ("source", source.to_owned()),
            ("source_path", source_path.display().to_string()),
            ("shard_hash", normalized),
        ])),
    })
}

fn xorb_inventory_item_from_hash(
    hash: &str,
    source: &str,
    source_path: &Path,
) -> Result<RecoveryInventoryItem> {
    let normalized = normalize_verified_b3_digest(hash)?;
    let item_id = normalized.trim_start_matches("b3:").to_owned();
    Ok(RecoveryInventoryItem {
        item_kind: RecoverItemKind::Xorb,
        path: format!("xorb:{item_id}"),
        file_hash: normalized.clone(),
        size: 0,
        metadata: Some(metadata_map([
            ("source", source.to_owned()),
            ("source_path", source_path.display().to_string()),
            ("xorb_hash", normalized),
        ])),
    })
}

fn parse_shard_inventory(path: &Path, bytes: &[u8]) -> Result<Vec<String>> {
    if let Ok(list) = serde_json::from_slice::<crab_metadata::manifests::ShardList>(bytes) {
        for hash in &list.entries {
            parse_b3_digest(hash)?;
        }
        return Ok(list.entries);
    }
    parse_hash_inventory(path, bytes, "shard-list")
}

fn parse_hash_inventory(path: &Path, bytes: &[u8], label: &str) -> Result<Vec<String>> {
    if let Ok(list) = serde_json::from_slice::<Vec<String>>(bytes) {
        for hash in &list {
            parse_b3_digest(hash)?;
        }
        return Ok(list);
    }
    crate::metadata::manifest::parse_shard_list(bytes).map_err(|err| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("invalid {label} inventory: {err}"),
    })
}

fn inventory_items_from_pack_list(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    let entries = parse_pack_inventory(path, &bytes)?;
    let mut items = Vec::new();
    for entry in entries {
        let normalized = normalize_verified_b3_digest(&entry.pack_id)?;
        let item_id = normalized.trim_start_matches("b3:").to_owned();
        let mut metadata = metadata_map([
            ("source", "pack_list".to_owned()),
            ("source_path", path.display().to_string()),
            ("pack_id", normalized.clone()),
        ]);
        if let Some(content_hash) = entry.content_hash {
            metadata.insert(
                "content_hash".to_owned(),
                normalize_verified_b3_digest(&content_hash)?,
            );
        }
        if let Some(object_count) = entry.object_count {
            metadata.insert("object_count".to_owned(), object_count.to_string());
        }
        if !entry.ref_tips.is_empty() {
            metadata.insert("ref_tips".to_owned(), entry.ref_tips.join(","));
        }
        items.push(RecoveryInventoryItem {
            item_kind: RecoverItemKind::Pack,
            path: format!("pack:{item_id}"),
            file_hash: normalized,
            size: entry.size,
            metadata: Some(metadata),
        });
    }
    Ok(items)
}

#[derive(Debug)]
struct PackInventoryEntry {
    pack_id: String,
    size: u64,
    content_hash: Option<String>,
    object_count: Option<u64>,
    ref_tips: Vec<String>,
}

fn parse_pack_inventory(path: &Path, bytes: &[u8]) -> Result<Vec<PackInventoryEntry>> {
    if let Ok(list) = serde_json::from_slice::<crab_metadata::manifests::PackList>(bytes) {
        return Ok(list
            .entries
            .into_iter()
            .map(|entry| PackInventoryEntry {
                pack_id: entry.pack_id,
                size: entry.size,
                content_hash: None,
                object_count: None,
                ref_tips: entry.ref_tips.unwrap_or_default(),
            })
            .collect());
    }
    let entries = crate::metadata::manifest::parse_pack_list(bytes).map_err(|err| {
        CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("invalid pack-list inventory: {err}"),
        }
    })?;
    Ok(entries
        .into_iter()
        .map(|entry| PackInventoryEntry {
            pack_id: entry.pack_id,
            size: entry.size,
            content_hash: Some(entry.content_hash),
            object_count: Some(entry.object_count),
            ref_tips: entry.ref_tips,
        })
        .collect())
}

fn inventory_items_from_file_index(path: &Path) -> Result<Vec<RecoveryInventoryItem>> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    let entries = parse_file_index_inventory(path, &bytes)?;
    let mut items = Vec::new();
    for entry in entries {
        let file_hash = normalize_verified_b3_digest(&entry.file_hash)?;
        let shard_hash = normalize_verified_b3_digest(&entry.shard_hash)?;
        let item_path = entry
            .path
            .unwrap_or_else(|| format!("file-index:{}", file_hash.trim_start_matches("b3:")));
        items.push(RecoveryInventoryItem {
            item_kind: RecoverItemKind::FileIndex,
            path: item_path,
            file_hash,
            size: entry.size.unwrap_or(0),
            metadata: Some(metadata_map([
                ("source", "file_index".to_owned()),
                ("source_path", path.display().to_string()),
                ("shard_hash", shard_hash),
            ])),
        });
    }
    Ok(items)
}

#[derive(Debug, Deserialize)]
struct FileIndexInventoryEntry {
    file_hash: String,
    shard_hash: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

fn parse_file_index_inventory(path: &Path, bytes: &[u8]) -> Result<Vec<FileIndexInventoryEntry>> {
    if let Ok(entries) = serde_json::from_slice::<Vec<FileIndexInventoryEntry>>(bytes) {
        return Ok(entries);
    }
    if let Ok(map) = serde_json::from_slice::<BTreeMap<String, String>>(bytes) {
        return Ok(map
            .into_iter()
            .map(|(file_hash, shard_hash)| FileIndexInventoryEntry {
                file_hash,
                shard_hash,
                path: None,
                size: None,
            })
            .collect());
    }
    parse_file_index_jsonl(path, bytes)
}

fn parse_file_index_jsonl(path: &Path, bytes: &[u8]) -> Result<Vec<FileIndexInventoryEntry>> {
    let text = std::str::from_utf8(bytes).map_err(|err| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("invalid file-index inventory UTF-8: {err}"),
    })?;
    let mut entries = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: FileIndexInventoryEntry =
            serde_json::from_str(line).map_err(|err| CrabError::Configuration {
                key: path.display().to_string(),
                origin: format!("invalid file-index inventory line {idx}: {err}"),
            })?;
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "file-index inventory contained no entries".to_owned(),
        });
    }
    Ok(entries)
}

fn metadata_map<const N: usize>(pairs: [(&str, String); N]) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn normalize_verified_b3_digest(value: &str) -> Result<String> {
    parse_b3_digest(value)?;
    let hex = value.strip_prefix("b3:").unwrap_or(value);
    Ok(format!("b3:{hex}"))
}

fn import_journal_db_path(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    path.join(".crab").join("import-journal.db")
}

fn workflow_journal_db_path(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    path.join("journal.db")
}

fn open_readonly_sqlite(path: &Path, label: &str) -> Result<Connection> {
    if !path.is_file() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("{label} does not exist"),
        });
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|err| sqlite_config_error(path, &format!("open {label}"), err))
}

fn sqlite_config_error(path: &Path, action: &str, err: rusqlite::Error) -> CrabError {
    CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("{action}: {err}"),
    }
}

fn decode_32_blob(bytes: &[u8], path: &Path) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).map_err(|_| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("expected 32-byte hash, got {} bytes", bytes.len()),
    })
}

fn find_candidate(
    sources: &RecoverySourceSpec,
    item: &RecoveryInventoryItem,
) -> Result<Option<RecoverCandidate>> {
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    for candidate in candidate_paths(&sources.explicit_paths, &item.path) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::ExplicitPath,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in cache_candidate_paths(&sources.cache_roots, &item.file_hash)? {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::LocalCache,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in candidate_paths(&sources.replica_sources, &item.path) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::ConfiguredReplica,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in candidate_paths(&sources.import_journals, &item.path) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::ImportJournal,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in workflow_journal_candidate_paths(&sources.workflow_journals, &item.path) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::WorkflowJournal,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn verify_candidate_path(
    source_kind: RecoverCandidateKind,
    candidate: &Path,
    expected_hash: [u8; 32],
    expected_size: u64,
) -> Result<Option<RecoverCandidate>> {
    if !candidate.is_file() {
        return Ok(None);
    }
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return Ok(None);
    };
    if metadata.len() != expected_size {
        return Ok(None);
    }
    let actual_hash = hash_file_blake3(candidate).map_err(CrabError::Io)?;
    if actual_hash == expected_hash {
        return Ok(Some(RecoverCandidate {
            source_kind,
            source_path: candidate.display().to_string(),
            file_hash: format_b3_digest(actual_hash),
            size: expected_size,
        }));
    }
    Ok(None)
}

fn find_shard_candidate(
    sources: &RecoverySourceSpec,
    item: &RecoveryInventoryItem,
) -> Result<Option<RecoverCandidate>> {
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    for candidate in shard_candidate_paths(&sources.explicit_paths, item) {
        if let Some(candidate) = verify_candidate_path_by_hash(
            RecoverCandidateKind::ExplicitPath,
            &candidate,
            expected_hash,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in shard_candidate_paths(&sources.replica_sources, item) {
        if let Some(candidate) = verify_candidate_path_by_hash(
            RecoverCandidateKind::ConfiguredReplica,
            &candidate,
            expected_hash,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn find_xorb_candidate(
    sources: &RecoverySourceSpec,
    item: &RecoveryInventoryItem,
) -> Result<Option<RecoverCandidate>> {
    let expected_hash = parse_merkle_digest(&item.file_hash)?;
    for candidate in xorb_candidate_paths(&sources.explicit_paths, item) {
        if let Some(candidate) = verify_xorb_candidate_path(
            RecoverCandidateKind::ExplicitPath,
            &candidate,
            expected_hash,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in xorb_cache_candidate_paths(&sources.cache_roots, item)? {
        if let Some(candidate) =
            verify_xorb_candidate_path(RecoverCandidateKind::LocalCache, &candidate, expected_hash)?
        {
            return Ok(Some(candidate));
        }
    }
    for candidate in xorb_candidate_paths(&sources.replica_sources, item) {
        if let Some(candidate) = verify_xorb_candidate_path(
            RecoverCandidateKind::ConfiguredReplica,
            &candidate,
            expected_hash,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn find_pack_candidate(
    sources: &RecoverySourceSpec,
    item: &RecoveryInventoryItem,
) -> Result<Option<RecoverCandidate>> {
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    for candidate in pack_candidate_paths(&sources.explicit_paths, item) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::ExplicitPath,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    for candidate in pack_candidate_paths(&sources.replica_sources, item) {
        if let Some(candidate) = verify_candidate_path(
            RecoverCandidateKind::ConfiguredReplica,
            &candidate,
            expected_hash,
            item.size,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn verify_xorb_candidate_path(
    source_kind: RecoverCandidateKind,
    candidate: &Path,
    expected_hash: MerkleHash,
) -> Result<Option<RecoverCandidate>> {
    if !candidate.is_file() {
        return Ok(None);
    }
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return Ok(None);
    };
    let bytes = std::fs::read(candidate).map_err(CrabError::Io)?;
    let parser = match XorbParser::parse(Bytes::from(bytes)) {
        Ok(parser) => parser,
        Err(_) => return Ok(None),
    };
    if parser.hash() != expected_hash {
        return Ok(None);
    }
    if parser.verify_all_chunks().is_err() {
        return Ok(None);
    }
    Ok(Some(RecoverCandidate {
        source_kind,
        source_path: candidate.display().to_string(),
        file_hash: format!("b3:{}", parser.hash().hex()),
        size: metadata.len(),
    }))
}

fn verify_candidate_path_by_hash(
    source_kind: RecoverCandidateKind,
    candidate: &Path,
    expected_hash: [u8; 32],
) -> Result<Option<RecoverCandidate>> {
    if !candidate.is_file() {
        return Ok(None);
    }
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return Ok(None);
    };
    let actual_hash = hash_file_blake3(candidate).map_err(CrabError::Io)?;
    if actual_hash == expected_hash {
        return Ok(Some(RecoverCandidate {
            source_kind,
            source_path: candidate.display().to_string(),
            file_hash: format_b3_digest(actual_hash),
            size: metadata.len(),
        }));
    }
    Ok(None)
}

fn pack_candidate_paths(sources: &[PathBuf], item: &RecoveryInventoryItem) -> Vec<PathBuf> {
    let hex = item
        .file_hash
        .strip_prefix("b3:")
        .unwrap_or(&item.file_hash)
        .to_owned();
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for source in sources {
        if source.is_dir() {
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join("packs").join(format!("pack-{hex}.pack")),
            );
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join(format!("pack-{hex}.pack")),
            );
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join(format!("{hex}.pack")),
            );
            push_candidate(&mut seen, &mut candidates, source.join(&hex));
            push_candidate(&mut seen, &mut candidates, source.join(&item.path));
        } else {
            push_candidate(&mut seen, &mut candidates, source.clone());
        }
    }
    candidates
}

fn xorb_candidate_paths(sources: &[PathBuf], item: &RecoveryInventoryItem) -> Vec<PathBuf> {
    let hex = item
        .file_hash
        .strip_prefix("b3:")
        .unwrap_or(&item.file_hash)
        .to_owned();
    let fanout = hex.get(..2);
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for source in sources {
        if source.is_dir() {
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join(".crab").join("xorbs").join(&hex),
            );
            push_candidate(&mut seen, &mut candidates, source.join("xorbs").join(&hex));
            if let Some(fanout) = fanout {
                push_candidate(
                    &mut seen,
                    &mut candidates,
                    source.join("xorbs").join(fanout).join(&hex),
                );
                push_candidate(
                    &mut seen,
                    &mut candidates,
                    source
                        .join("xorbs")
                        .join(fanout)
                        .join(format!("{hex}.xorb")),
                );
            }
            push_candidate(&mut seen, &mut candidates, source.join(&hex));
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join(format!("{hex}.xorb")),
            );
        } else {
            push_candidate(&mut seen, &mut candidates, source.clone());
        }
    }
    candidates
}

fn xorb_cache_candidate_paths(
    cache_roots: &[PathBuf],
    item: &RecoveryInventoryItem,
) -> Result<Vec<PathBuf>> {
    let hex = item
        .file_hash
        .strip_prefix("b3:")
        .unwrap_or(&item.file_hash);
    if hex.len() < 2 {
        return Err(CrabError::Configuration {
            key: "recover digest".to_owned(),
            origin: format!("expected b3 digest, got {}", item.file_hash),
        });
    }
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for root in cache_roots {
        push_candidate(
            &mut seen,
            &mut candidates,
            root.join("xorbs").join(&hex[..2]).join(hex),
        );
        push_candidate(
            &mut seen,
            &mut candidates,
            root.join("xorbs")
                .join(&hex[..2])
                .join(format!("{hex}.xorb")),
        );
    }
    Ok(candidates)
}

fn shard_candidate_paths(sources: &[PathBuf], item: &RecoveryInventoryItem) -> Vec<PathBuf> {
    let hex = item
        .file_hash
        .strip_prefix("b3:")
        .unwrap_or(&item.file_hash)
        .to_owned();
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for source in sources {
        if source.is_dir() {
            push_candidate(
                &mut seen,
                &mut candidates,
                source.join(".crab").join("shards").join(&hex),
            );
            push_candidate(&mut seen, &mut candidates, source.join("shards").join(&hex));
            push_candidate(&mut seen, &mut candidates, source.join(&hex));
            push_candidate(&mut seen, &mut candidates, source.join(&item.path));
        } else {
            push_candidate(&mut seen, &mut candidates, source.clone());
        }
    }
    candidates
}

fn cache_candidate_paths(cache_roots: &[PathBuf], expected_hash: &str) -> Result<Vec<PathBuf>> {
    let hex = expected_hash.strip_prefix("b3:").unwrap_or(expected_hash);
    if hex.len() < 2 {
        return Err(CrabError::Configuration {
            key: "recover digest".to_owned(),
            origin: format!("expected b3 digest, got {expected_hash}"),
        });
    }
    Ok(cache_roots
        .iter()
        .map(|root| {
            root.join("xorbs")
                .join(&hex[..2])
                .join(format!("{hex}.xorb"))
        })
        .collect())
}

fn workflow_journal_candidate_paths(journals: &[PathBuf], item_path: &str) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for journal in journals {
        if let Some(repo_root) = workflow_journal_repo_root(journal) {
            push_candidate(&mut seen, &mut candidates, repo_root.join(item_path));
        }
    }
    candidates
}

fn workflow_journal_repo_root(path: &Path) -> Option<PathBuf> {
    let db_path = workflow_journal_db_path(path);
    let crab_dir = db_path.parent()?.parent()?.parent()?.parent()?;
    if crab_dir.file_name().and_then(|name| name.to_str()) == Some(".crab") {
        return crab_dir.parent().map(Path::to_path_buf);
    }
    None
}

fn candidate_paths(sources: &[PathBuf], item_path: &str) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for source in sources {
        let source_candidate = if source.is_dir() {
            source.join(item_path)
        } else {
            source.clone()
        };
        push_candidate(&mut seen, &mut candidates, source_candidate);
        if source.is_dir()
            && let Some(name) = Path::new(item_path).file_name()
        {
            push_candidate(&mut seen, &mut candidates, source.join(name));
        }
    }
    candidates
}

fn push_candidate(seen: &mut BTreeSet<PathBuf>, candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        candidates.push(path);
    }
}

#[cfg(test)]
fn apply_plan(plan: &RecoverPlanPayload, restore_root: &Path) -> Result<RecoverApplyPayload> {
    let metadata_repairs = vec![None; plan.items.len()];
    apply_plan_with_repairs(
        plan,
        restore_root,
        &metadata_repairs,
        None,
        None,
        None,
        None,
    )
}

fn apply_plan_with_repairs(
    plan: &RecoverPlanPayload,
    restore_root: &Path,
    metadata_repairs: &[Option<RecoverApplyState>],
    shard_repair: Option<&ShardRestoreResult>,
    xorb_repair: Option<&XorbRestoreResult>,
    pack_repair: Option<&PackRestoreResult>,
    remote_repair: Option<&RemoteRepairResult>,
) -> Result<RecoverApplyPayload> {
    admit_restore_root(restore_root)?;
    let lock = lock_restore_root(restore_root)?;
    let shard_states = shard_repair.map(|repair| repair.item_repaired.as_slice());
    let xorb_states = xorb_repair.map(|repair| repair.item_repaired.as_slice());
    let pack_states = pack_repair.map(|repair| repair.item_repaired.as_slice());
    let remote_states = remote_repair.map(|repair| repair.item_repaired.as_slice());

    let mut payload = RecoverApplyPayload {
        plan_id: plan.plan_id.clone(),
        restore_root: restore_root.display().to_string(),
        restored: 0,
        already_present: 0,
        failed: 0,
        inventory_only: 0,
        metadata_repaired: 0,
        shards_repaired: shard_repair.map_or(0, |repair| repair.shards),
        shard_bytes_repaired: shard_repair.map_or(0, |repair| repair.bytes),
        xorbs_repaired: xorb_repair.map_or(0, |repair| repair.xorbs),
        xorb_bytes_repaired: xorb_repair.map_or(0, |repair| repair.bytes),
        packs_repaired: pack_repair.map_or(0, |repair| repair.packs),
        pack_bytes_repaired: pack_repair.map_or(0, |repair| repair.bytes),
        remote_repaired: remote_repair.map_or(0, |repair| repair.files),
        remote_bytes_repaired: remote_repair.map_or(0, |repair| repair.bytes),
        remote_refspecs: remote_repair.map_or_else(Vec::new, |repair| repair.refspecs.clone()),
        items: Vec::new(),
    };

    for (idx, item) in plan.items.iter().enumerate() {
        let remote_repaired = remote_states
            .and_then(|states| states.get(idx))
            .copied()
            .unwrap_or(false);
        let shard_repaired = shard_states
            .and_then(|states| states.get(idx))
            .copied()
            .unwrap_or(false);
        let xorb_repaired = xorb_states
            .and_then(|states| states.get(idx))
            .copied()
            .unwrap_or(false);
        let pack_repaired = pack_states
            .and_then(|states| states.get(idx))
            .copied()
            .unwrap_or(false);
        let state = if shard_repaired {
            RecoverApplyState::ShardRepaired
        } else if xorb_repaired {
            RecoverApplyState::XorbRepaired
        } else if pack_repaired {
            RecoverApplyState::PackRepaired
        } else {
            metadata_repairs
                .get(idx)
                .copied()
                .flatten()
                .map_or_else(|| apply_item(item, restore_root), Ok)?
        };
        match state {
            RecoverApplyState::Restored => payload.restored += 1,
            RecoverApplyState::AlreadyPresent => payload.already_present += 1,
            RecoverApplyState::Failed => payload.failed += 1,
            RecoverApplyState::InventoryOnly => payload.inventory_only += 1,
            RecoverApplyState::MetadataRepaired => payload.metadata_repaired += 1,
            RecoverApplyState::ShardRepaired => {}
            RecoverApplyState::XorbRepaired => {}
            RecoverApplyState::PackRepaired => {}
            RecoverApplyState::Skipped => {}
        }
        payload.items.push(RecoverApplyItem {
            path: item.path.clone(),
            state,
            remote_repaired,
            message: apply_message(item, state),
        });
    }
    // Release the root lock before returning so a sequential apply can reacquire it.
    LockFileExt::unlock(&lock).map_err(CrabError::Io)?;
    Ok(payload)
}

fn apply_message(item: &RecoverPlanItem, state: RecoverApplyState) -> Option<String> {
    match state {
        RecoverApplyState::Failed if item.item_kind == RecoverItemKind::FileIndex => {
            Some("file-index mapping was not rebuilt from durable shard objects".to_owned())
        }
        RecoverApplyState::Failed => Some("candidate bytes did not restore".to_owned()),
        RecoverApplyState::Skipped if item.item_kind == RecoverItemKind::File => {
            Some("no verified candidate available".to_owned())
        }
        RecoverApplyState::Skipped
            if item.item_kind == RecoverItemKind::Shard
                && item.state == RecoverItemState::Repairable =>
        {
            Some("verified shard candidate available; rerun with `--restore-shards` to repair the remote shard object".to_owned())
        }
        RecoverApplyState::Skipped
            if item.item_kind == RecoverItemKind::Xorb
                && item.state == RecoverItemState::Repairable =>
        {
            Some("verified xorb candidate available; rerun with `--restore-xorbs` to repair the remote xorb object".to_owned())
        }
        RecoverApplyState::Skipped
            if item.item_kind == RecoverItemKind::Pack
                && item.state == RecoverItemState::Repairable =>
        {
            Some("verified pack candidate available; rerun with `--restore-packs` to repair the remote pack object".to_owned())
        }
        RecoverApplyState::InventoryOnly => Some(inventory_only_message(item.item_kind).to_owned()),
        RecoverApplyState::Skipped => Some("no recovery action for this plan item".to_owned()),
        RecoverApplyState::MetadataRepaired => {
            Some("file-index mapping verified in file_index_db".to_owned())
        }
        RecoverApplyState::ShardRepaired => Some("shard object restored to remote storage".to_owned()),
        RecoverApplyState::XorbRepaired => Some("xorb object restored to remote storage".to_owned()),
        RecoverApplyState::PackRepaired => Some("pack object restored to remote storage".to_owned()),
        RecoverApplyState::Restored | RecoverApplyState::AlreadyPresent => None,
    }
}

fn repairable_action(item_kind: RecoverItemKind) -> &'static str {
    match item_kind {
        RecoverItemKind::File => "restore_verified_bytes",
        RecoverItemKind::Shard => "restore_shard_object",
        RecoverItemKind::Xorb => "restore_xorb_object",
        RecoverItemKind::Pack => "restore_pack_object",
        RecoverItemKind::FileIndex => "rebuild_file_index_after_shard_restore",
    }
}

fn inventory_only_action(item_kind: RecoverItemKind) -> &'static str {
    match item_kind {
        RecoverItemKind::Shard => "restore_shard_object_or_repush",
        RecoverItemKind::Xorb => "restore_xorb_object_from_cache_or_replica",
        RecoverItemKind::Pack => "restore_pack_object_from_replica",
        RecoverItemKind::FileIndex => "rebuild_file_index_after_shard_restore",
        RecoverItemKind::File => "inspect_metadata_reference",
    }
}

fn inventory_only_message(item_kind: RecoverItemKind) -> &'static str {
    match item_kind {
        RecoverItemKind::Shard => {
            "shard reference recorded; restore the shard object from backup or re-push original file bytes"
        }
        RecoverItemKind::Xorb => {
            "xorb reference recorded; restore the xorb object from local cache, backup, or a healthy replica"
        }
        RecoverItemKind::Pack => {
            "pack reference recorded; restore the Git pack from a healthy replica or source remote"
        }
        RecoverItemKind::FileIndex => {
            "file-index mapping recorded; after shard objects are present, rerun with `--rebuild-file-index` or use `crab metadb rebuild --db file_index`"
        }
        RecoverItemKind::File => {
            "metadata reference recorded for inspection; recover apply does not repair remote metadata"
        }
    }
}

async fn file_index_metadata_repairs(
    plan: &RecoverPlanPayload,
) -> Result<Vec<Option<RecoverApplyState>>> {
    let planned_entries = planned_file_index_entries(plan)?;
    let mut states = vec![None; plan.items.len()];
    if planned_entries.is_empty() {
        return Ok(states);
    }

    let entries: Vec<(MerkleHash, MerkleHash)> = planned_entries
        .iter()
        .map(|entry| (entry.file_hash, entry.shard_hash))
        .collect();
    let verified =
        crate::cmd::metadb::rebuild_file_index_for_current_repo_and_verify(&entries).await?;

    for (entry, matches_plan) in planned_entries.into_iter().zip(verified.into_iter()) {
        states[entry.item_index] = Some(if matches_plan {
            RecoverApplyState::MetadataRepaired
        } else {
            RecoverApplyState::Failed
        });
    }
    Ok(states)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlannedFileIndexEntry {
    item_index: usize,
    file_hash: MerkleHash,
    shard_hash: MerkleHash,
}

fn planned_file_index_entries(plan: &RecoverPlanPayload) -> Result<Vec<PlannedFileIndexEntry>> {
    let mut entries = Vec::new();
    for (idx, item) in plan.items.iter().enumerate() {
        if item.item_kind != RecoverItemKind::FileIndex
            || item.state != RecoverItemState::InventoryOnly
        {
            continue;
        }
        let metadata = item
            .metadata
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "recover file-index metadata".to_owned(),
                origin: format!("{} is missing metadata", item.path),
            })?;
        let shard_hash = metadata
            .get("shard_hash")
            .ok_or_else(|| CrabError::Configuration {
                key: "recover file-index metadata".to_owned(),
                origin: format!("{} is missing shard_hash", item.path),
            })?;
        entries.push(PlannedFileIndexEntry {
            item_index: idx,
            file_hash: MerkleHash::from(parse_b3_digest(&item.file_hash)?),
            shard_hash: MerkleHash::from(parse_b3_digest(shard_hash)?),
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShardRestoreResult {
    item_repaired: Vec<bool>,
    shards: u64,
    bytes: u64,
}

async fn restore_shards_from_plan(
    plan: &RecoverPlanPayload,
    args: &RecoverApplyArgs,
    cancel: &CancellationToken,
) -> Result<ShardRestoreResult> {
    let repo_root = recover_current_repo_root()?;
    let remote = open_recovery_write_remote(
        &repo_root,
        args.remote.as_deref(),
        "recover.restore_shards",
        cancel,
    )
    .await?;
    let result = async {
        let mut item_repaired = vec![false; plan.items.len()];
        let mut shards = 0u64;
        let mut bytes = 0u64;

        for (idx, item) in plan.items.iter().enumerate() {
            let Some(candidate) = verified_shard_restore_candidate(item)? else {
                continue;
            };
            let shard_hash = MerkleHash::from(parse_b3_digest(&item.file_hash)?);
            let shard_path = remote.router.shard_path(&shard_hash.hex());
            remote
                .store
                .put(&shard_path, candidate.bytes.clone())
                .await?;
            crate::cmd::gc::closure::publish(
                &remote.store,
                remote.router.global_prefix(),
                &shard_hash,
                candidate.bytes.clone(),
                shard_path.as_ref(),
            )
            .await?;
            item_repaired[idx] = true;
            shards = shards.saturating_add(1);
            bytes = bytes.saturating_add(candidate.size);
        }

        if shards == 0 {
            return Err(CrabError::Configuration {
                key: "recover apply --restore-shards".to_owned(),
                origin: "recovery plan has no repairable shard entries with verified candidates"
                    .to_owned(),
            });
        }

        Ok(ShardRestoreResult {
            item_repaired,
            shards,
            bytes,
        })
    }
    .await;
    remote.finish(result).await
}

struct VerifiedShardCandidate {
    bytes: Bytes,
    size: u64,
}

fn verified_shard_restore_candidate(
    item: &RecoverPlanItem,
) -> Result<Option<VerifiedShardCandidate>> {
    if item.item_kind != RecoverItemKind::Shard || item.state != RecoverItemState::Repairable {
        return Ok(None);
    }
    let Some(candidate) = &item.candidate else {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-shards".to_owned(),
            origin: format!("{} is repairable but has no shard candidate", item.path),
        });
    };
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    let candidate_path = PathBuf::from(&candidate.source_path);
    if !candidate_path.is_file() {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-shards".to_owned(),
            origin: format!("candidate {} is not a file", candidate_path.display()),
        });
    }
    let bytes = std::fs::read(&candidate_path).map_err(CrabError::Io)?;
    let actual_hash = *blake3::hash(&bytes).as_bytes();
    if actual_hash != expected_hash {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-shards".to_owned(),
            origin: format!(
                "candidate {} hash does not match planned shard {}",
                candidate_path.display(),
                item.file_hash
            ),
        });
    }
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size != item.size || size != candidate.size {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-shards".to_owned(),
            origin: format!(
                "candidate {} has size {}, expected plan size {}",
                candidate_path.display(),
                size,
                item.size
            ),
        });
    }
    Ok(Some(VerifiedShardCandidate {
        bytes: Bytes::from(bytes),
        size,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct XorbRestoreResult {
    item_repaired: Vec<bool>,
    xorbs: u64,
    bytes: u64,
}

async fn restore_xorbs_from_plan(
    plan: &RecoverPlanPayload,
    args: &RecoverApplyArgs,
    cancel: &CancellationToken,
) -> Result<XorbRestoreResult> {
    let repo_root = recover_current_repo_root()?;
    let remote = open_recovery_write_remote(
        &repo_root,
        args.remote.as_deref(),
        "recover.restore_xorbs",
        cancel,
    )
    .await?;
    let result = async {
        let mut item_repaired = vec![false; plan.items.len()];
        let mut xorbs = 0u64;
        let mut bytes = 0u64;

        for (idx, item) in plan.items.iter().enumerate() {
            let Some(candidate) = verified_xorb_restore_candidate(item)? else {
                continue;
            };
            let xorb_hash = parse_merkle_digest(&item.file_hash)?;
            let xorb_path = remote.router.xorb_path(&xorb_hash.hex());
            remote
                .store
                .put(&xorb_path, candidate.bytes.clone())
                .await?;
            item_repaired[idx] = true;
            xorbs = xorbs.saturating_add(1);
            bytes = bytes.saturating_add(candidate.size);
        }

        if xorbs == 0 {
            return Err(CrabError::Configuration {
                key: "recover apply --restore-xorbs".to_owned(),
                origin: "recovery plan has no repairable xorb entries with verified candidates"
                    .to_owned(),
            });
        }

        Ok(XorbRestoreResult {
            item_repaired,
            xorbs,
            bytes,
        })
    }
    .await;
    remote.finish(result).await
}

#[derive(Debug)]
struct VerifiedXorbCandidate {
    bytes: Bytes,
    size: u64,
}

fn verified_xorb_restore_candidate(
    item: &RecoverPlanItem,
) -> Result<Option<VerifiedXorbCandidate>> {
    if item.item_kind != RecoverItemKind::Xorb || item.state != RecoverItemState::Repairable {
        return Ok(None);
    }
    let Some(candidate) = &item.candidate else {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!("{} is repairable but has no xorb candidate", item.path),
        });
    };
    let expected_hash = parse_merkle_digest(&item.file_hash)?;
    let candidate_path = PathBuf::from(&candidate.source_path);
    if !candidate_path.is_file() {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!("candidate {} is not a file", candidate_path.display()),
        });
    }
    let bytes = std::fs::read(&candidate_path).map_err(CrabError::Io)?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if candidate.size != 0 && size != candidate.size {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!(
                "candidate {} has size {}, expected plan candidate size {}",
                candidate_path.display(),
                size,
                candidate.size
            ),
        });
    }
    if item.size != 0 && size != item.size {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!(
                "candidate {} has size {}, expected plan size {}",
                candidate_path.display(),
                size,
                item.size
            ),
        });
    }
    let bytes = Bytes::from(bytes);
    let parser = XorbParser::parse(bytes.clone()).map_err(|err| CrabError::Configuration {
        key: "recover apply --restore-xorbs".to_owned(),
        origin: format!(
            "candidate {} is not a valid xorb: {err}",
            candidate_path.display()
        ),
    })?;
    if parser.hash() != expected_hash {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!(
                "candidate {} xorb hash does not match planned xorb {}",
                candidate_path.display(),
                item.file_hash
            ),
        });
    }
    parser
        .verify_all_chunks()
        .map_err(|err| CrabError::Configuration {
            key: "recover apply --restore-xorbs".to_owned(),
            origin: format!(
                "candidate {} has corrupt xorb payload: {err}",
                candidate_path.display()
            ),
        })?;
    Ok(Some(VerifiedXorbCandidate { bytes, size }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackRestoreResult {
    item_repaired: Vec<bool>,
    packs: u64,
    bytes: u64,
}

async fn restore_packs_from_plan(
    plan: &RecoverPlanPayload,
    args: &RecoverApplyArgs,
    cancel: &CancellationToken,
) -> Result<PackRestoreResult> {
    let repo_root = recover_current_repo_root()?;
    let remote = open_recovery_write_remote(
        &repo_root,
        args.remote.as_deref(),
        "recover.restore_packs",
        cancel,
    )
    .await?;
    let result = async {
        let mut item_repaired = vec![false; plan.items.len()];
        let mut packs = 0u64;
        let mut bytes = 0u64;

        for (idx, item) in plan.items.iter().enumerate() {
            let Some(candidate) = verified_pack_restore_candidate(item)? else {
                continue;
            };
            let pack_id = item.file_hash.trim_start_matches("b3:");
            let pack_path = remote.router.pack_path(pack_id);
            let metadata_path = remote.router.pack_metadata_path(pack_id);
            remote
                .store
                .put(&pack_path, candidate.bytes.clone())
                .await?;
            remote
                .store
                .put(&metadata_path, candidate.metadata.clone())
                .await?;
            item_repaired[idx] = true;
            packs = packs.saturating_add(1);
            bytes = bytes.saturating_add(candidate.size);
        }

        if packs == 0 {
            return Err(CrabError::Configuration {
                key: "recover apply --restore-packs".to_owned(),
                origin: "recovery plan has no repairable pack entries with verified candidates"
                    .to_owned(),
            });
        }

        Ok(PackRestoreResult {
            item_repaired,
            packs,
            bytes,
        })
    }
    .await;
    remote.finish(result).await
}

struct VerifiedPackCandidate {
    bytes: Bytes,
    metadata: Bytes,
    size: u64,
}

fn verified_pack_restore_candidate(
    item: &RecoverPlanItem,
) -> Result<Option<VerifiedPackCandidate>> {
    if item.item_kind != RecoverItemKind::Pack || item.state != RecoverItemState::Repairable {
        return Ok(None);
    }
    let Some(candidate) = &item.candidate else {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!("{} is repairable but has no pack candidate", item.path),
        });
    };
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    let pack_path = PathBuf::from(&candidate.source_path);
    if !pack_path.is_file() {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!("candidate {} is not a file", pack_path.display()),
        });
    }
    let bytes = std::fs::read(&pack_path).map_err(CrabError::Io)?;
    let actual_hash = *blake3::hash(&bytes).as_bytes();
    if actual_hash != expected_hash {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!(
                "candidate {} hash does not match planned pack {}",
                pack_path.display(),
                item.file_hash
            ),
        });
    }
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size != item.size || size != candidate.size {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!(
                "candidate {} has size {}, expected plan size {}",
                pack_path.display(),
                size,
                item.size
            ),
        });
    }
    let object_count = verify_pack_body(&bytes)?;
    let metadata = verified_pack_metadata(item, &pack_path, object_count)?;
    validate_pack_metadata_object_count(item, object_count, &metadata)?;
    Ok(Some(VerifiedPackCandidate {
        bytes: Bytes::from(bytes),
        metadata,
        size,
    }))
}

fn verify_pack_body(bytes: &[u8]) -> Result<u64> {
    use gix_pack::data::input::{BytesToEntriesIter, Mode};
    use std::io::BufReader;

    const HEADER_LEN: usize = 12;
    if bytes.len() < HEADER_LEN {
        return Err(CrabError::PackIntegrity {
            expected: "PACK header".to_owned(),
            computed: format!("{} bytes", bytes.len()),
        });
    }
    if &bytes[0..4] != b"PACK" {
        return Err(CrabError::PackIntegrity {
            expected: "PACK header".to_owned(),
            computed: format!("{:?}", &bytes[0..4]),
        });
    }
    crab_git::pack::verify_pack_sha1(bytes).map_err(CrabError::from)?;
    let declared = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as u64;
    let reader = BufReader::new(bytes);
    let iter = BytesToEntriesIter::new_from_header(
        reader,
        Mode::Verify,
        gix_pack::data::input::EntryDataMode::Ignore,
        gix_hash::Kind::Sha1,
    )
    .map_err(|e| CrabError::Internal(format!("failed to open pack for recovery: {e}")))?;
    let mut actual = 0_u64;
    for entry in iter {
        entry.map_err(|e| CrabError::Internal(format!("pack entry iteration failed: {e}")))?;
        actual = actual.saturating_add(1);
    }
    if actual != declared {
        return Err(CrabError::PackIntegrity {
            expected: format!("{declared} pack entries"),
            computed: format!("{actual} pack entries"),
        });
    }
    Ok(actual)
}

fn verified_pack_metadata(
    item: &RecoverPlanItem,
    pack_path: &Path,
    object_count: u64,
) -> Result<Bytes> {
    let expected_id = item.file_hash.trim_start_matches("b3:");
    if let Some(metadata_path) = pack_metadata_candidate_path(pack_path)
        && metadata_path.is_file()
    {
        let bytes = std::fs::read(&metadata_path).map_err(CrabError::Io)?;
        let metadata: crab_metadata::pack_metadata::PackMetadata =
            serde_json::from_slice(&bytes).map_err(|e| CrabError::Configuration {
                key: "recover apply --restore-packs".to_owned(),
                origin: format!("invalid pack metadata {}: {e}", metadata_path.display()),
            })?;
        if metadata.pack_id != expected_id {
            return Err(CrabError::Configuration {
                key: "recover apply --restore-packs".to_owned(),
                origin: format!(
                    "pack metadata {} has pack_id {}, expected {}",
                    metadata_path.display(),
                    metadata.pack_id,
                    expected_id
                ),
            });
        }
        return Ok(Bytes::from(bytes));
    }

    let metadata = pack_metadata_from_plan_item(item, object_count)?;
    let bytes = serde_json::to_vec(&metadata).map_err(|e| {
        CrabError::Internal(format!("failed to serialize recovery PackMetadata: {e}"))
    })?;
    Ok(Bytes::from(bytes))
}

fn validate_pack_metadata_object_count(
    item: &RecoverPlanItem,
    object_count: u64,
    metadata: &[u8],
) -> Result<()> {
    let parsed: crab_metadata::pack_metadata::PackMetadata = serde_json::from_slice(metadata)
        .map_err(|e| CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!("invalid generated metadata for {}: {e}", item.path),
        })?;
    if parsed.object_count != 0 && parsed.object_count != object_count {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!(
                "pack metadata object_count {} does not match pack body count {} for {}",
                parsed.object_count, object_count, item.path
            ),
        });
    }
    Ok(())
}

fn pack_metadata_candidate_path(pack_path: &Path) -> Option<PathBuf> {
    if pack_path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
        return Some(pack_path.with_extension("meta"));
    }
    let name = pack_path.file_name()?.to_str()?;
    Some(pack_path.with_file_name(format!("{name}.meta")))
}

fn pack_metadata_from_plan_item(
    item: &RecoverPlanItem,
    pack_object_count: u64,
) -> Result<crab_metadata::pack_metadata::PackMetadata> {
    let expected_id = item.file_hash.trim_start_matches("b3:").to_owned();
    let metadata = item
        .metadata
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!("{} is missing pack metadata", item.path),
        })?;
    let object_count = match metadata.get("object_count") {
        Some(value) => value.parse::<u64>().map_err(|e| CrabError::Configuration {
            key: "recover apply --restore-packs".to_owned(),
            origin: format!("invalid object_count for {}: {e}", item.path),
        })?,
        None => pack_object_count,
    };
    let ref_tips = metadata
        .get("ref_tips")
        .map(|value| {
            value
                .split(',')
                .filter_map(|tip| {
                    let trimmed = tip.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_owned())
                    }
                })
                .collect()
        })
        .unwrap_or_else(Vec::new);
    Ok(crab_metadata::pack_metadata::PackMetadata {
        pack_id: expected_id,
        ref_tips,
        object_count,
    })
}

struct RecoveryWriteRemote {
    store: crate::storage::Store,
    router: crate::storage::StoreLayout,
    gc_writer: crate::maintenance::GcWriterLeases,
}

impl RecoveryWriteRemote {
    async fn finish<T>(self, result: Result<T>) -> Result<T> {
        let release = self.gc_writer.release().await;
        match (result, release) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

async fn open_recovery_write_remote(
    repo_root: &Path,
    remote_name: Option<&str>,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<RecoveryWriteRemote> {
    let remote_url = crate::cmd::workflow::read_crab_remote_url_for(repo_root, remote_name)?;
    let parsed = crate::git::url::CrabUrl::parse(&remote_url)?;
    let config = crate::core::config::Config::resolve_for_repo(repo_root)?;
    let selection = crate::replication::StoreResolver::new(&config, &parsed, cancel)
        .write_store(operation)
        .await?;
    let gc_writer = crate::maintenance::GcWriterLeases::acquire(
        &selection.store,
        selection.router.global_prefix(),
        selection.router.repo_prefix(),
        cancel,
    )
    .await?;
    Ok(RecoveryWriteRemote {
        store: selection.store,
        router: selection.router,
        gc_writer,
    })
}

fn validate_apply_remote_args(args: &RecoverApplyArgs) -> Result<()> {
    if args.remote.is_some()
        && !args.repair_remote
        && !args.restore_shards
        && !args.restore_xorbs
        && !args.restore_packs
    {
        return Err(CrabError::Configuration {
            key: "recover apply --remote".to_owned(),
            origin: "--remote requires --repair-remote, --restore-shards, --restore-xorbs, or --restore-packs"
                .to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteRepairResult {
    item_repaired: Vec<bool>,
    files: u64,
    bytes: u64,
    refspecs: Vec<String>,
}

async fn repair_remote_from_plan(
    plan: &RecoverPlanPayload,
    args: &RecoverApplyArgs,
    cancel: &CancellationToken,
) -> Result<RemoteRepairResult> {
    let manifest = read_manifest(Path::new(&plan.manifest_path))?;
    let repo_root = recover_current_repo_root()?;
    let refspecs = remote_repair_refspecs(args, &manifest)?;
    verify_remote_repair_refspecs(&repo_root, &refspecs, &manifest.revision.commit)?;

    let staging_root = repo_root.join(".crab").join("staging");
    let staging = StagingArea::open_blocking_default(staging_root).await?;
    let stage_result = stage_remote_repair_files(plan, &repo_root, &staging, cancel).await;
    let close_result = staging.close().await.map_err(CrabError::from);
    let mut staged = close_remote_repair_staging(stage_result, close_result)?;

    if staged.files == 0 {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: "recovery plan has no repairable file-byte entries to push".to_owned(),
        });
    }

    crate::cmd::push::run_push_repair_refspecs(args.remote.as_deref(), &refspecs, cancel).await?;
    staged.refspecs = refspecs;
    Ok(staged)
}

async fn stage_remote_repair_files(
    plan: &RecoverPlanPayload,
    repo_root: &Path,
    staging: &StagingArea,
    cancel: &CancellationToken,
) -> Result<RemoteRepairResult> {
    let mut item_repaired = vec![false; plan.items.len()];
    let mut files = 0u64;
    let mut bytes = 0u64;

    for (idx, item) in plan.items.iter().enumerate() {
        let Some(candidate_path) = verified_remote_repair_candidate(item)? else {
            continue;
        };
        let repo_path = safe_repo_relative_path(&item.path)?;
        let result = stage_file_streaming_as(
            &candidate_path,
            repo_root,
            &repo_path,
            staging,
            StreamStageProgress::default(),
            cancel,
        )
        .await?;
        let expected_hash = parse_b3_digest(&item.file_hash)?;
        if result.file_hash != expected_hash || result.size != item.size {
            return Err(CrabError::Configuration {
                key: "recover apply --repair-remote".to_owned(),
                origin: format!(
                    "staged {} as {} bytes/{}, expected {} bytes/{}",
                    item.path,
                    result.size,
                    format_b3_digest(result.file_hash),
                    item.size,
                    item.file_hash
                ),
            });
        }
        item_repaired[idx] = true;
        files = files.saturating_add(1);
        bytes = bytes.saturating_add(item.size);
    }

    Ok(RemoteRepairResult {
        item_repaired,
        files,
        bytes,
        refspecs: Vec::new(),
    })
}

fn close_remote_repair_staging<T>(result: Result<T>, close_result: Result<()>) -> Result<T> {
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(close_err)) => {
            warn!(error = %close_err, "remote recovery staging close failed after staging error");
            Err(err)
        }
    }
}

fn verified_remote_repair_candidate(item: &RecoverPlanItem) -> Result<Option<PathBuf>> {
    if item.item_kind != RecoverItemKind::File || item.state != RecoverItemState::Repairable {
        return Ok(None);
    }
    let Some(candidate) = &item.candidate else {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: format!("{} is repairable but has no candidate", item.path),
        });
    };
    let expected_hash = parse_b3_digest(&item.file_hash)?;
    let candidate_path = PathBuf::from(&candidate.source_path);
    if !candidate_path.is_file() {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: format!("candidate {} is not a file", candidate_path.display()),
        });
    }
    let metadata = std::fs::metadata(&candidate_path).map_err(CrabError::Io)?;
    if metadata.len() != item.size {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: format!(
                "candidate {} has size {}, expected {}",
                candidate_path.display(),
                metadata.len(),
                item.size
            ),
        });
    }
    if hash_file_blake3(&candidate_path).map_err(CrabError::Io)? != expected_hash {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: format!(
                "candidate {} hash does not match plan",
                candidate_path.display()
            ),
        });
    }
    Ok(Some(candidate_path))
}

fn remote_repair_refspecs(
    args: &RecoverApplyArgs,
    manifest: &ReleaseManifest,
) -> Result<Vec<String>> {
    if !args.repair_refspec.is_empty() {
        return Ok(args.repair_refspec.clone());
    }

    let refspecs: Vec<String> = manifest
        .selected_refs
        .iter()
        .filter(|(name, target)| {
            name.starts_with("refs/heads/") && target.oid == manifest.revision.commit
        })
        .map(|(name, _)| format!("{name}:{name}"))
        .collect();

    if refspecs.is_empty() {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: "manifest has no selected branch ref at its release commit; pass --repair-refspec refs/heads/<name>:refs/heads/<name>".to_owned(),
        });
    }
    Ok(refspecs)
}

fn verify_remote_repair_refspecs(
    repo_root: &Path,
    refspecs: &[String],
    manifest_commit: &str,
) -> Result<()> {
    for refspec in refspecs {
        let src = normalized_repair_refspec_src(refspec)?;
        let commit = resolve_recover_commit(repo_root, &src)?;
        if commit != manifest_commit {
            return Err(CrabError::Configuration {
                key: "recover apply --repair-refspec".to_owned(),
                origin: format!(
                    "{src} resolves to {commit}, expected manifest commit {manifest_commit}"
                ),
            });
        }
    }
    Ok(())
}

fn normalized_repair_refspec_src(refspec: &str) -> Result<String> {
    let rest = refspec.strip_prefix('+').unwrap_or(refspec);
    let src = rest.split_once(':').map_or(rest, |(src, _)| src);
    if src.is_empty() {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-refspec".to_owned(),
            origin: "delete refspecs cannot repair remote content".to_owned(),
        });
    }
    if src == "HEAD" || src.starts_with("refs/heads/") {
        return Ok(src.to_owned());
    }
    if src.starts_with("refs/") {
        return Err(CrabError::Configuration {
            key: "recover apply --repair-refspec".to_owned(),
            origin: format!("{src} is not a branch ref"),
        });
    }
    Ok(format!("refs/heads/{src}"))
}

fn recover_current_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "recover apply --repair-remote".to_owned(),
            origin: format!("not inside a Git working tree: {stderr}"),
        });
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Err(CrabError::Internal(
            "git rev-parse --show-toplevel returned an empty path".to_owned(),
        ));
    }
    Ok(PathBuf::from(root))
}

fn resolve_recover_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &spec])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "recover apply --repair-refspec".to_owned(),
            origin: format!("{rev} does not resolve to a commit: {stderr}"),
        });
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CrabError::Internal(format!(
            "git rev-parse returned unexpected commit '{commit}'"
        )));
    }
    Ok(commit)
}

fn safe_repo_relative_path(rel: &str) -> Result<PathBuf> {
    let rel = Path::new(rel);
    let mut out = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "recover item path".to_owned(),
                    origin: format!("refusing unsafe repository path {}", rel.display()),
                });
            }
        }
    }
    Ok(out)
}

fn admit_restore_root(restore_root: &Path) -> Result<()> {
    if restore_root.exists() && !restore_root.is_dir() {
        return Err(CrabError::Configuration {
            key: "recover apply --restore-to".to_owned(),
            origin: format!(
                "restore root {} exists but is not a directory",
                restore_root.display()
            ),
        });
    }
    std::fs::create_dir_all(restore_root).map_err(CrabError::Io)
}

fn lock_restore_root(restore_root: &Path) -> Result<File> {
    let lock_path = restore_root.join(".crab-recover.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(CrabError::Io)?;
    match file.try_lock_exclusive() {
        Ok(true) => Ok(file),
        Ok(false) => Err(CrabError::Configuration {
            key: "recover apply --restore-to".to_owned(),
            origin: format!("restore root {} is already locked", restore_root.display()),
        }),
        Err(err) => Err(CrabError::Io(err)),
    }
}

fn apply_item(item: &RecoverPlanItem, restore_root: &Path) -> Result<RecoverApplyState> {
    if item.item_kind != RecoverItemKind::File {
        return match item.state {
            RecoverItemState::InventoryOnly => Ok(RecoverApplyState::InventoryOnly),
            RecoverItemState::Repairable | RecoverItemState::Unrecoverable => {
                Ok(RecoverApplyState::Skipped)
            }
        };
    }
    if item.state != RecoverItemState::Repairable {
        return Ok(RecoverApplyState::Skipped);
    }

    let expected_hash = parse_b3_digest(&item.file_hash)?;
    let dest = safe_restore_path(restore_root, &item.path)?;
    if dest.is_file()
        && std::fs::metadata(&dest)
            .map(|meta| meta.len())
            .unwrap_or_default()
            == item.size
        && hash_file_blake3(&dest).map_err(CrabError::Io)? == expected_hash
    {
        return Ok(RecoverApplyState::AlreadyPresent);
    }

    let Some(candidate) = &item.candidate else {
        return Ok(RecoverApplyState::Failed);
    };
    let candidate_path = PathBuf::from(&candidate.source_path);
    if !candidate_path.is_file() {
        return Ok(RecoverApplyState::Failed);
    }
    let metadata = std::fs::metadata(&candidate_path).map_err(CrabError::Io)?;
    if metadata.len() != item.size {
        return Ok(RecoverApplyState::Failed);
    }
    if hash_file_blake3(&candidate_path).map_err(CrabError::Io)? != expected_hash {
        return Ok(RecoverApplyState::Failed);
    }
    copy_verified_file(&candidate_path, &dest)?;
    Ok(RecoverApplyState::Restored)
}

fn safe_restore_path(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel = Path::new(rel);
    let mut dest = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => dest.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "recover item path".to_owned(),
                    origin: format!("refusing unsafe recovery path {}", rel.display()),
                });
            }
        }
    }
    Ok(dest)
}

fn copy_verified_file(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let mut src = std::fs::File::open(source).map_err(CrabError::Io)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(CrabError::Io)?;
    std::io::copy(&mut src, tmp.as_file_mut()).map_err(CrabError::Io)?;
    tmp.flush().map_err(CrabError::Io)?;
    tmp.persist(dest).map_err(|err| CrabError::Io(err.error))?;
    Ok(())
}

fn hash_file_blake3(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn parse_b3_digest(value: &str) -> Result<[u8; 32]> {
    let hex = value.strip_prefix("b3:").unwrap_or(value);
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: "recover digest".to_owned(),
            origin: format!("expected b3 digest, got {value}"),
        });
    }
    let mut out = [0u8; 32];
    for (idx, byte) in out.iter_mut().enumerate() {
        let start = idx * 2;
        *byte = u8::from_str_radix(&hex[start..start + 2], 16).map_err(|err| {
            CrabError::Configuration {
                key: "recover digest".to_owned(),
                origin: format!("invalid b3 digest {value}: {err}"),
            }
        })?;
    }
    Ok(out)
}

fn parse_merkle_digest(value: &str) -> Result<MerkleHash> {
    let hex = value.strip_prefix("b3:").unwrap_or(value);
    MerkleHash::from_hex(hex).map_err(|err| CrabError::Configuration {
        key: "recover digest".to_owned(),
        origin: format!("invalid merkle digest {value}: {err}"),
    })
}

fn format_b3_digest(bytes: [u8; 32]) -> String {
    format!("b3:{}", blake3::Hash::from_bytes(bytes).to_hex())
}

fn read_manifest(path: &Path) -> Result<ReleaseManifest> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    serde_json::from_slice(&bytes).map_err(|err| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("invalid release manifest JSON: {err}"),
    })
}

fn read_plan(path: &Path) -> Result<RecoverPlanPayload> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    serde_json::from_slice(&bytes).map_err(|err| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("invalid recovery plan JSON: {err}"),
    })
}

fn write_plan(path: &Path, plan: &RecoverPlanPayload) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    let bytes = serde_json::to_vec_pretty(plan)
        .map_err(|err| CrabError::Internal(format!("serialize recovery plan: {err}")))?;
    std::fs::write(path, bytes).map_err(CrabError::Io)
}

fn plan_digest(plan: &RecoverPlanPayload) -> Result<String> {
    let mut copy = plan.clone();
    copy.plan_id.clear();
    let bytes = serde_json::to_vec(&copy)
        .map_err(|err| CrabError::Internal(format!("serialize recovery plan identity: {err}")))?;
    Ok(format!("b3:{}", blake3::hash(&bytes).to_hex()))
}

fn record_recover_apply_audit(payload: &RecoverApplyPayload) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "recover.apply".to_owned(),
        outcome: if payload.failed == 0 {
            AuditOutcome::Success
        } else {
            AuditOutcome::Failure
        },
        actor: None,
        repository: None,
        details: serde_json::json!({
            "plan_id": payload.plan_id.clone(),
            "restore_root": payload.restore_root.clone(),
            "restored": payload.restored,
            "already_present": payload.already_present,
            "failed": payload.failed,
            "inventory_only": payload.inventory_only,
            "metadata_repaired": payload.metadata_repaired,
            "shards_repaired": payload.shards_repaired,
            "shard_bytes_repaired": payload.shard_bytes_repaired,
            "xorbs_repaired": payload.xorbs_repaired,
            "xorb_bytes_repaired": payload.xorb_bytes_repaired,
            "packs_repaired": payload.packs_repaired,
            "pack_bytes_repaired": payload.pack_bytes_repaired,
            "remote_repaired": payload.remote_repaired,
            "remote_bytes_repaired": payload.remote_bytes_repaired,
            "remote_refspecs": payload.remote_refspecs.clone(),
        }),
    });
    append_event(&default_log_path(), &event)
}

#[cfg(test)]
fn placeholder_payload(cmd: &RecoverCmd) -> RecoverStatusPayload {
    PlanApplyResult::not_implemented(
        cmd.command_name(),
        cmd.operation(),
        cmd.idempotent_apply(),
        "repository recovery command is not available for this mode; no changes were made",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{EntryState, ImportEntry, Journal as ImportJournal};
    use crate::metadata::manifest::{PackManifestEntry, serialize_pack_list, serialize_shard_list};
    use crate::release::{
        RELEASE_MANIFEST_SCHEMA_VERSION, ReleaseCrabInventory, ReleaseLargeFile, ReleaseRefTarget,
        ReleaseRevision, ReleaseSignature, ReleaseWorkflowMetadata, ReleaseWorkflowOutput,
        ReleaseWorkflowStage,
    };
    use crate::workflow::journal::Journal as WorkflowJournal;
    use uuid::Uuid;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn manifest_for(path: &str, bytes: &[u8]) -> ReleaseManifest {
        manifest_for_many(&[(path, bytes)])
    }

    fn manifest_for_many(files: &[(&str, &[u8])]) -> ReleaseManifest {
        ReleaseManifest {
            schema_version: RELEASE_MANIFEST_SCHEMA_VERSION,
            release_id: "recover-v1".to_owned(),
            revision: ReleaseRevision {
                requested: "HEAD".to_owned(),
                commit: "0123456789012345678901234567890123456789".to_owned(),
            },
            selected_refs: Default::default(),
            crab: ReleaseCrabInventory {
                large_files: files
                    .iter()
                    .map(|(path, bytes)| ReleaseLargeFile {
                        path: (*path).to_owned(),
                        file_hash: format_b3_digest(*blake3::hash(bytes).as_bytes()),
                        size: bytes.len() as u64,
                        shard_hint: None,
                    })
                    .collect(),
            },
            workflow: ReleaseWorkflowMetadata::default(),
            signature: ReleaseSignature::default(),
        }
    }

    fn workflow_out(path: &str, bytes: &[u8]) -> ReleaseWorkflowOutput {
        ReleaseWorkflowOutput {
            path: path.to_owned(),
            file_hash: format_b3_digest(*blake3::hash(bytes).as_bytes()),
            size: bytes.len() as u64,
        }
    }

    fn write_manifest(path: &Path, manifest: &ReleaseManifest) -> TestResult {
        std::fs::write(path, serde_json::to_vec(manifest)?)?;
        Ok(())
    }

    fn file_index_plan_for_test(root: &Path) -> TestResult<RecoverPlanPayload> {
        let manifest = manifest_for_many(&[]);
        let manifest_path = root.join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let file_index = root.join("file-index.jsonl");
        std::fs::write(
            &file_index,
            serde_json::json!({
                "file_hash": "e".repeat(64),
                "shard_hash": "f".repeat(64),
                "path": "dataset/model.bin",
                "size": 12_u64,
            })
            .to_string(),
        )?;
        Ok(build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                file_indexes: vec![file_index],
                ..RecoverySourceSpec::default()
            },
        )?)
    }

    fn valid_small_pack_bytes() -> TestResult<Vec<u8>> {
        use std::io::Write as _;
        use std::process::Stdio;

        let dir = tempfile::tempdir()?;
        let init_output = Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()?;
        if !init_output.status.success() {
            return Err(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&init_output.stderr)
            )
            .into());
        }
        std::fs::write(dir.path().join("blob.txt"), b"pack recovery fixture")?;
        let hash_output = Command::new("git")
            .args(["hash-object", "-w", "blob.txt"])
            .current_dir(dir.path())
            .output()?;
        if !hash_output.status.success() {
            return Err(format!(
                "git hash-object failed: {}",
                String::from_utf8_lossy(&hash_output.stderr)
            )
            .into());
        }
        let oid = String::from_utf8(hash_output.stdout)?;
        let mut child = Command::new("git")
            .args(["pack-objects", "--stdout"])
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "git pack-objects stdin")
            })?;
            stdin.write_all(oid.trim().as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(format!(
                "git pack-objects failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(output.stdout)
    }

    fn write_cache_candidate(cache_root: &Path, bytes: &[u8]) -> TestResult<PathBuf> {
        let hash = format_b3_digest(*blake3::hash(bytes).as_bytes());
        let hex = hash.strip_prefix("b3:").unwrap_or(&hash);
        let path = cache_root
            .join("xorbs")
            .join(&hex[..2])
            .join(format!("{hex}.xorb"));
        std::fs::create_dir_all(path.parent().expect("cache candidate parent"))?;
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn test_xorb(data: &[u8]) -> TestResult<(MerkleHash, Bytes)> {
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let chunk = Chunk::new(Bytes::copy_from_slice(data));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0))?;
        let xorb = builder
            .finalize()?
            .pop()
            .ok_or_else(|| std::io::Error::other("xorb builder produced no xorb"))?;
        Ok((xorb.hash, xorb.bytes))
    }

    fn write_xorb_cache_candidate(
        cache_root: &Path,
        hash: &MerkleHash,
        bytes: &[u8],
    ) -> TestResult<PathBuf> {
        let hex = hash.hex();
        let path = cache_root.join("xorbs").join(&hex[..2]).join(&hex);
        std::fs::create_dir_all(path.parent().expect("xorb cache candidate parent"))?;
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    #[test]
    fn recover_placeholder_is_non_mutating() {
        let payload = placeholder_payload(&RecoverCmd::Apply(RecoverApplyArgs {
            plan: PathBuf::from("plan.json"),
            restore_to: PathBuf::from("restore"),
            rebuild_file_index: false,
            restore_shards: false,
            restore_xorbs: false,
            restore_packs: false,
            repair_remote: false,
            remote: None,
            repair_refspec: Vec::new(),
            json: false,
        }));
        assert_eq!(payload.command, "recover.apply");
        assert_eq!(payload.operation, PlanApplyOperation::Apply);
        assert!(!payload.mutates);
        assert!(payload.idempotent_apply);
    }

    #[test]
    fn plan_marks_matching_source_repairable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        std::fs::create_dir(&source)?;
        let bytes = b"original model";
        std::fs::write(source.join("model.bin"), bytes)?;
        let manifest = manifest_for("model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan(&manifest_path, &[source])?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.unrecoverable, 0);
        assert_eq!(plan.items[0].state, RecoverItemState::Repairable);
        assert!(plan.items[0].candidate.is_some());
        Ok(())
    }

    #[test]
    fn plan_marks_hash_mismatch_unrecoverable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("model.bin"), b"wrong bytes")?;
        let manifest = manifest_for("model.bin", b"original model");
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan(&manifest_path, &[source])?;

        assert_eq!(plan.repairable, 0);
        assert_eq!(plan.unrecoverable, 1);
        assert_eq!(plan.items[0].state, RecoverItemState::Unrecoverable);
        Ok(())
    }

    #[test]
    fn plan_finds_local_cache_candidate() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cache_root = dir.path().join("cache");
        let bytes = b"cached workflow output";
        let cache_path = write_cache_candidate(&cache_root, bytes)?;
        let manifest = manifest_for("model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                cache_roots: vec![cache_root],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_kind, RecoverCandidateKind::LocalCache);
        assert_eq!(candidate.source_path, cache_path.display().to_string());
        Ok(())
    }

    #[test]
    fn plan_marks_replica_source_candidate() -> TestResult {
        let dir = tempfile::tempdir()?;
        let replica = dir.path().join("replica");
        std::fs::create_dir(&replica)?;
        let bytes = b"replica bytes";
        std::fs::write(replica.join("model.bin"), bytes)?;
        let manifest = manifest_for("model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                replica_sources: vec![replica],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(
            candidate.source_kind,
            RecoverCandidateKind::ConfiguredReplica
        );
        Ok(())
    }

    #[test]
    fn plan_includes_import_journal_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let import_root = dir.path().join("import-root");
        std::fs::create_dir(&import_root)?;
        let bytes = b"imported model";
        let file_hash = *blake3::hash(bytes).as_bytes();
        std::fs::write(import_root.join("imported.bin"), bytes)?;
        let journal = ImportJournal::open(&import_root)?;
        journal.upsert_entry_batch(&[ImportEntry {
            relative_path: "imported.bin".to_owned(),
            version_id: String::new(),
            size: bytes.len() as u64,
            etag: None,
            last_modified: 1,
            is_delete_marker: false,
            state: EntryState::Staged { file_hash },
        }])?;
        drop(journal);
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                import_journals: vec![import_root],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.items[0].path, "imported.bin");
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_kind, RecoverCandidateKind::ImportJournal);
        Ok(())
    }

    #[test]
    fn plan_includes_workflow_journal_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let repo_root = dir.path().join("repo");
        let run_id = Uuid::now_v7();
        let journal_path = repo_root
            .join(".crab")
            .join("workflow")
            .join("runs")
            .join(run_id.to_string())
            .join("journal.db");
        std::fs::create_dir_all(repo_root.join(".crab/workflow/runs"))?;
        let bytes = b"workflow metrics";
        std::fs::write(repo_root.join("metrics.json"), bytes)?;
        let output_hash = format_b3_digest(*blake3::hash(bytes).as_bytes());
        let journal = WorkflowJournal::open(&journal_path)?;
        journal.insert_run_start(run_id, "test", "host")?;
        journal.insert_stage_start(run_id, "eval")?;
        for state in [
            StageState::Resolved,
            StageState::CacheChecked,
            StageState::Running,
            StageState::Produced,
        ] {
            journal.transition(run_id, "eval", 1, state, "{}")?;
        }
        journal.transition(
            run_id,
            "eval",
            1,
            StageState::Hashed,
            &serde_json::json!({
                "outs": [{
                    "path": "metrics.json",
                    "hash": output_hash,
                    "size": bytes.len() as u64,
                }]
            })
            .to_string(),
        )?;
        drop(journal);
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                workflow_journals: vec![journal_path],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.items[0].path, "metrics.json");
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_kind, RecoverCandidateKind::WorkflowJournal);
        Ok(())
    }

    #[test]
    fn plan_includes_workflow_outputs_from_manifest() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        std::fs::create_dir(&source)?;
        let bytes = b"workflow output";
        std::fs::write(source.join("metrics.json"), bytes)?;
        let mut manifest = manifest_for("model.bin", b"model");
        manifest.crab.large_files.clear();
        manifest.workflow.stages.push(ReleaseWorkflowStage {
            name: "eval".to_owned(),
            stage_hash: "b3:stage".to_owned(),
            outs: vec![workflow_out("metrics.json", bytes)],
        });
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;

        let plan = build_plan(&manifest_path, &[source])?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.items[0].path, "metrics.json");
        assert_eq!(plan.items[0].state, RecoverItemState::Repairable);
        Ok(())
    }

    #[test]
    fn plan_includes_shard_list_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let shard_hash = "a".repeat(64);
        let shard_list = dir.path().join("shards.jsonl");
        std::fs::write(
            &shard_list,
            serialize_shard_list(std::slice::from_ref(&shard_hash)),
        )?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                shard_lists: vec![shard_list.clone()],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 0);
        assert_eq!(plan.unrecoverable, 0);
        assert_eq!(plan.inventory_only, 1);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Shard);
        assert_eq!(plan.items[0].path, format!("shard:{shard_hash}"));
        assert_eq!(plan.items[0].file_hash, format!("b3:{shard_hash}"));
        assert_eq!(plan.items[0].state, RecoverItemState::InventoryOnly);
        assert_eq!(plan.items[0].action, "restore_shard_object_or_repush");
        let metadata = plan.items[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("source").map(String::as_str),
            Some("shard_list")
        );
        assert_eq!(
            metadata.get("shard_hash").map(String::as_str),
            Some(format!("b3:{shard_hash}").as_str())
        );
        Ok(())
    }

    #[test]
    fn plan_marks_shard_backup_candidate_repairable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let shard_bytes = b"recoverable shard body";
        let shard_hash = blake3::hash(shard_bytes).to_hex().to_string();
        let shard_list = dir.path().join("shards.jsonl");
        std::fs::write(
            &shard_list,
            serialize_shard_list(std::slice::from_ref(&shard_hash)),
        )?;
        let backup = dir.path().join("backup");
        let shard_path = backup.join(".crab").join("shards").join(&shard_hash);
        std::fs::create_dir_all(shard_path.parent().expect("shard parent"))?;
        std::fs::write(&shard_path, shard_bytes)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                shard_lists: vec![shard_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.inventory_only, 0);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Shard);
        assert_eq!(plan.items[0].state, RecoverItemState::Repairable);
        assert_eq!(plan.items[0].action, "restore_shard_object");
        assert_eq!(plan.items[0].size, shard_bytes.len() as u64);
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_path, shard_path.display().to_string());
        assert_eq!(candidate.size, shard_bytes.len() as u64);
        Ok(())
    }

    #[test]
    fn plan_marks_xorb_cache_candidate_repairable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let (xorb_hash, xorb_bytes) = test_xorb(b"recoverable xorb body")?;
        let xorb_list = dir.path().join("xorbs.jsonl");
        std::fs::write(&xorb_list, format!("{}\n", xorb_hash.hex()))?;
        let cache_root = dir.path().join("cache");
        let xorb_path = write_xorb_cache_candidate(&cache_root, &xorb_hash, &xorb_bytes)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                cache_roots: vec![cache_root],
                xorb_lists: vec![xorb_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.unrecoverable, 0);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Xorb);
        assert_eq!(plan.items[0].path, format!("xorb:{}", xorb_hash.hex()));
        assert_eq!(plan.items[0].state, RecoverItemState::Repairable);
        assert_eq!(plan.items[0].action, "restore_xorb_object");
        assert_eq!(plan.items[0].size, xorb_bytes.len() as u64);
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_kind, RecoverCandidateKind::LocalCache);
        assert_eq!(candidate.source_path, xorb_path.display().to_string());
        Ok(())
    }

    #[test]
    fn plan_includes_fsck_jsonl_xorb_and_shard_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let (xorb_hash, xorb_bytes) = test_xorb(b"fsck recoverable xorb body")?;
        let shard_hash = "b".repeat(64);
        let fsck_jsonl = dir.path().join("fsck.jsonl");
        std::fs::write(
            &fsck_jsonl,
            format!(
                "{{\"schema\":\"fsck.event\",\"version\":\"1.0\",\"timestamp\":\"2026-04-24T18:32:18.200Z\",\"type\":\"warning\",\"data\":{{\"code\":\"fsck-missing-xorb\",\"message\":\"missing xorb\",\"path\":\"{}\"}}}}\n\
                 {{\"schema\":\"fsck.event\",\"version\":\"1.0\",\"timestamp\":\"2026-04-24T18:32:18.201Z\",\"type\":\"warning\",\"data\":{{\"code\":\"fsck-shard-list-divergence\",\"message\":\"missing shard\",\"path\":\"{}\"}}}}\n\
                 {{\"schema\":\"fsck.event\",\"version\":\"1.0\",\"timestamp\":\"2026-04-24T18:32:18.202Z\",\"type\":\"result\",\"data\":{{}}}}\n",
                xorb_hash.hex(),
                shard_hash
            ),
        )?;
        let cache_root = dir.path().join("cache");
        let xorb_path = write_xorb_cache_candidate(&cache_root, &xorb_hash, &xorb_bytes)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                cache_roots: vec![cache_root],
                fsck_jsonl: vec![fsck_jsonl.clone()],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.inventory_only, 1);
        assert!(
            plan.sources
                .iter()
                .any(|source| source == &format!("fsck-jsonl:{}", fsck_jsonl.display()))
        );
        let xorb_item = plan
            .items
            .iter()
            .find(|item| item.item_kind == RecoverItemKind::Xorb)
            .expect("xorb item");
        assert_eq!(xorb_item.path, format!("xorb:{}", xorb_hash.hex()));
        assert_eq!(xorb_item.state, RecoverItemState::Repairable);
        let candidate = xorb_item.candidate.as_ref().expect("xorb candidate");
        assert_eq!(candidate.source_kind, RecoverCandidateKind::LocalCache);
        assert_eq!(candidate.source_path, xorb_path.display().to_string());
        let xorb_metadata = xorb_item.metadata.as_ref().expect("xorb metadata");
        assert_eq!(
            xorb_metadata.get("source").map(String::as_str),
            Some("fsck_jsonl")
        );
        let shard_item = plan
            .items
            .iter()
            .find(|item| item.item_kind == RecoverItemKind::Shard)
            .expect("shard item");
        assert_eq!(shard_item.path, format!("shard:{shard_hash}"));
        assert_eq!(shard_item.state, RecoverItemState::InventoryOnly);
        Ok(())
    }

    #[test]
    fn plan_marks_corrupt_xorb_candidate_unrecoverable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let (xorb_hash, _xorb_bytes) = test_xorb(b"recoverable xorb body")?;
        let xorb_list = dir.path().join("xorbs.jsonl");
        std::fs::write(&xorb_list, format!("{}\n", xorb_hash.hex()))?;
        let cache_root = dir.path().join("cache");
        let hex = xorb_hash.hex();
        let corrupt = cache_root.join("xorbs").join(&hex[..2]).join(&hex);
        std::fs::create_dir_all(corrupt.parent().expect("corrupt xorb parent"))?;
        std::fs::write(&corrupt, b"not a valid xorb")?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                cache_roots: vec![cache_root],
                xorb_lists: vec![xorb_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 0);
        assert_eq!(plan.unrecoverable, 1);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Xorb);
        assert_eq!(plan.items[0].state, RecoverItemState::Unrecoverable);
        assert!(plan.items[0].candidate.is_none());
        Ok(())
    }

    #[test]
    fn plan_includes_pack_list_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let pack_id = "c".repeat(64);
        let pack_list = dir.path().join("packs.jsonl");
        std::fs::write(
            &pack_list,
            serialize_pack_list(&[PackManifestEntry {
                pack_id: pack_id.clone(),
                size: 42,
                content_hash: pack_id.clone(),
                ref_tips: vec!["1".repeat(40)],
                object_count: 7,
            }]),
        )?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                pack_lists: vec![pack_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.inventory_only, 1);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Pack);
        assert_eq!(plan.items[0].path, format!("pack:{pack_id}"));
        assert_eq!(plan.items[0].file_hash, format!("b3:{pack_id}"));
        assert_eq!(plan.items[0].size, 42);
        assert_eq!(plan.items[0].state, RecoverItemState::InventoryOnly);
        assert_eq!(plan.items[0].action, "restore_pack_object_from_replica");
        let metadata = plan.items[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("content_hash").map(String::as_str),
            Some(format!("b3:{pack_id}").as_str())
        );
        assert_eq!(metadata.get("object_count").map(String::as_str), Some("7"));
        Ok(())
    }

    #[test]
    fn plan_marks_pack_backup_candidate_repairable() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let pack_bytes = valid_small_pack_bytes()?;
        let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let pack_list = dir.path().join("packs.jsonl");
        std::fs::write(
            &pack_list,
            serialize_pack_list(&[PackManifestEntry {
                pack_id: pack_id.clone(),
                size: pack_bytes.len() as u64,
                content_hash: pack_id.clone(),
                ref_tips: Vec::new(),
                object_count: 1,
            }]),
        )?;
        let backup = dir.path().join("backup");
        let pack_path = backup.join("packs").join(format!("pack-{pack_id}.pack"));
        std::fs::create_dir_all(pack_path.parent().expect("pack parent"))?;
        std::fs::write(&pack_path, &pack_bytes)?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                pack_lists: vec![pack_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.repairable, 1);
        assert_eq!(plan.inventory_only, 0);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::Pack);
        assert_eq!(plan.items[0].path, format!("pack:{pack_id}"));
        assert_eq!(plan.items[0].file_hash, format!("b3:{pack_id}"));
        assert_eq!(plan.items[0].state, RecoverItemState::Repairable);
        assert_eq!(plan.items[0].action, "restore_pack_object");
        let candidate = plan.items[0].candidate.as_ref().expect("candidate");
        assert_eq!(candidate.source_path, pack_path.display().to_string());
        assert_eq!(candidate.size, pack_bytes.len() as u64);
        Ok(())
    }

    #[test]
    fn verified_pack_candidate_synthesizes_metadata_from_plan() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let pack_bytes = valid_small_pack_bytes()?;
        let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let pack_list = dir.path().join("packs.jsonl");
        std::fs::write(
            &pack_list,
            serialize_pack_list(&[PackManifestEntry {
                pack_id: pack_id.clone(),
                size: pack_bytes.len() as u64,
                content_hash: pack_id.clone(),
                ref_tips: Vec::new(),
                object_count: 1,
            }]),
        )?;
        let pack_path = dir.path().join(format!("pack-{pack_id}.pack"));
        std::fs::write(&pack_path, &pack_bytes)?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![pack_path],
                pack_lists: vec![pack_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        let verified = verified_pack_restore_candidate(&plan.items[0])?.expect("verified pack");
        let metadata: crab_metadata::pack_metadata::PackMetadata =
            serde_json::from_slice(&verified.metadata)?;

        assert_eq!(verified.size, pack_bytes.len() as u64);
        assert_eq!(metadata.pack_id, pack_id);
        assert_eq!(metadata.object_count, 1);
        assert!(metadata.ref_tips.is_empty());
        Ok(())
    }

    #[test]
    fn plan_includes_file_index_inventory() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let file_hash = "e".repeat(64);
        let shard_hash = "f".repeat(64);
        let file_index = dir.path().join("file-index.jsonl");
        std::fs::write(
            &file_index,
            serde_json::json!({
                "file_hash": file_hash,
                "shard_hash": shard_hash,
                "path": "dataset/model.bin",
                "size": 12_u64,
            })
            .to_string(),
        )?;

        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                file_indexes: vec![file_index],
                ..RecoverySourceSpec::default()
            },
        )?;

        assert_eq!(plan.inventory_only, 1);
        assert_eq!(plan.items[0].item_kind, RecoverItemKind::FileIndex);
        assert_eq!(plan.items[0].path, "dataset/model.bin");
        assert_eq!(plan.items[0].file_hash, format!("b3:{file_hash}"));
        assert_eq!(plan.items[0].size, 12);
        assert_eq!(plan.items[0].state, RecoverItemState::InventoryOnly);
        assert_eq!(
            plan.items[0].action,
            "rebuild_file_index_after_shard_restore"
        );
        let metadata = plan.items[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("shard_hash").map(String::as_str),
            Some(format!("b3:{shard_hash}").as_str())
        );
        Ok(())
    }

    #[test]
    fn apply_reports_metadata_inventory_items_without_remote_repair() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let shard_hash = "a".repeat(64);
        let shard_list = dir.path().join("shards.jsonl");
        std::fs::write(
            &shard_list,
            serialize_shard_list(std::slice::from_ref(&shard_hash)),
        )?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                shard_lists: vec![shard_list],
                ..RecoverySourceSpec::default()
            },
        )?;

        let result = apply_plan(&plan, &dir.path().join("restore"))?;

        assert_eq!(result.restored, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.inventory_only, 1);
        assert_eq!(result.items[0].state, RecoverApplyState::InventoryOnly);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some(
                "shard reference recorded; restore the shard object from backup or re-push original file bytes"
            )
        );
        Ok(())
    }

    #[test]
    fn apply_reports_file_index_inventory_rebuild_action() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let file_hash = "e".repeat(64);
        let shard_hash = "f".repeat(64);
        let file_index = dir.path().join("file-index.jsonl");
        std::fs::write(
            &file_index,
            serde_json::json!({
                "file_hash": file_hash,
                "shard_hash": shard_hash,
                "path": "dataset/model.bin",
                "size": 12_u64,
            })
            .to_string(),
        )?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                file_indexes: vec![file_index],
                ..RecoverySourceSpec::default()
            },
        )?;

        let result = apply_plan(&plan, &dir.path().join("restore"))?;

        assert_eq!(
            result.items[0].message.as_deref(),
            Some(
                "file-index mapping recorded; after shard objects are present, rerun with `--rebuild-file-index` or use `crab metadb rebuild --db file_index`"
            )
        );
        Ok(())
    }

    #[test]
    fn apply_marks_verified_file_index_metadata_repaired() -> TestResult {
        let dir = tempfile::tempdir()?;
        let plan = file_index_plan_for_test(dir.path())?;

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[Some(RecoverApplyState::MetadataRepaired)],
            None,
            None,
            None,
            None,
        )?;

        assert_eq!(result.metadata_repaired, 1);
        assert_eq!(result.inventory_only, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.items[0].state, RecoverApplyState::MetadataRepaired);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some("file-index mapping verified in file_index_db")
        );
        Ok(())
    }

    #[test]
    fn apply_marks_unverified_file_index_metadata_repair_failed() -> TestResult {
        let dir = tempfile::tempdir()?;
        let plan = file_index_plan_for_test(dir.path())?;

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[Some(RecoverApplyState::Failed)],
            None,
            None,
            None,
            None,
        )?;

        assert_eq!(result.metadata_repaired, 0);
        assert_eq!(result.inventory_only, 0);
        assert_eq!(result.failed, 1);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some("file-index mapping was not rebuilt from durable shard objects")
        );
        Ok(())
    }

    #[test]
    fn apply_reports_shard_repaired_items() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let shard_bytes = b"remote shard repair body";
        let shard_hash = blake3::hash(shard_bytes).to_hex().to_string();
        let shard_list = dir.path().join("shards.jsonl");
        std::fs::write(
            &shard_list,
            serialize_shard_list(std::slice::from_ref(&shard_hash)),
        )?;
        let backup = dir.path().join("backup");
        let shard_path = backup.join(".crab").join("shards").join(&shard_hash);
        std::fs::create_dir_all(shard_path.parent().expect("shard parent"))?;
        std::fs::write(shard_path, shard_bytes)?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                shard_lists: vec![shard_list],
                ..RecoverySourceSpec::default()
            },
        )?;
        let shard_repair = ShardRestoreResult {
            item_repaired: vec![true],
            shards: 1,
            bytes: shard_bytes.len() as u64,
        };

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[None],
            Some(&shard_repair),
            None,
            None,
            None,
        )?;

        assert_eq!(result.shards_repaired, 1);
        assert_eq!(result.shard_bytes_repaired, shard_bytes.len() as u64);
        assert_eq!(result.inventory_only, 0);
        assert_eq!(result.items[0].state, RecoverApplyState::ShardRepaired);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some("shard object restored to remote storage")
        );
        Ok(())
    }

    #[test]
    fn apply_reports_xorb_repaired_items() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let (xorb_hash, xorb_bytes) = test_xorb(b"remote xorb repair body")?;
        let xorb_list = dir.path().join("xorbs.jsonl");
        std::fs::write(&xorb_list, format!("{}\n", xorb_hash.hex()))?;
        let backup = dir.path().join("backup");
        let xorb_path = backup.join(".crab").join("xorbs").join(xorb_hash.hex());
        std::fs::create_dir_all(xorb_path.parent().expect("xorb parent"))?;
        std::fs::write(xorb_path, &xorb_bytes)?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                xorb_lists: vec![xorb_list],
                ..RecoverySourceSpec::default()
            },
        )?;
        let xorb_repair = XorbRestoreResult {
            item_repaired: vec![true],
            xorbs: 1,
            bytes: xorb_bytes.len() as u64,
        };

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[None],
            None,
            Some(&xorb_repair),
            None,
            None,
        )?;

        assert_eq!(result.xorbs_repaired, 1);
        assert_eq!(result.xorb_bytes_repaired, xorb_bytes.len() as u64);
        assert_eq!(result.inventory_only, 0);
        assert_eq!(result.items[0].state, RecoverApplyState::XorbRepaired);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some("xorb object restored to remote storage")
        );
        Ok(())
    }

    #[test]
    fn verified_xorb_restore_candidate_rejects_changed_candidate() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let (xorb_hash, xorb_bytes) = test_xorb(b"remote xorb repair body")?;
        let xorb_list = dir.path().join("xorbs.jsonl");
        std::fs::write(&xorb_list, format!("{}\n", xorb_hash.hex()))?;
        let backup = dir.path().join("backup");
        let xorb_path = backup.join(".crab").join("xorbs").join(xorb_hash.hex());
        std::fs::create_dir_all(xorb_path.parent().expect("xorb parent"))?;
        std::fs::write(&xorb_path, &xorb_bytes)?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                xorb_lists: vec![xorb_list],
                ..RecoverySourceSpec::default()
            },
        )?;
        std::fs::write(&xorb_path, b"changed bytes")?;

        let err = verified_xorb_restore_candidate(&plan.items[0])
            .expect_err("changed xorb candidate must fail apply-time verification");

        assert!(matches!(err, CrabError::Configuration { .. }));
        Ok(())
    }

    #[test]
    fn apply_reports_pack_repaired_items() -> TestResult {
        let dir = tempfile::tempdir()?;
        let manifest = manifest_for_many(&[]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let pack_bytes = valid_small_pack_bytes()?;
        let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let pack_list = dir.path().join("packs.jsonl");
        std::fs::write(
            &pack_list,
            serialize_pack_list(&[PackManifestEntry {
                pack_id: pack_id.clone(),
                size: pack_bytes.len() as u64,
                content_hash: pack_id,
                ref_tips: Vec::new(),
                object_count: 1,
            }]),
        )?;
        let backup = dir.path().join("backup");
        let pack_path = backup
            .join("packs")
            .join(format!("pack-{}.pack", blake3::hash(&pack_bytes).to_hex()));
        std::fs::create_dir_all(pack_path.parent().expect("pack parent"))?;
        std::fs::write(pack_path, &pack_bytes)?;
        let plan = build_plan_with_sources(
            &manifest_path,
            &RecoverySourceSpec {
                explicit_paths: vec![backup],
                pack_lists: vec![pack_list],
                ..RecoverySourceSpec::default()
            },
        )?;
        let pack_repair = PackRestoreResult {
            item_repaired: vec![true],
            packs: 1,
            bytes: pack_bytes.len() as u64,
        };

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[None],
            None,
            None,
            Some(&pack_repair),
            None,
        )?;

        assert_eq!(result.packs_repaired, 1);
        assert_eq!(result.pack_bytes_repaired, pack_bytes.len() as u64);
        assert_eq!(result.inventory_only, 0);
        assert_eq!(result.items[0].state, RecoverApplyState::PackRepaired);
        assert_eq!(
            result.items[0].message.as_deref(),
            Some("pack object restored to remote storage")
        );
        Ok(())
    }

    #[test]
    fn remote_repair_refspecs_select_manifest_branch_refs() -> TestResult {
        let commit = "0123456789012345678901234567890123456789";
        let mut manifest = manifest_for_many(&[]);
        manifest.revision.commit = commit.to_owned();
        manifest.selected_refs.insert(
            "refs/heads/main".to_owned(),
            ReleaseRefTarget {
                oid: commit.to_owned(),
                peeled_oid: None,
            },
        );
        manifest.selected_refs.insert(
            "refs/tags/v1".to_owned(),
            ReleaseRefTarget {
                oid: "a".repeat(40),
                peeled_oid: Some(commit.to_owned()),
            },
        );
        let args = RecoverApplyArgs {
            plan: PathBuf::from("plan.json"),
            restore_to: PathBuf::from("restore"),
            rebuild_file_index: false,
            restore_shards: false,
            restore_xorbs: false,
            restore_packs: false,
            repair_remote: true,
            remote: None,
            repair_refspec: Vec::new(),
            json: false,
        };

        let refspecs = remote_repair_refspecs(&args, &manifest)?;

        assert_eq!(refspecs, vec!["refs/heads/main:refs/heads/main"]);
        Ok(())
    }

    #[test]
    fn remote_repair_refspecs_allow_explicit_branch_override() -> TestResult {
        let manifest = manifest_for_many(&[]);
        let args = RecoverApplyArgs {
            plan: PathBuf::from("plan.json"),
            restore_to: PathBuf::from("restore"),
            rebuild_file_index: false,
            restore_shards: false,
            restore_xorbs: false,
            restore_packs: false,
            repair_remote: true,
            remote: Some("backup".to_owned()),
            repair_refspec: vec!["repair:refs/heads/main".to_owned()],
            json: false,
        };

        let refspecs = remote_repair_refspecs(&args, &manifest)?;

        assert_eq!(refspecs, vec!["repair:refs/heads/main"]);
        Ok(())
    }

    #[test]
    fn normalized_repair_refspec_rejects_tags() {
        let err = normalized_repair_refspec_src("refs/tags/v1:refs/tags/v1")
            .expect_err("tag refspecs are not a safe automatic repair source");

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn apply_reports_remote_repaired_items() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        std::fs::create_dir(&source)?;
        let bytes = b"original model";
        std::fs::write(source.join("model.bin"), bytes)?;
        let manifest = manifest_for("model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let plan = build_plan(&manifest_path, &[source])?;
        let remote = RemoteRepairResult {
            item_repaired: vec![true],
            files: 1,
            bytes: bytes.len() as u64,
            refspecs: vec!["refs/heads/main:refs/heads/main".to_owned()],
        };

        let result = apply_plan_with_repairs(
            &plan,
            &dir.path().join("restore"),
            &[None],
            None,
            None,
            None,
            Some(&remote),
        )?;

        assert_eq!(result.remote_repaired, 1);
        assert_eq!(result.remote_bytes_repaired, bytes.len() as u64);
        assert_eq!(
            result.remote_refspecs,
            vec!["refs/heads/main:refs/heads/main"]
        );
        assert!(result.items[0].remote_repaired);
        Ok(())
    }

    #[test]
    fn apply_restores_verified_candidate_and_is_idempotent() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let restore = dir.path().join("restore");
        std::fs::create_dir(&source)?;
        let bytes = b"original model";
        std::fs::write(source.join("model.bin"), bytes)?;
        let manifest = manifest_for("nested/model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let plan = build_plan(&manifest_path, &[source])?;

        let first = apply_plan(&plan, &restore)?;
        let second = apply_plan(&plan, &restore)?;

        assert_eq!(first.restored, 1);
        assert_eq!(first.failed, 0);
        assert_eq!(first.inventory_only, 0);
        assert_eq!(second.restored, 0);
        assert_eq!(second.already_present, 1);
        assert_eq!(second.inventory_only, 0);
        assert_eq!(std::fs::read(restore.join("nested/model.bin"))?, bytes);
        Ok(())
    }

    #[test]
    fn apply_rejects_restore_root_that_is_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let restore = dir.path().join("restore-file");
        std::fs::write(&restore, b"not a directory")?;
        let plan = RecoverPlanPayload {
            plan_id: "plan".to_owned(),
            manifest_path: "release.json".to_owned(),
            sources: Vec::new(),
            repairable: 0,
            unrecoverable: 0,
            inventory_only: 0,
            items: Vec::new(),
        };

        let err = apply_plan(&plan, &restore).expect_err("file restore root must be rejected");

        assert!(matches!(err, CrabError::Configuration { .. }));
        Ok(())
    }

    #[test]
    fn apply_skips_candidate_that_changes_after_plan() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let restore = dir.path().join("restore");
        std::fs::create_dir(&source)?;
        let bytes = b"original model";
        let candidate = source.join("model.bin");
        std::fs::write(&candidate, bytes)?;
        let manifest = manifest_for("model.bin", bytes);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let plan = build_plan(&manifest_path, &[source])?;
        std::fs::write(&candidate, b"changed bytes!")?;

        let result = apply_plan(&plan, &restore)?;

        assert_eq!(result.failed, 1);
        assert_eq!(result.items[0].state, RecoverApplyState::Failed);
        assert!(!restore.join("model.bin").exists());
        Ok(())
    }

    #[test]
    fn apply_continues_after_partial_restore() -> TestResult {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let restore = dir.path().join("restore");
        std::fs::create_dir(&source)?;
        std::fs::create_dir(&restore)?;
        let first = b"first";
        let second = b"second";
        std::fs::write(source.join("one.bin"), first)?;
        std::fs::write(source.join("two.bin"), second)?;
        std::fs::write(restore.join("one.bin"), first)?;
        let manifest = manifest_for_many(&[
            ("one.bin", first.as_slice()),
            ("two.bin", second.as_slice()),
        ]);
        let manifest_path = dir.path().join("release.json");
        write_manifest(&manifest_path, &manifest)?;
        let plan = build_plan(&manifest_path, &[source])?;

        let result = apply_plan(&plan, &restore)?;

        assert_eq!(result.already_present, 1);
        assert_eq!(result.restored, 1);
        assert_eq!(std::fs::read(restore.join("two.bin"))?, second);
        Ok(())
    }
}
