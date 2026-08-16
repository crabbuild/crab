//! Workflow-scoped garbage-collection helpers.
//!
//! The bucket-scope GC walker (`cmd::gc::bucket`) lists content-
//! addressed objects under `.crab/{shards,xorbs,file-index}/` and
//! deletes anything unreachable from the ref-registry. Workflow stage
//! entries and experiment metadata live under `.crab/workflow/…`
//! — a namespace the current walker intentionally does not touch —
//! but once the workflow push path ships (task 4.8) the remote GC
//! walker will need a way to decide whether a workflow object is
//! reachable from a host's local view.
//!
//! This module is that decision: given a local repo root, walk the
//! on-disk experiment metadata cache and return the union of stage
//! hashes + experiment IDs a remote GC walker would consider
//! reachable. The function is I/O-light (`fs::read_dir` + `fs::read`)
//! and never spawns subprocesses — it's safe to call from any
//! context, including the signal handlers that run during a GC sweep.
//!
//! The helper is conservative about what it parses: malformed or
//! half-written `.meta.json` blobs are logged and skipped, not
//! surfaced as errors. The live set is the set of artifacts the
//! local cache *confidently* declares live; a partially written
//! blob is neither confidently live nor confidently dead, so the
//! GC walker's grace period handles it via its normal cutoff logic.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use tracing::warn;

use crate::experiment::{ExperimentId, ExperimentMetadata};
use crate::{Result, WorkflowError as CrabError};

/// Relative path (from a repo root) to the parent directory that
/// holds per-experiment metadata blobs. Kept as a `const` so the
/// `cmd::exp` write path and this reader agree on the layout
/// without duplicating the literal across modules.
const EXP_META_PARENT_REL: &str = ".crab/workflow/exp";

/// Suffix identifying experiment metadata files inside
/// [`EXP_META_PARENT_REL`]. Must match what `cmd::exp::write_local_metadata`
/// emits.
const EXP_META_SUFFIX: &str = ".meta.json";

/// Union of workflow artifacts reachable from the local experiment
/// metadata cache.
///
/// Used by the future bucket GC walker (task 4.8) to gate removal of
/// remote workflow state (stage entry JSONs, experiment metadata
/// blobs) against what this host still cares about. Kept as plain
/// `HashSet`s — cheap to union across hosts when the remote GC
/// walker aggregates multiple registries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalWorkflowLiveSet {
    /// Lowercase hex stage hashes declared by every live
    /// experiment on this host. Keyed by hex rather than
    /// [`crab_types::workflow::StageHash`] so the set can be
    /// unioned across hosts without reparsing bytes.
    pub stage_hashes: HashSet<String>,
    /// Experiment IDs declared live on this host. A [`HashSet`]
    /// rather than a [`Vec`] because duplicates are a no-op for
    /// downstream consumers; the walker emits each id at most once.
    pub experiment_ids: HashSet<ExperimentId>,
}

impl LocalWorkflowLiveSet {
    /// True when neither the stage-hash set nor the experiment-id
    /// set has any entries. Cheap and distinct from the derived
    /// `Default::default()` equality check.
    pub fn is_empty(&self) -> bool {
        self.stage_hashes.is_empty() && self.experiment_ids.is_empty()
    }
}

