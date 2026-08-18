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
//! on-disk experiment metadata and checkpoint caches and return the union
//! of stage hashes, experiment IDs, and checkpoint payload identities a
//! remote GC walker would consider reachable. The function is I/O-light
//! (`fs::read_dir` + `fs::read`) and never spawns subprocesses — it's safe
//! to call from any context, including the signal handlers that run during
//! a GC sweep.
//!
//! The helper is conservative about what it parses: malformed or
//! half-written `.meta.json` blobs are logged and skipped, not
//! surfaced as errors. Checkpoint state is different: a malformed
//! lineage can hide a live payload, so checkpoint parse or reference
//! failures abort the walk. A caller must fail closed rather than
//! treating an unknown checkpoint as dead.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use tracing::warn;

use crate::checkpoint::{CheckpointLineage, CheckpointRecord};
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

/// Relative path to per-experiment checkpoint state.
const CHECKPOINT_PARENT_REL: &str = ".crab/workflow/checkpoints";

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
    /// Blake3 payload identities referenced by acknowledged checkpoints.
    /// Values retain the canonical `b3:<lowercase-hex>` spelling used by
    /// [`CheckpointRecord`], allowing a remote walker to protect the exact
    /// immutable payloads rather than only the lineage JSON.
    pub checkpoint_object_hashes: HashSet<String>,
}

impl LocalWorkflowLiveSet {
    /// True when no stage, experiment, or checkpoint payload roots were found.
    /// Cheap and distinct from the derived `Default::default()` equality
    /// check.
    pub fn is_empty(&self) -> bool {
        self.stage_hashes.is_empty()
            && self.experiment_ids.is_empty()
            && self.checkpoint_object_hashes.is_empty()
    }
}

/// Walk the local experiment metadata cache and return the
/// [`LocalWorkflowLiveSet`] implied by every `.meta.json` blob the
/// cache contains.
///
/// Behavior:
/// - A missing metadata parent contributes no metadata entries rather than an
///   error. Checkpoint state is still scanned, because a crash can leave a
///   durable lineage after its summary blob was not written.
/// - Entries that aren't regular files or whose name doesn't end in
///   `.meta.json` are ignored silently — the cache shares its parent
///   with experiment-scratch tmpdirs handled by
///   [`crate::exp_worktree::sweep_orphan_experiment_tmpdirs`].
/// - Parse errors on individual metadata blobs log a `warn!` and skip the
///   blob.
/// - Checkpoint state is validated after metadata. A missing, malformed, or
///   unreferenced checkpoint payload returns an error so a destructive caller
///   cannot mistake unknown state for unreachable state.
///   The live set is the union of successfully-parsed blobs, never a
///   "partially determined" set that could misgate a delete.
///
/// Filesystem and checkpoint-contract errors are returned so a destructive
/// caller can fail closed; malformed legacy metadata blobs remain logged and
/// skipped for compatibility.
pub fn collect_local_workflow_live_set(repo_root: &Path) -> Result<LocalWorkflowLiveSet> {
    let parent = repo_root.join(EXP_META_PARENT_REL);
    let mut live = LocalWorkflowLiveSet::default();
    match fs::read_dir(&parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(error = %e, "workflow gc: dir entry unreadable; skipping");
                        continue;
                    }
                };
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        warn!(path = %path.display(), error = %error, "workflow gc: metadata entry stat failed; skipping");
                        continue;
                    }
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
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
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CrabError::Io(e)),
    }

    collect_local_checkpoint_live_set(repo_root, &mut live)?;

    Ok(live)
}

fn collect_local_checkpoint_live_set(
    repo_root: &Path,
    live: &mut LocalWorkflowLiveSet,
) -> Result<()> {
    let parent = repo_root.join(CHECKPOINT_PARENT_REL);
    let entries = match fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CrabError::Io(error)),
    };

    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(checkpoint_state_error(&path, "symlink is not allowed"));
        }
        if !metadata.is_dir() {
            // Temporary files are ignored only when they use the same naming
            // convention as the atomic checkpoint writers. Any other regular
            // file means state was written outside the contract.
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if name.contains(".tmp-") || name.contains(".backup-") || name.ends_with(".lock") {
                continue;
            }
            return Err(checkpoint_state_error(
                &path,
                "checkpoint state entry is not a directory",
            ));
        }
        let transient = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| {
                name.contains(".tmp-")
                    || name.contains(".backup-")
                    || name.contains(".resume-")
                    || name.contains(".pull-")
                    || name.ends_with(".lock")
            });
        if transient {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                checkpoint_state_error(&path, "checkpoint experiment directory is not UTF-8")
            })?;
        let exp_id = name.parse::<ExperimentId>().map_err(|error| {
            checkpoint_state_error(&path, &format!("invalid experiment id: {error}"))
        })?;
        live.experiment_ids.insert(exp_id);
        collect_checkpoint_state(&path, live)?;
    }
    Ok(())
}

fn collect_checkpoint_state(state_root: &Path, live: &mut LocalWorkflowLiveSet) -> Result<()> {
    let mut lineage_paths = Vec::new();
    collect_checkpoint_files(state_root, state_root, &mut lineage_paths)?;
    let mut saw_lineage = false;
    for path in lineage_paths {
        if path.file_name().and_then(|value| value.to_str()) == Some("reset.json") {
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            return Err(checkpoint_state_error(
                &path,
                "checkpoint state file has an unsupported extension",
            ));
        }
        saw_lineage = true;
        let lineage = CheckpointLineage::load(&path).map_err(|error| {
            checkpoint_state_error(&path, &format!("checkpoint lineage is invalid: {error}"))
        })?;
        for record in lineage.records {
            collect_checkpoint_record(state_root, &record, live)?;
        }
    }

    if !saw_lineage {
        // An empty state directory is normal for an experiment that has not
        // emitted a checkpoint. Keep it live by experiment id, but do not
        // silently accept an objects-only directory.
        let objects = state_root.join("objects");
        if objects.exists() {
            return Err(checkpoint_state_error(
                &objects,
                "checkpoint objects exist without a lineage",
            ));
        }
    }
    Ok(())
}

