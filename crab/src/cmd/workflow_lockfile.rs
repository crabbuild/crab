//! `crab workflow lockfile resolve` — git-merge-conflict resolution
//! for `crab.lock`.
//!
//! Accepts the three strategies called out in R5:
//! `--ours`, `--theirs`, `--recompute` (default). The flags are
//! mutually exclusive via a clap `ArgGroup`; omitting all three
//! selects `--recompute`, which re-derives the lockfile from both
//! sides so the output is byte-identical regardless of which side
//! of the conflict invoked the command.
//!
//! On success the resolved lockfile is written back to disk (same
//! canonical serializer as a normal `crab run` write) and a
//! `workflow.lockfile_resolve` envelope is emitted describing which
//! stages were kept or dropped. The caller exits 0 on a clean
//! resolve, propagates the usual `CrabError` exit mapping on
//! failure — in particular [`CrabError::LockfileMergeConflict`]
//! for a file that isn't actually in conflict form.

use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser};
use crab_workflow::{Lockfile, ResolveOutcome, ResolveStrategy, lockfile};
use serde::Serialize;
use tracing::info;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};

/// Structured-output schema label for `crab workflow lockfile resolve`.
pub const WORKFLOW_LOCKFILE_RESOLVE_SCHEMA: &str = "workflow.lockfile_resolve";

/// Clap args for `crab workflow lockfile resolve`.
///
/// The three strategy flags live in an `ArgGroup` so clap refuses
/// `--ours --theirs` etc. at parse time. No `required = true` on the
/// group — omitting all three falls back to `Recompute`, matching
/// the R5 default.
#[derive(Debug, Clone, Parser)]
#[command(group(
    ArgGroup::new("strategy")
        .args(["ours", "theirs", "recompute"])
        .multiple(false)
        .required(false),
))]
pub struct ResolveArgs {
    /// Pick the "ours" (HEAD) side wholesale for every conflicted
    /// stage block.
    #[arg(long, default_value_t = false)]
    pub ours: bool,

    /// Pick the "theirs" (incoming) side wholesale for every
    /// conflicted stage block.
    #[arg(long, default_value_t = false)]
    pub theirs: bool,

    /// Recompute the resolved lockfile from both sides (default).
    /// Stages present on only one side are kept verbatim; stages
    /// that disagree between sides are dropped so the next
    /// `crab run` re-runs them from working-tree state.
    #[arg(long, default_value_t = false)]
    pub recompute: bool,

    /// Path to the lockfile to resolve. Defaults to `crab.lock`
    /// at the repo root.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl ResolveArgs {
    /// Fold the three mutually-exclusive flags into a
    /// [`ResolveStrategy`]. Omitting all three returns the default.
    pub fn strategy(&self) -> ResolveStrategy {
        match (self.ours, self.theirs, self.recompute) {
            (true, false, false) => ResolveStrategy::Ours,
            (false, true, false) => ResolveStrategy::Theirs,
            // Explicit --recompute or no flag at all → Recompute.
            _ => ResolveStrategy::Recompute,
        }
    }

    /// Output mode derived from the `--json` flag.
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// JSON payload emitted under `--json`. Mirrors the fields the
/// design doc calls for: `strategy`, `stages_kept`, `stages_dropped`,
/// and the `path` the resolved lockfile was written to.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LockfileResolvePayload {
    pub strategy: String,
    pub path: PathBuf,
    pub stages_kept: Vec<String>,
    pub stages_dropped: Vec<String>,
}

/// CLI entry point. Dispatches from `main.rs`.
pub fn exec(args: ResolveArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    resolve_in(&args, &cwd)
}

/// Testable variant that accepts an explicit `repo_root`.
pub fn resolve_in(args: &ResolveArgs, repo_root: &Path) -> Result<()> {
    let lockfile_path = args
        .path
        .clone()
        .unwrap_or_else(|| repo_root.join("crab.lock"));
    let strategy = args.strategy();

    let outcome = lockfile::resolve(&lockfile_path, strategy, repo_root)?;
    write_resolved(&lockfile_path, &outcome.lockfile)?;
    emit(args.output_mode(), &lockfile_path, &outcome);
    Ok(())
}

/// Convenience wrapper around [`Lockfile::save`] so callers outside
/// this module don't have to import the method directly.
pub fn write_resolved(path: &Path, resolved: &Lockfile) -> Result<()> {
    resolved.save(path)?;
    Ok(())
}