/// Walk the local experiment metadata cache and return the
/// [`LocalWorkflowLiveSet`] implied by every `.meta.json` blob the
/// cache contains.
///
/// Behavior:
/// - A missing parent directory yields an empty [`LocalWorkflowLiveSet`]
///   rather than an error. Absence is the normal state for a repo that
///   has never run an experiment.
/// - Entries that aren't regular files or whose name doesn't end in
///   `.meta.json` are ignored silently — the cache shares its parent
///   with experiment-scratch tmpdirs handled by
///   [`crate::exp_worktree::sweep_orphan_experiment_tmpdirs`].
/// - Parse errors on individual blobs log a `warn!` and skip the blob.
///   The live set is the union of successfully-parsed blobs, never a
///   "partially determined" set that could misgate a delete.
///
/// Returns [`CrabError::Io`] only for filesystem errors the caller
/// cannot recover from (unreadable parent directory, `fs::read_dir`
/// itself failing).
pub fn collect_local_workflow_live_set(repo_root: &Path) -> Result<LocalWorkflowLiveSet> {
    let parent = repo_root.join(EXP_META_PARENT_REL);
    let entries = match fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalWorkflowLiveSet::default());
        }
        Err(e) => return Err(CrabError::Io(e)),
    };

    let mut live = LocalWorkflowLiveSet::default();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "workflow gc: dir entry unreadable; skipping");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            // Tmpdir worktrees and other siblings live in this
            // directory; only plain `.meta.json` files describe
            // experiments. Silently skip everything else.
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(EXP_META_SUFFIX) {
            continue;
        }

        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "workflow gc: metadata read failed; skipping",
                );
                continue;
            }
        };

        let meta: ExperimentMetadata = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                // Malformed blob — partial write, disk corruption,
                // or forward-compat drift. Don't fail the live-set
                // walk: the remote GC walker's grace period is the
                // backstop for genuinely stale objects whose
                // metadata became unreadable.
                warn!(
                    path = %path.display(),
                    error = %e,
                    "workflow gc: metadata parse failed; skipping",
                );
                continue;
            }
        };

        live.experiment_ids.insert(meta.exp_id);
        for hex in meta.stages.values() {
            live.stage_hashes.insert(hex.clone());
        }
    }

    Ok(live)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::experiment::EXPERIMENT_METADATA_SCHEMA_VERSION;

    /// Build a valid [`ExperimentMetadata`] stamped with deterministic
    /// stage hashes drawn from `stage_hashes`. Empty `stage_hashes`
    /// produces an experiment with no stage entries, which is
    /// legitimate — experiments can mint before any stage runs.
    fn make_meta(id: ExperimentId, stage_hashes: &[&str]) -> ExperimentMetadata {
        let mut stages = BTreeMap::new();
        for (i, h) in stage_hashes.iter().enumerate() {
            stages.insert(format!("stage-{i}"), (*h).to_owned());
        }
        ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "0".repeat(40),
            queue_commit: None,
            name: None,
            message: None,
            status: "success".to_owned(),
            param_overrides: BTreeMap::new(),
            stages,
            metrics: BTreeMap::new(),
            cli_args: Vec::new(),
            host_fingerprint: "test-host".to_owned(),
            started_at: "2024-01-01T00:00:00.000Z".to_owned(),
            ended_at: None,
        }
    }

    /// Write `meta` to the conventional path under `repo_root`, creating
    /// parents as needed. Mirrors `cmd::exp::write_local_metadata` — if
    /// the two ever drift the walker's unit tests will surface the
    /// difference immediately.
    fn write_meta(repo_root: &Path, meta: &ExperimentMetadata) {
        let dir = repo_root.join(EXP_META_PARENT_REL);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{}{EXP_META_SUFFIX}", meta.exp_id));
        let bytes = meta.canonical_json().expect("serialize ok");
        fs::write(&file, bytes).unwrap();
    }

    #[test]
    fn collect_local_workflow_live_set_unions_stages_and_ids() {
        let tmp = TempDir::new().unwrap();
        let id_a = ExperimentId::new_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_b = ExperimentId::new_v7();

        write_meta(tmp.path(), &make_meta(id_a, &["aa", "bb"]));
        write_meta(tmp.path(), &make_meta(id_b, &["bb", "cc"]));

        let live = collect_local_workflow_live_set(tmp.path()).expect("walk ok");
        assert_eq!(live.experiment_ids.len(), 2);
        assert!(live.experiment_ids.contains(&id_a));
        assert!(live.experiment_ids.contains(&id_b));
        // Union across both experiments: "aa", "bb", "cc".
        assert_eq!(live.stage_hashes.len(), 3);
        assert!(live.stage_hashes.contains("aa"));
        assert!(live.stage_hashes.contains("bb"));
        assert!(live.stage_hashes.contains("cc"));
        assert!(!live.is_empty());
    }

    #[test]
    fn collect_local_workflow_live_set_missing_parent_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let live = collect_local_workflow_live_set(tmp.path()).expect("walk ok");
        assert!(live.is_empty());
        assert_eq!(live, LocalWorkflowLiveSet::default());
    }

    #[test]
    fn collect_local_workflow_live_set_skips_malformed_blobs() {
        let tmp = TempDir::new().unwrap();
        let valid_id = ExperimentId::new_v7();
        write_meta(tmp.path(), &make_meta(valid_id, &["aa"]));

        // Drop a garbage file alongside the valid one. It must not
        // propagate as an error — the walker keeps going and returns
        // only the artifacts it could parse.
        let dir = tmp.path().join(EXP_META_PARENT_REL);
        let garbage = dir.join("not-a-uuid.meta.json");
        fs::write(&garbage, b"{ this is not valid json").unwrap();

        let live = collect_local_workflow_live_set(tmp.path()).expect("walk ok");
        assert_eq!(live.experiment_ids.len(), 1);
        assert!(live.experiment_ids.contains(&valid_id));
        assert_eq!(live.stage_hashes.len(), 1);
        assert!(live.stage_hashes.contains("aa"));
    }

    #[test]
    fn collect_local_workflow_live_set_ignores_non_meta_files() {
        let tmp = TempDir::new().unwrap();
        let valid_id = ExperimentId::new_v7();
        write_meta(tmp.path(), &make_meta(valid_id, &["aa"]));

        let dir = tmp.path().join(EXP_META_PARENT_REL);
        fs::write(dir.join("README.md"), "hello").unwrap();
        // Subdirectory resembling an experiment tmpdir — must be
        // skipped without trying to read it as a meta blob.
        fs::create_dir_all(dir.join(format!("{valid_id}"))).unwrap();

        let live = collect_local_workflow_live_set(tmp.path()).expect("walk ok");
        assert_eq!(live.experiment_ids.len(), 1);
        assert_eq!(live.stage_hashes.len(), 1);
    }

    #[test]
    fn collect_local_workflow_live_set_handles_experiment_without_stages() {
        // An experiment that failed before any stage was hashed
        // writes a meta blob with an empty `stages` map. The
        // experiment id should still count as live — the user may
        // want to re-inspect it — but no stage hash contributes.
        let tmp = TempDir::new().unwrap();
        let id = ExperimentId::new_v7();
        write_meta(tmp.path(), &make_meta(id, &[]));

        let live = collect_local_workflow_live_set(tmp.path()).expect("walk ok");
        assert!(live.experiment_ids.contains(&id));
        assert!(live.stage_hashes.is_empty());
    }
}