fn collect_checkpoint_record(
    state_root: &Path,
    record: &CheckpointRecord,
    live: &mut LocalWorkflowLiveSet,
) -> Result<()> {
    if let Some(stage_hash) = record.stage_hash.strip_prefix("b3:") {
        live.stage_hashes.insert(stage_hash.to_owned());
    }
    for hash in record.outputs.values().chain(record.metrics.values()) {
        let digest = hash.strip_prefix("b3:").ok_or_else(|| {
            checkpoint_state_error(state_root, "checkpoint payload hash is not a b3 digest")
        })?;
        let object = state_root.join("objects").join(digest).join("payload");
        let metadata = fs::symlink_metadata(&object).map_err(|error| {
            checkpoint_state_error(
                &object,
                &format!("checkpoint payload is unavailable: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(checkpoint_state_error(
                &object,
                "checkpoint payload has an invalid type",
            ));
        }
        live.checkpoint_object_hashes.insert(hash.clone());
    }
    Ok(())
}

fn collect_checkpoint_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(checkpoint_state_error(
                &path,
                "checkpoint state symlink is not allowed",
            ));
        }
        let relative = path.strip_prefix(root).map_err(|error| {
            checkpoint_state_error(
                &path,
                &format!("checkpoint state path escaped root: {error}"),
            )
        })?;
        if relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return Err(checkpoint_state_error(
                &path,
                "checkpoint state path is unsafe",
            ));
        }
        if metadata.is_dir() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if name == "objects"
                || name.contains(".tmp-")
                || name.contains(".backup-")
                || name.ends_with(".lock")
            {
                continue;
            }
            collect_checkpoint_files(root, &path, files)?;
        } else if metadata.is_file() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if !name.contains(".tmp-") && !name.contains(".backup-") && !name.ends_with(".lock") {
                files.push(path);
            }
        } else {
            return Err(checkpoint_state_error(
                &path,
                "checkpoint state entry has an invalid type",
            ));
        }
    }
    Ok(())
}

fn checkpoint_state_error(path: &Path, detail: &str) -> CrabError {
    CrabError::YamlInvalid {
        key: "workflow_gc_checkpoint_state".to_owned(),
        origin: format!("{}: {detail}", path.display()),
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
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::checkpoint::{CHECKPOINT_SCHEMA_VERSION, CheckpointLineage, CheckpointRecord};
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

    #[test]
    fn collect_local_workflow_live_set_keeps_checkpoint_payloads_live() {
        let tmp = TempDir::new().unwrap();
        let id = ExperimentId::new_v7();
        let payload_hash = format!("b3:{}", "ab".repeat(32));
        let state_root = tmp.path().join(CHECKPOINT_PARENT_REL).join(id.to_string());
        let payload = state_root
            .join("objects")
            .join(payload_hash.strip_prefix("b3:").unwrap())
            .join("payload");
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(&payload, b"checkpoint").unwrap();
        let mut lineage = CheckpointLineage::default();
        lineage
            .append(CheckpointRecord {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                id: "checkpoint-0".to_owned(),
                experiment: id.to_string(),
                stage: "train".to_owned(),
                sequence: 0,
                parent: None,
                request_nonce: None,
                stage_hash: format!("b3:{}", "cd".repeat(32)),
                created_at_unix_ms: 0,
                outputs: BTreeMap::from([("model.bin".to_owned(), payload_hash.clone())]),
                metrics: BTreeMap::new(),
                terminal: false,
                resumable: true,
            })
            .unwrap();
        lineage.save_atomic(&state_root.join("train.json")).unwrap();

        let live = collect_local_workflow_live_set(tmp.path()).unwrap();
        assert!(live.experiment_ids.contains(&id));
        assert!(live.stage_hashes.contains(&"cd".repeat(32)));
        assert!(live.checkpoint_object_hashes.contains(&payload_hash));
    }

    #[test]
    fn collect_local_workflow_live_set_fails_closed_on_missing_checkpoint_payload() {
        let tmp = TempDir::new().unwrap();
        let id = ExperimentId::new_v7();
        let state_root = tmp.path().join(CHECKPOINT_PARENT_REL).join(id.to_string());
        fs::create_dir_all(&state_root).unwrap();
        let mut lineage = CheckpointLineage::default();
        lineage
            .append(CheckpointRecord {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                id: "checkpoint-0".to_owned(),
                experiment: id.to_string(),
                stage: "train".to_owned(),
                sequence: 0,
                parent: None,
                request_nonce: None,
                stage_hash: format!("b3:{}", "cd".repeat(32)),
                created_at_unix_ms: 0,
                outputs: BTreeMap::from([(
                    "model.bin".to_owned(),
                    format!("b3:{}", "ab".repeat(32)),
                )]),
                metrics: BTreeMap::new(),
                terminal: false,
                resumable: true,
            })
            .unwrap();
        lineage.save_atomic(&state_root.join("train.json")).unwrap();

        let error = collect_local_workflow_live_set(tmp.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("checkpoint payload is unavailable")
        );
    }
}