fn emit(mode: OutputMode, path: &Path, outcome: &ResolveOutcome) {
    let payload = LockfileResolvePayload {
        strategy: outcome.strategy.as_str().to_owned(),
        path: path.to_path_buf(),
        stages_kept: outcome
            .stages_kept
            .iter()
            .map(|n| n.as_str().to_owned())
            .collect(),
        stages_dropped: outcome
            .stages_dropped
            .iter()
            .map(|n| n.as_str().to_owned())
            .collect(),
    };

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(WORKFLOW_LOCKFILE_RESOLVE_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            info!(
                strategy = payload.strategy,
                path = %path.display(),
                stages_kept = payload.stages_kept.len(),
                stages_dropped = payload.stages_dropped.len(),
                "workflow: crab.lock resolved"
            );
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn base_args() -> ResolveArgs {
        ResolveArgs {
            ours: false,
            theirs: false,
            recompute: false,
            path: None,
            json: false,
        }
    }

    #[test]
    fn default_strategy_is_recompute() {
        let args = base_args();
        assert_eq!(args.strategy(), ResolveStrategy::Recompute);
    }

    #[test]
    fn explicit_flags_select_corresponding_strategy() {
        let mut a = base_args();
        a.ours = true;
        assert_eq!(a.strategy(), ResolveStrategy::Ours);
        let mut a = base_args();
        a.theirs = true;
        assert_eq!(a.strategy(), ResolveStrategy::Theirs);
        let mut a = base_args();
        a.recompute = true;
        assert_eq!(a.strategy(), ResolveStrategy::Recompute);
    }

    #[test]
    fn resolve_in_writes_resolved_lockfile() {
        // Seed a conflicted lockfile on disk, invoke resolve_in, and
        // verify the file comes back clean and reloadable.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crab.lock");
        let conflicted = b"<<<<<<< HEAD\n\
crab_hash_algo: \"crab.stage.v1\"\n\
schema_version: 1\n\
stages: {}\n\
=======\n\
crab_hash_algo: \"crab.stage.v1\"\n\
schema_version: 1\n\
stages: {}\n\
>>>>>>> theirs\n";
        fs::write(&path, conflicted).unwrap();

        let mut args = base_args();
        args.path = Some(path.clone());
        resolve_in(&args, tmp.path()).unwrap();

        // Post-resolve, the file parses as a valid lockfile.
        let reloaded = Lockfile::load(&path).unwrap();
        assert_eq!(reloaded, Lockfile::default());
    }

    #[test]
    fn resolve_in_reports_error_on_clean_file() {
        // Running resolve on a non-conflicted lockfile is a user
        // error — we surface the canonical merge-conflict variant
        // so the error-code system can map it consistently.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("crab.lock");
        Lockfile::default().save(&path).unwrap();
        let mut args = base_args();
        args.path = Some(path);
        let err = resolve_in(&args, tmp.path()).expect_err("clean file must fail");
        assert!(matches!(err, CrabError::LockfileMergeConflict { .. }));
    }

    // ----- rewrite_config_for_split -----

    #[test]
    fn rewrite_config_empty_body_creates_workflow_section() {
        let out = rewrite_config_for_split("");
        assert_eq!(out, "[workflow]\nlockfile = \"split\"\n");
    }

    #[test]
    fn rewrite_config_no_workflow_section_appends_one() {
        let input = "[storage]\ns3_bucket = \"my-bucket\"\n";
        let out = rewrite_config_for_split(input);
        assert!(out.contains("[storage]"));
        assert!(out.contains("[workflow]\nlockfile = \"split\""));
    }

    #[test]
    fn rewrite_config_existing_workflow_without_lockfile_injects_key() {
        let input = "[workflow]\nenabled = true\n";
        let out = rewrite_config_for_split(input);
        // Injected as the first line inside the section — before
        // `enabled = true`. That keeps the `[workflow]` table
        // contiguous and doesn't create a duplicate header.
        let lockfile_pos = out.find("lockfile = \"split\"").unwrap();
        let enabled_pos = out.find("enabled = true").unwrap();
        assert!(
            lockfile_pos < enabled_pos,
            "key injected before existing entries"
        );
        // No duplicate headers introduced.
        assert_eq!(out.matches("[workflow]").count(), 1);
    }

    #[test]
    fn rewrite_config_existing_lockfile_key_is_overwritten() {
        let input = "[workflow]\nlockfile = \"single\"\nenabled = true\n";
        let out = rewrite_config_for_split(input);
        assert!(out.contains("lockfile = \"split\""));
        assert!(!out.contains("lockfile = \"single\""));
        assert!(out.contains("enabled = true"));
    }

    #[test]
    fn rewrite_config_preserves_existing_content() {
        let input = "# top-level comment\n[storage]\ns3_bucket = \"bucket\"\n\n\
                     [workflow]\nenabled = true\n";
        let out = rewrite_config_for_split(input);
        assert!(out.contains("# top-level comment"));
        assert!(out.contains("s3_bucket = \"bucket\""));
        assert!(out.contains("lockfile = \"split\""));
    }

    #[test]
    fn rewrite_config_result_parses_as_valid_toml() {
        // The rewriter must produce TOML the parser accepts. Regression
        // guard for the earlier "append duplicate [workflow] header" bug
        // that produced unparseable output.
        let input = "[workflow]\nenabled = true\ndiscover = \"recursive\"\n";
        let out = rewrite_config_for_split(input);
        let parsed: toml::Value = toml::from_str(&out).expect("must be valid TOML");
        let wf = parsed
            .get("workflow")
            .and_then(|v| v.as_table())
            .expect("workflow table");
        assert_eq!(wf.get("lockfile").and_then(|v| v.as_str()), Some("split"));
        assert_eq!(wf.get("enabled").and_then(|v| v.as_bool()), Some(true));
    }
}

// ---------------------------------------------------------------------------
// `crab workflow lockfile split` — migrate single → split layout.
// ---------------------------------------------------------------------------

/// Structured-output schema label for `crab workflow lockfile split`.
pub const WORKFLOW_LOCKFILE_SPLIT_SCHEMA: &str = "workflow.lockfile_split";

/// Clap args for `crab workflow lockfile split`.
///
/// Reads the repo-root `crab.lock` (if any), partitions its stages
/// by the workflow file that declared them, writes per-workflow
/// lockfiles alongside each yaml, and by default removes the
/// monolithic file afterwards.
///
/// Discovery uses the recursive mode so both `*.workflow.yaml` and
/// nested `crab.yaml` files are considered — the split layout
/// fundamentally assumes multi-file workflows, and running in root
/// mode would be a no-op.
#[derive(Debug, Clone, Parser)]
pub struct SplitArgs {
    /// Preview the split without writing anything. Prints the
    /// per-file breakdown to stdout and exits without touching the
    /// working tree.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Keep the monolithic `crab.lock` after writing the split
    /// files. By default the source file is removed so the working
    /// tree reflects the new layout exactly. Users on mixed
    /// single/split repos (stages declared in both `crab.yaml`
    /// *and* `*.workflow.yaml`) may want `--keep` so the root file
    /// continues to carry root-yaml stages.
    #[arg(long, default_value_t = false)]
    pub keep: bool,

    /// Also flip `[workflow] lockfile = "split"` in the repo
    /// `.crab/local.toml` so subsequent `crab run` invocations
    /// use the new layout automatically.
    #[arg(long, default_value_t = false)]
    pub update_config: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl SplitArgs {
    /// Output mode derived from the `--json` flag.
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// JSON payload emitted under `--json`. One `files` entry per
/// per-workflow lockfile that was (or would be) written, with the
/// stage count for quick visual verification.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LockfileSplitPayload {
    pub dry_run: bool,
    pub removed_monolithic: bool,
    pub files: Vec<LockfileSplitFile>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LockfileSplitFile {
    pub path: PathBuf,
    pub stage_count: usize,
}

/// CLI entry point for `crab workflow lockfile split`. Dispatches
/// from `main.rs`.
pub fn exec_split(args: SplitArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    split_in(&args, &cwd)
}

/// Testable variant that accepts an explicit `repo_root`.
pub fn split_in(args: &SplitArgs, repo_root: &Path) -> Result<()> {
    use crate::workflow::discover;
    use crate::workflow::lockfile_split;

    // Recursive discovery: the split layout doesn't make sense in
    // root-only mode. We explicitly pass Recursive here regardless
    // of config so the user gets a consistent migration experience
    // even when their repo is still declared as root-mode.
    let workflow_files = discover::discover(repo_root, discover::DiscoverMode::Recursive)?;
    if workflow_files.is_empty() {
        return Err(CrabError::Configuration {
            key:
                "no crab.yaml or *.workflow.yaml found — split requires at least one workflow file"
                    .into(),
            origin: "cli".into(),
        });
    }

    // Provenance is derived from the same parse + merge pipeline
    // the runtime uses, so the split writes stages to exactly the
    // same lockfiles a fresh run would have produced.
    let (_workflow, provenance) = discover::parse_all_with_provenance(repo_root, &workflow_files)?;

    if args.dry_run {
        // Load the monolithic file into memory, partition, summarize.
        let monolithic = Lockfile::load(&repo_root.join("crab.lock"))?;
        let partitions =
            lockfile_split::partition_stages(repo_root, &workflow_files, &provenance, &monolithic);
        let files: Vec<LockfileSplitFile> = partitions
            .into_iter()
            .filter(|(_, lock)| !lock.stages.is_empty())
            .map(|(path, lock)| LockfileSplitFile {
                path,
                stage_count: lock.stages.len(),
            })
            .collect();
        emit_split(args.output_mode(), true, false, &files);
        return Ok(());
    }

    let summary = lockfile_split::migrate_single_to_split(
        repo_root,
        &workflow_files,
        &provenance,
        !args.keep,
    )?;

    let removed_monolithic = !args.keep && !summary.is_empty();
    let files: Vec<LockfileSplitFile> = summary
        .into_iter()
        .map(|(path, count)| LockfileSplitFile {
            path,
            stage_count: count,
        })
        .collect();

    if args.update_config {
        update_config_to_split(repo_root)?;
    }

    emit_split(args.output_mode(), false, removed_monolithic, &files);
    Ok(())
}

/// Flip `[workflow] lockfile = "split"` in `.crab/local.toml`.
///
/// Idempotent: if the key already holds `"split"` the file isn't
/// rewritten. Preserves surrounding comments and formatting — we
/// edit in place rather than re-serialize so the user's TOML stays
/// close to how they wrote it.
///
/// Three cases:
/// 1. File missing → create with `[workflow]\nlockfile = "split"\n`.
/// 2. `[workflow]` table exists, `lockfile =` absent → inject the
///    key as the first line of the table.
/// 3. `lockfile =` already present → rewrite the value to `"split"`.
fn update_config_to_split(repo_root: &Path) -> Result<()> {
    let config_path = repo_root.join(".crab").join("local.toml");
    let existing = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CrabError::Io(e)),
    };

    // Already set correctly — no-op.
    if existing.contains(r#"lockfile = "split""#) || existing.contains(r#"lockfile="split""#) {
        return Ok(());
    }

    let new_body = rewrite_config_for_split(&existing);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    std::fs::write(&config_path, new_body).map_err(CrabError::Io)?;
    Ok(())
}

/// Pure function that rewrites the TOML body to set
/// `[workflow] lockfile = "split"`. Isolated from I/O so we can
/// unit-test every case without a scratch directory.
fn rewrite_config_for_split(existing: &str) -> String {
    // Case 3: an existing `lockfile =` line gets its value rewritten.
    if let Some(new_body) = rewrite_existing_lockfile_key(existing) {
        return new_body;
    }

    // Case 2: a `[workflow]` section exists → prepend the key to it.
    if let Some(new_body) = inject_into_workflow_section(existing) {
        return new_body;
    }

    // Case 1: no `[workflow]` section → append one.
    if existing.is_empty() {
        "[workflow]\nlockfile = \"split\"\n".to_owned()
    } else if existing.ends_with('\n') {
        format!("{existing}\n[workflow]\nlockfile = \"split\"\n")
    } else {
        format!("{existing}\n\n[workflow]\nlockfile = \"split\"\n")
    }
}

/// If the body has a `lockfile = "..."` line inside a `[workflow]`
/// table, rewrite it to `"split"` and return the new body. Returns
/// `None` if no such line exists.
fn rewrite_existing_lockfile_key(body: &str) -> Option<String> {
    let mut in_workflow = false;
    let mut found = false;
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_workflow = trimmed.starts_with("[workflow]");
        }
        if in_workflow && !found && trimmed.starts_with("lockfile") && trimmed.contains('=') {
            // Preserve the leading indentation the user had.
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            out.push_str(&format!("{indent}lockfile = \"split\"\n"));
            found = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if found { Some(out) } else { None }
}

/// If the body has a `[workflow]` section without a `lockfile =`
/// key, inject the key as the first line of the section.
fn inject_into_workflow_section(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len() + 32);
    let mut injected = false;
    for line in body.lines() {
        out.push_str(line);
        out.push('\n');
        if !injected && line.trim_start().starts_with("[workflow]") {
            out.push_str("lockfile = \"split\"\n");
            injected = true;
        }
    }
    if injected { Some(out) } else { None }
}

fn emit_split(
    mode: OutputMode,
    dry_run: bool,
    removed_monolithic: bool,
    files: &[LockfileSplitFile],
) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            let payload = LockfileSplitPayload {
                dry_run,
                removed_monolithic,
                files: files.to_vec(),
            };
            emit_json(WORKFLOW_LOCKFILE_SPLIT_SCHEMA, "1.0", payload);
        }
        OutputMode::Text => {
            if files.is_empty() {
                info!(dry_run, "workflow: crab.lock has no stages to split");
                return;
            }
            for f in files {
                info!(
                    path = %f.path.display(),
                    stages = f.stage_count,
                    dry_run,
                    "workflow: lockfile partition"
                );
            }
            if removed_monolithic {
                info!("workflow: removed monolithic crab.lock");
            }
        }
    }
}

// Manual Clone impls: auto-derive would need every field to be Clone,
// and PathBuf / String are already Clone — but we use the payload for
// JSON emission only, and the JSON emitter takes owned values. This
// block makes the payload reusable in other contexts (e.g. embedding
// in a larger envelope) without forcing every call site to rebuild.
impl Clone for LockfileSplitFile {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            stage_count: self.stage_count,
        }
    }
}
