//! Overlay publish helpers for writable VFS mounts.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crab_staging::{StagingArea, StagingAreaReadOnly, StagingBatchId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::core::error::{CrabError, Result};
use crate::overlay::{OverlayRecord, OverlayStore};
use crate::resolver::OverlayKind;
use crate::snapshot::SnapshotStore;

const DELETIONS_MANIFEST: &str = ".crab-overlay-deletions";
const OVERLAY_RESET_DRAIN: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OverlayChangeKind {
    Create,
    Modify,
    Delete,
    Rename,
    Mkdir,
    Symlink,
}

impl std::fmt::Display for OverlayChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create => write!(f, "create"),
            Self::Modify => write!(f, "modify"),
            Self::Delete => write!(f, "delete"),
            Self::Rename => write!(f, "rename"),
            Self::Mkdir => write!(f, "mkdir"),
            Self::Symlink => write!(f, "symlink"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OverlayChange {
    pub path: String,
    pub kind: OverlayChangeKind,
    pub size_bytes: u64,
    pub mode: u32,
    pub has_backing_file: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OverlayDiff {
    pub changes: Vec<OverlayChange>,
    pub estimated_upload_bytes: u64,
    pub deletion_count: u64,
}

#[derive(Debug, Clone)]
pub struct OverlayPaths {
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub upper_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct OverlayCommitOptions {
    pub cache_dir: PathBuf,
    pub git_dir: PathBuf,
    pub ref_name: String,
    pub message: String,
    pub push: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OverlayCommitResult {
    pub transaction_id: String,
    pub commit_oid: Option<String>,
    pub pushed: bool,
    pub overlay_cleaned: bool,
    pub diff: OverlayDiff,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PublishTransaction {
    id: String,
    status: String,
    ref_name: String,
    base_oid: Option<String>,
    commit_oid: Option<String>,
    pushed: bool,
    error: Option<String>,
    #[serde(default)]
    overlay_fingerprint: Vec<String>,
}

#[derive(Debug, Clone)]
struct CrabOverlayFile {
    path: String,
    source_path: PathBuf,
    mode: u32,
}

#[derive(Debug)]
struct DirectPointerEntry {
    path: String,
    abs_path: PathBuf,
    file_hash: [u8; 32],
    size: u64,
    mode: u32,
    batch_id: StagingBatchId,
}

struct GitIndexEntry {
    path: String,
    mode: String,
    sha: String,
}

impl OverlayPaths {
    #[must_use]
    pub fn from_cache_dir(cache_dir: &Path) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            db_path: cache_dir.join("overlay.db"),
            upper_dir: cache_dir.join("overlay/upper"),
        }
    }
}

pub fn inspect_overlay(paths: &OverlayPaths) -> Result<OverlayDiff> {
    if !paths.db_path.exists() {
        return Ok(empty_diff());
    }

    let store = OverlayStore::open(&paths.db_path, &paths.upper_dir)?;
    inspect_overlay_store(&store)
}

pub fn inspect_overlay_store(store: &OverlayStore) -> Result<OverlayDiff> {
    diff_from_store(store)
}

pub fn export_overlay(paths: &OverlayPaths, destination: &Path) -> Result<OverlayDiff> {
    export_overlay_with_view(paths, destination, None)
}

pub fn export_overlay_from_view(
    paths: &OverlayPaths,
    destination: &Path,
    view_root: &Path,
) -> Result<OverlayDiff> {
    export_overlay_with_view(paths, destination, Some(view_root))
}

fn export_overlay_with_view(
    paths: &OverlayPaths,
    destination: &Path,
    view_root: Option<&Path>,
) -> Result<OverlayDiff> {
    if !paths.db_path.exists() {
        std::fs::create_dir_all(destination)?;
        return Ok(empty_diff());
    }

    let store = OverlayStore::open(&paths.db_path, &paths.upper_dir)?;
    let _freeze = store.freeze_writes()?;
    let all_records = store.records()?;
    let records = publishable_records(&all_records);
    std::fs::create_dir_all(destination)?;

    let mut deletion_manifest = Vec::new();
    for record in &records {
        let kind = change_kind(record.kind);
        match kind {
            OverlayChangeKind::Delete => {
                deletion_manifest.push(record.path.clone());
            }
            OverlayChangeKind::Mkdir => {
                std::fs::create_dir_all(safe_join(destination, &record.path)?)?;
            }
            OverlayChangeKind::Symlink => {
                let Some(backing) = &record.backing_path else {
                    continue;
                };
                materialize_symlink(backing, &safe_join(destination, &record.path)?)?;
            }
            OverlayChangeKind::Create | OverlayChangeKind::Modify | OverlayChangeKind::Rename => {
                if is_metadata_base_rename(record) {
                    let Some(view_root) = view_root else {
                        return Err(CrabError::Internal(format!(
                            "overlay entry {} requires a mounted view for export",
                            record.path
                        )));
                    };
                    export_from_mounted_view(record, view_root, destination)?;
                    continue;
                }
                let Some(backing) = &record.backing_path else {
                    continue;
                };
                let target = safe_join(destination, &record.path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(backing, &target)?;
                set_regular_permissions(&target, record.mode)?;
            }
        }
    }

    if !deletion_manifest.is_empty() {
        deletion_manifest.sort();
        let content = deletion_manifest.join("\n") + "\n";
        std::fs::write(destination.join(DELETIONS_MANIFEST), content)?;
    }

    Ok(diff_from_records(&records))
}

fn export_from_mounted_view(
    record: &crate::overlay::OverlayRecord,
    view_root: &Path,
    destination: &Path,
) -> Result<()> {
    let source = safe_join(view_root, &record.path)?;
    let target = safe_join(destination, &record.path)?;
    copy_view_path(&source, &target, record.mode)
}

pub fn reset_overlay(paths: &OverlayPaths) -> Result<OverlayDiff> {
    if !paths.db_path.exists() {
        return Ok(empty_diff());
    }
    let store = OverlayStore::open(&paths.db_path, &paths.upper_dir)?;
    reset_overlay_store(&store)
}

pub fn reset_overlay_store(store: &OverlayStore) -> Result<OverlayDiff> {
    let _freeze = store.freeze_writes()?;
    let diff = diff_from_store(store)?;
    std::thread::sleep(OVERLAY_RESET_DRAIN);
    store.clear()?;
    Ok(diff)
}

pub fn commit_overlay(options: &OverlayCommitOptions) -> Result<OverlayCommitResult> {
    commit_overlay_with_snapshot(options, None)
}

pub fn commit_overlay_with_snapshot(
    options: &OverlayCommitOptions,
    snapshot: Option<&SnapshotStore>,
) -> Result<OverlayCommitResult> {
    let paths = OverlayPaths::from_cache_dir(&options.cache_dir);
    let transaction_id = transaction_id();
    if !paths.db_path.exists() {
        if options.push {
            let commit_oid = push_current_ref(options)?;
            return Ok(OverlayCommitResult {
                transaction_id,
                commit_oid: Some(commit_oid),
                pushed: true,
                overlay_cleaned: false,
                diff: empty_diff(),
            });
        }
        return Ok(OverlayCommitResult {
            transaction_id,
            commit_oid: None,
            pushed: false,
            overlay_cleaned: false,
            diff: empty_diff(),
        });
    }

    let store = OverlayStore::open(&paths.db_path, &paths.upper_dir)?;
    let _freeze = store.freeze_writes()?;
    let all_records = store.records()?;
    let records = publishable_records(&all_records);
    let diff = diff_from_records(&records);
    let overlay_fingerprint = overlay_records_fingerprint(&records);
    let mut transaction = PublishTransaction {
        id: transaction_id.clone(),
        status: "started".to_owned(),
        ref_name: options.ref_name.clone(),
        base_oid: match snapshot {
            Some(snapshot) => snapshot.head_oid()?,
            None => read_base_oid(&options.cache_dir)?,
        },
        commit_oid: None,
        pushed: false,
        error: None,
        overlay_fingerprint: overlay_fingerprint.clone(),
    };
    write_transaction(&options.cache_dir, &transaction)?;

    let result = (|| {
        if records.is_empty() {
            let cleaned_ignored = if all_records.is_empty() {
                false
            } else {
                store.clear()?;
                true
            };
            if options.push {
                let commit_oid = push_current_ref(options).inspect_err(|error| {
                    set_transaction_status(&mut transaction, "failed");
                    transaction.error = Some(error.to_string());
                    let _ = write_transaction(&options.cache_dir, &transaction);
                })?;
                transaction.commit_oid = Some(commit_oid.clone());
                transaction.pushed = true;
                set_transaction_status(&mut transaction, "pushed_cleaned");
                write_transaction(&options.cache_dir, &transaction)?;
                return Ok(OverlayCommitResult {
                    transaction_id,
                    commit_oid: Some(commit_oid),
                    pushed: true,
                    overlay_cleaned: cleaned_ignored,
                    diff,
                });
            }
            set_transaction_status(&mut transaction, "empty");
            write_transaction(&options.cache_dir, &transaction)?;
            return Ok(OverlayCommitResult {
                transaction_id,
                commit_oid: None,
                pushed: false,
                overlay_cleaned: cleaned_ignored,
                diff,
            });
        }

        if let Some(base_oid) = transaction.base_oid.clone() {
            let current_oid = rev_parse(&options.git_dir, &options.ref_name)?;
            if current_oid != base_oid {
                if let Some(result) = try_finalize_recorded_publish(
                    &options.cache_dir,
                    &paths,
                    &options.git_dir,
                    &options.ref_name,
                    &current_oid,
                    &overlay_fingerprint,
                    &diff,
                    snapshot,
                )? {
                    set_transaction_status(&mut transaction, "recovered_existing");
                    transaction.commit_oid.clone_from(&result.commit_oid);
                    transaction.pushed = result.pushed;
                    write_transaction(&options.cache_dir, &transaction)?;
                    return Ok(result);
                }
                set_transaction_status(&mut transaction, "failed");
                transaction.error = Some(format!(
                    "base ref moved from {base_oid} to {current_oid}; refresh or remount before committing overlay changes"
                ));
                write_transaction(&options.cache_dir, &transaction)?;
                return Err(CrabError::Internal(
                    transaction.error.clone().unwrap_or_default(),
                ));
            }
        }

        let mut worktree = PublishWorktree::create(&options.git_dir)?;
        let base = transaction
            .base_oid
            .clone()
            .unwrap_or_else(|| options.ref_name.clone());
        run_git(
            Command::new("git")
                .arg("--git-dir")
                .arg(&options.git_dir)
                .args(["worktree", "add", "--detach", "--no-checkout"])
                .arg(worktree.path())
                .arg(&base),
            "git worktree add",
        )
        .inspect_err(|e| {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(e.to_string());
            let _ = write_transaction(&options.cache_dir, &transaction);
        })?;
        worktree.mark_registered();

        run_git(
            Command::new("git")
                .arg("-C")
                .arg(worktree.path())
                .args(["read-tree", &base]),
            "git read-tree",
        )?;

        ensure_git_commit_identity(worktree.path()).inspect_err(|e| {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(e.to_string());
            let _ = write_transaction(&options.cache_dir, &transaction);
        })?;

        let applied_base_renames = apply_metadata_base_renames(&records, worktree.path())?;
        materialize_git_policy_files(worktree.path())?;
        if overlay_changes_git_attributes(&records) {
            let attribute_records = records
                .iter()
                .filter(|record| is_git_attributes_path(&record.path))
                .cloned()
                .collect::<Vec<_>>();
            apply_records(
                &attribute_records,
                worktree.path(),
                &std::collections::HashSet::new(),
                &applied_base_renames,
            )?;
            run_git_add(
                worktree.path(),
                &attribute_records,
                &[],
                &applied_base_renames,
            )?;
        }
        let crab_files = crab_overlay_files(&records, worktree.path())?;
        let skipped_crab_paths = crab_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<std::collections::HashSet<_>>();

        if let Err(err) = apply_records(
            &records,
            worktree.path(),
            &skipped_crab_paths,
            &applied_base_renames,
        ) {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(err.to_string());
            write_transaction(&options.cache_dir, &transaction)?;
            return Err(err);
        }

        run_git_add(
            worktree.path(),
            &records,
            &crab_files,
            &applied_base_renames,
        )
        .inspect_err(|e| {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(e.to_string());
            let _ = write_transaction(&options.cache_dir, &transaction);
        })?;

        if !crab_files.is_empty() {
            let pointer_entries =
                stage_crab_overlay_files(&crab_files, worktree.path(), &options.git_dir)
                    .inspect_err(|e| {
                        set_transaction_status(&mut transaction, "failed");
                        transaction.error = Some(e.to_string());
                        let _ = write_transaction(&options.cache_dir, &transaction);
                    })?;
            if let Err(error) =
                write_pointer_entries_to_git_index(worktree.path(), &pointer_entries)
            {
                if let Err(rollback_error) =
                    rollback_staged_pointer_entries(&options.git_dir, &pointer_entries)
                {
                    tracing::warn!(
                        error = %rollback_error,
                        "failed to roll back mount staging after Git index publication failed"
                    );
                }
                set_transaction_status(&mut transaction, "failed");
                transaction.error = Some(error.to_string());
                write_transaction(&options.cache_dir, &transaction)?;
                return Err(error);
            }
            if let Err(error) =
                mark_staged_pointer_entries_published(&options.git_dir, &pointer_entries)
            {
                if let Err(rollback_error) =
                    rollback_staged_pointer_entries(&options.git_dir, &pointer_entries)
                {
                    tracing::warn!(
                        error = %rollback_error,
                        "failed to roll back mount staging after batch publication failed"
                    );
                }
                set_transaction_status(&mut transaction, "failed");
                transaction.error = Some(error.to_string());
                write_transaction(&options.cache_dir, &transaction)?;
                return Err(error);
            }
        }

        let tree_oid = command_stdout(
            Command::new("git")
                .arg("-C")
                .arg(worktree.path())
                .arg("write-tree"),
            "git write-tree",
        )?;
        let base_tree = rev_parse(&options.git_dir, &format!("{base}^{{tree}}"))?;
        if tree_oid == base_tree {
            let error = CrabError::Internal("overlay has no publishable Git changes".into());
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(error.to_string());
            write_transaction(&options.cache_dir, &transaction)?;
            return Err(error);
        }
        let commit_oid = command_stdout(
            Command::new("git")
                .arg("-C")
                .arg(worktree.path())
                .arg("commit-tree")
                .arg(&tree_oid)
                .arg("-p")
                .arg(&base)
                .arg("-m")
                .arg(&options.message),
            "git commit-tree",
        )
        .inspect_err(|e| {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(e.to_string());
            let _ = write_transaction(&options.cache_dir, &transaction);
        })?;
        transaction.commit_oid = Some(commit_oid.clone());
        set_transaction_status(&mut transaction, "created");
        write_transaction(&options.cache_dir, &transaction)?;

        if options.push {
            let refspec = format!("{commit_oid}:{}", options.ref_name);
            run_git(
                Command::new("git")
                    .arg("-C")
                    .arg(worktree.path())
                    .args(["push", "origin", &refspec]),
                "git push",
            )
            .inspect_err(|e| {
                set_transaction_status(&mut transaction, "failed");
                transaction.error = Some(e.to_string());
                let _ = write_transaction(&options.cache_dir, &transaction);
            })?;
            set_transaction_status(&mut transaction, "pushed");
            transaction.pushed = true;
            write_transaction(&options.cache_dir, &transaction)?;
        }

        update_published_ref(
            &options.git_dir,
            &options.ref_name,
            &commit_oid,
            transaction.base_oid.as_deref(),
            transaction.pushed,
        )
        .inspect_err(|e| {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(e.to_string());
            let _ = write_transaction(&options.cache_dir, &transaction);
        })?;

        if !transaction.pushed {
            set_transaction_status(&mut transaction, "committed");
            write_transaction(&options.cache_dir, &transaction)?;
        }

        if let Err(err) = finalize_committed_overlay(
            &paths,
            &options.git_dir,
            &commit_oid,
            &options.ref_name,
            snapshot,
        ) {
            set_transaction_status(&mut transaction, "failed");
            transaction.error = Some(err.to_string());
            write_transaction(&options.cache_dir, &transaction)?;
            return Err(err);
        }
        let final_status = if transaction.pushed {
            "pushed_cleaned"
        } else {
            "committed_cleaned"
        };
        set_transaction_status(&mut transaction, final_status);
        write_transaction(&options.cache_dir, &transaction)?;

        Ok(OverlayCommitResult {
            transaction_id,
            commit_oid: Some(commit_oid),
            pushed: transaction.pushed,
            overlay_cleaned: true,
            diff,
        })
    })();
    if let Err(error) = &result {
        set_transaction_status(&mut transaction, "failed");
        transaction.error = Some(error.to_string());
        if let Err(write_error) = write_transaction(&options.cache_dir, &transaction) {
            tracing::warn!(
                error = %write_error,
                transaction_id = %transaction.id,
                "failed to record publish transaction failure"
            );
        }
    }
    result
}

fn push_current_ref(options: &OverlayCommitOptions) -> Result<String> {
    let commit_oid = rev_parse(&options.git_dir, &options.ref_name)?;
    let refspec = format!("{}:{}", options.ref_name, options.ref_name);
    run_git(
        Command::new("git")
            .arg("--git-dir")
            .arg(&options.git_dir)
            .args(["push", "origin", &refspec]),
        "git push",
    )?;
    Ok(commit_oid)
}

fn diff_from_store(store: &OverlayStore) -> Result<OverlayDiff> {
    let records = store.records()?;
    let records = publishable_records(&records);
    Ok(diff_from_records(&records))
}

fn empty_diff() -> OverlayDiff {
    OverlayDiff {
        changes: Vec::new(),
        estimated_upload_bytes: 0,
        deletion_count: 0,
    }
}

fn set_transaction_status(transaction: &mut PublishTransaction, status: &str) {
    status.clone_into(&mut transaction.status);
}

fn try_finalize_recorded_publish(
    cache_dir: &Path,
    paths: &OverlayPaths,
    git_dir: &Path,
    ref_name: &str,
    current_oid: &str,
    overlay_fingerprint: &[String],
    diff: &OverlayDiff,
    snapshot: Option<&SnapshotStore>,
) -> Result<Option<OverlayCommitResult>> {
    let mut transactions = read_publish_transactions(cache_dir)?;
    transactions.sort_by(|left, right| right.id.cmp(&left.id));
    let Some(mut transaction) = transactions.into_iter().find(|transaction| {
        transaction.ref_name == ref_name
            && transaction.commit_oid.as_deref() == Some(current_oid)
            && !transaction.overlay_fingerprint.is_empty()
            && transaction.overlay_fingerprint == overlay_fingerprint
            && !matches!(
                transaction.status.as_str(),
                "committed_cleaned" | "pushed_cleaned"
            )
    }) else {
        return Ok(None);
    };

    if let Err(err) = finalize_committed_overlay(paths, git_dir, current_oid, ref_name, snapshot) {
        set_transaction_status(&mut transaction, "failed");
        transaction.error = Some(err.to_string());
        write_transaction(cache_dir, &transaction)?;
        return Err(err);
    }

    let final_status = if transaction.pushed {
        "pushed_cleaned"
    } else {
        "committed_cleaned"
    };
    set_transaction_status(&mut transaction, final_status);
    transaction.error = None;
    write_transaction(cache_dir, &transaction)?;

    Ok(Some(OverlayCommitResult {
        transaction_id: transaction.id,
        commit_oid: Some(current_oid.to_owned()),
        pushed: transaction.pushed,
        overlay_cleaned: true,
        diff: diff.clone(),
    }))
}

fn read_publish_transactions(cache_dir: &Path) -> Result<Vec<PublishTransaction>> {
    let dir = cache_dir.join("publish/transactions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(CrabError::Io(err)),
    };

    let mut transactions = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        match serde_json::from_slice::<PublishTransaction>(&bytes) {
            Ok(transaction) => transactions.push(transaction),
            Err(err) => debug!(
                path = %path.display(),
                error = %err,
                "skipping unreadable publish transaction"
            ),
        }
    }
    Ok(transactions)
}

fn overlay_records_fingerprint(records: &[OverlayRecord]) -> Vec<String> {
    let mut entries = records
        .iter()
        .map(|record| {
            let backing = record
                .backing_path
                .as_ref()
                .map(|path| format!("{}\0{}", path.display(), backing_file_fingerprint(path)))
                .unwrap_or_default();
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}\0{}",
                record.path,
                change_kind(record.kind),
                record.base_path.as_deref().unwrap_or_default(),
                record.mode,
                record.size,
                record.mtime_ns,
                backing
            )
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn backing_file_fingerprint(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_dir() {
                "dir"
            } else {
                "other"
            };
            let modified = metadata
                .modified()
                .map_or_else(|err| format!("modified:{:?}", err.kind()), system_time_ns);
            format!("{kind}:{}:{modified}", metadata.len())
        }
        Err(err) => format!("missing:{:?}", err.kind()),
    }
}

fn system_time_ns(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().to_string(),
        Err(err) => format!("-{}", err.duration().as_nanos()),
    }
}

fn diff_from_records(records: &[OverlayRecord]) -> OverlayDiff {
    let mut estimated_upload_bytes = 0u64;
    let mut deletion_count = 0u64;
    let mut changes = Vec::with_capacity(records.len());

    for record in records {
        if is_macos_appledouble_path(&record.path) {
            continue;
        }
        let kind = change_kind(record.kind);
        let has_backing_file = record
            .backing_path
            .as_ref()
            .is_some_and(|path| path.exists());
        if kind == OverlayChangeKind::Delete {
            deletion_count += 1;
        } else if matches!(
            kind,
            OverlayChangeKind::Create
                | OverlayChangeKind::Modify
                | OverlayChangeKind::Rename
                | OverlayChangeKind::Symlink
        ) && has_backing_file
        {
            estimated_upload_bytes = estimated_upload_bytes.saturating_add(record.size);
        }
        changes.push(OverlayChange {
            path: record.path.clone(),
            kind,
            size_bytes: record.size,
            mode: record.mode,
            has_backing_file,
        });
    }

    OverlayDiff {
        changes,
        estimated_upload_bytes,
        deletion_count,
    }
}

fn publishable_records(records: &[OverlayRecord]) -> Vec<OverlayRecord> {
    records
        .iter()
        .filter(|record| !is_macos_appledouble_path(&record.path))
        .cloned()
        .collect()
}

fn is_macos_appledouble_path(path: &str) -> bool {
    path.split('/')
        .any(|component| component.len() > 2 && component.starts_with("._"))
}

fn apply_records(
    records: &[crate::overlay::OverlayRecord],
    worktree: &Path,
    skipped_crab_paths: &std::collections::HashSet<String>,
    applied_base_renames: &[AppliedBaseRename],
) -> Result<()> {
    for record in records {
        if is_applied_metadata_base_rename(record, applied_base_renames) {
            continue;
        }
        match change_kind(record.kind) {
            OverlayChangeKind::Delete => {
                let target = safe_join(worktree, &record.path)?;
                if target.is_dir() {
                    std::fs::remove_dir_all(target)?;
                } else if target.exists() {
                    std::fs::remove_file(target)?;
                }
            }
            OverlayChangeKind::Mkdir => {
                std::fs::create_dir_all(safe_join(worktree, &record.path)?)?;
            }
            OverlayChangeKind::Symlink => {
                let backing = record.backing_path.as_ref().ok_or_else(|| {
                    CrabError::Internal(format!(
                        "overlay entry {} has no backing file",
                        record.path
                    ))
                })?;
                materialize_symlink(backing, &safe_join(worktree, &record.path)?)?;
            }
            OverlayChangeKind::Create | OverlayChangeKind::Modify | OverlayChangeKind::Rename => {
                if skipped_crab_paths.contains(&record.path)
                    && overlay_record_has_regular_backing(record)
                {
                    continue;
                }
                let backing = record.backing_path.as_ref().ok_or_else(|| {
                    CrabError::Internal(format!(
                        "overlay entry {} has no backing file",
                        record.path
                    ))
                })?;
                let target = safe_join(worktree, &record.path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(backing, &target)?;
                set_regular_permissions(&target, record.mode)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct AppliedBaseRename {
    old_path: String,
    new_path: String,
}

fn apply_metadata_base_renames(
    records: &[crate::overlay::OverlayRecord],
    worktree: &Path,
) -> Result<Vec<AppliedBaseRename>> {
    let mut candidates = records
        .iter()
        .filter(|record| is_metadata_base_rename(record))
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        path_depth(&a.path)
            .cmp(&path_depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut applied = Vec::new();
    for record in candidates {
        if is_applied_metadata_base_rename(record, &applied) {
            continue;
        }
        let base_path = record.base_path.as_deref().ok_or_else(|| {
            CrabError::Internal(format!("metadata rename {} has no base path", record.path))
        })?;
        rename_git_index_subtree(worktree, base_path, &record.path)?;
        applied.push(AppliedBaseRename {
            old_path: base_path.to_owned(),
            new_path: record.path.clone(),
        });
    }
    Ok(applied)
}

fn rename_git_index_subtree(worktree: &Path, old_path: &str, new_path: &str) -> Result<()> {
    let source_entries = git_index_entries_under(worktree, old_path)?;
    let mut removed_paths = source_entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    removed_paths.extend(
        git_index_entries_under(worktree, new_path)?
            .into_iter()
            .map(|entry| entry.path),
    );
    remove_git_index_paths(worktree, &removed_paths)?;

    let moved_entries = source_entries
        .into_iter()
        .map(|entry| GitIndexEntry {
            path: replace_path_prefix(&entry.path, old_path, new_path),
            mode: entry.mode,
            sha: entry.sha,
        })
        .collect::<Vec<_>>();
    publish_git_index_entries(worktree, &moved_entries)
}

fn replace_path_prefix(path: &str, old_path: &str, new_path: &str) -> String {
    if path == old_path {
        return new_path.to_owned();
    }
    format!("{new_path}{}", &path[old_path.len()..])
}

fn git_index_entries_under(worktree: &Path, path: &str) -> Result<Vec<GitIndexEntry>> {
    let pathspec = format!(":(literal){path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-s", "-z", "--", &pathspec])
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {}",
            command_diagnostics(&output)
        )));
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(parse_git_index_entry)
        .collect()
}

fn parse_git_index_entry(entry: &[u8]) -> Result<GitIndexEntry> {
    let separator = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| CrabError::Internal("git ls-files returned malformed index entry".into()))?;
    let metadata = &entry[..separator];
    let path = &entry[separator + 1..];
    let fields = metadata
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != 3 || fields[2] != b"0" {
        return Err(CrabError::Internal(
            "git ls-files returned an unsupported staged index entry".into(),
        ));
    }
    Ok(GitIndexEntry {
        path: String::from_utf8(path.to_vec())
            .map_err(|error| CrabError::Internal(format!("git index path utf8: {error}")))?,
        mode: String::from_utf8(fields[0].to_vec())
            .map_err(|error| CrabError::Internal(format!("git index mode utf8: {error}")))?,
        sha: String::from_utf8(fields[1].to_vec())
            .map_err(|error| CrabError::Internal(format!("git index oid utf8: {error}")))?,
    })
}

fn remove_git_index_paths(worktree: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["update-index", "--force-remove", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git update-index stdin missing".into()))?;
        for path in paths {
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
    }
    command_child_status(child, "git update-index --force-remove")
}

fn materialize_git_policy_files(worktree: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["ls-files", "-z"])
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {}",
            command_diagnostics(&output)
        )));
    }
    let paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| {
            path.rsplit(|byte| *byte == b'/')
                .next()
                .is_some_and(|name| name == b".gitignore" || name == b".gitattributes")
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["checkout-index", "--force", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git checkout-index stdin missing".into()))?;
        for path in paths {
            stdin.write_all(path)?;
            stdin.write_all(b"\0")?;
        }
    }
    command_child_status(child, "git checkout-index policy files")
}

fn is_metadata_base_rename(record: &crate::overlay::OverlayRecord) -> bool {
    change_kind(record.kind) == OverlayChangeKind::Rename
        && record.backing_path.is_none()
        && record.base_path.is_some()
}

fn is_applied_metadata_base_rename(
    record: &crate::overlay::OverlayRecord,
    applied: &[AppliedBaseRename],
) -> bool {
    applied.iter().any(|rename| {
        record.base_path.as_deref().is_some_and(|base_path| {
            path_is_at_or_under(&record.path, &rename.new_path)
                && path_is_at_or_under(base_path, &rename.old_path)
        }) || (change_kind(record.kind) == OverlayChangeKind::Delete
            && path_is_at_or_under(&record.path, &rename.old_path))
    })
}

fn path_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

fn path_is_at_or_under(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn materialize_symlink(backing: &Path, target: &Path) -> Result<()> {
    let link_target = std::fs::read_to_string(backing)?;
    remove_existing_path(target)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    create_platform_symlink(&link_target, target)
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn copy_view_path(source: &Path, target: &Path, mode: u32) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::create_dir_all(target)?;
        return Ok(());
    }
    remove_existing_path(target)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if metadata.file_type().is_symlink() {
        let link_target = std::fs::read_link(source)?;
        create_platform_symlink_path(&link_target, target)?;
        return Ok(());
    }
    std::fs::copy(source, target)?;
    set_regular_permissions(target, mode)
}

#[cfg(unix)]
fn set_regular_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_regular_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_platform_symlink(link_target: &str, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link_target, path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_platform_symlink(_link_target: &str, _path: &Path) -> Result<()> {
    Err(CrabError::Configuration {
        key: "symlink publish is unsupported on this platform".into(),
        origin: "crab mount commit".into(),
    })
}

#[cfg(unix)]
fn create_platform_symlink_path(link_target: &Path, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link_target, path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_platform_symlink_path(_link_target: &Path, _path: &Path) -> Result<()> {
    Err(CrabError::Configuration {
        key: "symlink export is unsupported on this platform".into(),
        origin: "crab mount export".into(),
    })
}

fn crab_overlay_files(
    records: &[crate::overlay::OverlayRecord],
    worktree: &Path,
) -> Result<Vec<CrabOverlayFile>> {
    let candidates = records
        .iter()
        .filter(|record| overlay_record_has_regular_backing(record))
        .collect::<Vec<_>>();
    let candidate_paths = candidates
        .iter()
        .map(|record| record.path.clone())
        .collect::<Vec<_>>();
    let crab_paths = git_crab_filter_paths(worktree, &candidate_paths)?;
    let mut files = Vec::new();
    for record in candidates {
        if !crab_paths.contains(&record.path) {
            continue;
        }
        let source_path = record.backing_path.clone().ok_or_else(|| {
            CrabError::Internal(format!("overlay entry {} has no backing file", record.path))
        })?;
        files.push(CrabOverlayFile {
            path: record.path.clone(),
            source_path,
            mode: record.mode,
        });
    }
    Ok(files)
}

fn overlay_changes_git_attributes(records: &[crate::overlay::OverlayRecord]) -> bool {
    records.iter().any(|record| {
        is_git_attributes_path(&record.path)
            || record
                .base_path
                .as_deref()
                .is_some_and(is_git_attributes_path)
    })
}

fn is_git_attributes_path(path: &str) -> bool {
    path == ".gitattributes" || path.ends_with("/.gitattributes")
}

fn git_crab_filter_paths(
    worktree: &Path,
    paths: &[String],
) -> Result<std::collections::HashSet<String>> {
    if paths.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["check-attr", "--cached", "-z", "--stdin", "filter"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git check-attr stdin missing".into()))?;
        for path in paths {
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
    }

    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git check-attr failed: {}",
            command_diagnostics(&output)
        )));
    }

    let fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        return Err(CrabError::Internal(
            "git check-attr returned malformed output".into(),
        ));
    }

    let mut crab_paths = std::collections::HashSet::new();
    for chunk in fields.chunks(3) {
        if chunk[1] != b"filter" || chunk[2] != b"crab" {
            continue;
        }
        let path = std::str::from_utf8(chunk[0])
            .map_err(|e| CrabError::Internal(format!("git check-attr path utf8: {e}")))?;
        crab_paths.insert(path.to_owned());
    }
    Ok(crab_paths)
}

fn overlay_record_has_regular_backing(record: &crate::overlay::OverlayRecord) -> bool {
    if record.backing_path.is_none() {
        return false;
    }
    if !matches!(
        change_kind(record.kind),
        OverlayChangeKind::Create | OverlayChangeKind::Modify | OverlayChangeKind::Rename
    ) {
        return false;
    }

    let file_type = record.mode & 0o170_000;
    file_type == 0 || file_type == 0o100_000
}

fn run_git_add(
    worktree: &Path,
    records: &[OverlayRecord],
    crab_files: &[CrabOverlayFile],
    applied_base_renames: &[AppliedBaseRename],
) -> Result<()> {
    let crab_paths = crab_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut paths = std::collections::BTreeSet::new();
    for record in records {
        if is_applied_metadata_base_rename(record, applied_base_renames) {
            continue;
        }
        if !crab_paths.contains(record.path.as_str()) {
            paths.insert(record.path.clone());
        }
        if let Some(base_path) = record.base_path.as_deref()
            && base_path != record.path
        {
            paths.insert(base_path.to_owned());
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let ignored = git_ignored_paths(worktree, &paths)?;
    paths.retain(|path| !ignored.contains(path));
    if paths.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git add stdin missing".into()))?;
        for path in paths {
            stdin.write_all(b":(literal)")?;
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
    }

    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "git add failed: {}",
        command_diagnostics(&output)
    )))
}

fn git_ignored_paths(
    worktree: &Path,
    paths: &std::collections::BTreeSet<String>,
) -> Result<std::collections::HashSet<String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git check-ignore stdin missing".into()))?;
        for path in paths {
            stdin.write_all(path.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
    }
    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(CrabError::Internal(format!(
            "git check-ignore failed: {}",
            command_diagnostics(&output)
        )));
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map_err(|error| CrabError::Internal(format!("git ignored path utf8: {error}")))
        })
        .collect()
}

fn stage_crab_overlay_files(
    files: &[CrabOverlayFile],
    worktree: &Path,
    git_dir: &Path,
) -> Result<Vec<DirectPointerEntry>> {
    let files = files.to_vec();
    let worktree = worktree.to_path_buf();
    let staging_root = publish_staging_root(git_dir)?;
    block_on_publish_runtime(async move {
        let staging = StagingArea::open_blocking_default(staging_root.clone())
            .await
            .map_err(CrabError::from)?;
        let mut entries = Vec::with_capacity(files.len());
        let cancel = CancellationToken::new();

        let stage_result = Box::pin(async {
            for file in &files {
                let result = Box::pin(crab_staging::stream::stage_file_streaming_as(
                    &file.source_path,
                    &worktree,
                    Path::new(&file.path),
                    &staging,
                    crab_staging::stream::StreamStageProgress::default(),
                    &cancel,
                ))
                .await?;
                entries.push(DirectPointerEntry {
                    path: file.path.clone(),
                    abs_path: file.source_path.clone(),
                    file_hash: result.file_hash,
                    size: result.size,
                    mode: file.mode,
                    batch_id: result.batch_id,
                });
            }
            Ok(())
        })
        .await;

        if let Err(err) = stage_result {
            for entry in &entries {
                if let Err(rollback_error) = staging.rollback_batch(&entry.batch_id) {
                    tracing::warn!(
                        error = %rollback_error,
                        "failed to roll back earlier mount staging batch"
                    );
                }
            }
            let _ = staging.close().await;
            return Err(err);
        }

        if let Err(error) = staging.close().await {
            if let Ok(staging) = StagingAreaReadOnly::open(staging_root).await {
                for entry in &entries {
                    if let Err(rollback_error) = staging.rollback_batch(&entry.batch_id).await {
                        tracing::warn!(
                            error = %rollback_error,
                            "failed to roll back mount staging after close failed"
                        );
                    }
                }
            }
            return Err(CrabError::from(error));
        }
        Ok(entries)
    })
}

fn mark_staged_pointer_entries_published(
    git_dir: &Path,
    entries: &[DirectPointerEntry],
) -> Result<()> {
    let staging_root = publish_staging_root(git_dir)?;
    let batch_ids = entries
        .iter()
        .map(|entry| entry.batch_id.clone())
        .collect::<Vec<_>>();
    block_on_publish_runtime(async move {
        let staging = StagingAreaReadOnly::open(staging_root)
            .await
            .map_err(CrabError::from)?;
        for batch_id in &batch_ids {
            staging
                .mark_batch_published(batch_id)
                .map_err(CrabError::from)?;
        }
        Ok(())
    })
}

fn rollback_staged_pointer_entries(git_dir: &Path, entries: &[DirectPointerEntry]) -> Result<()> {
    let staging_root = publish_staging_root(git_dir)?;
    let batch_ids = entries
        .iter()
        .map(|entry| entry.batch_id.clone())
        .collect::<Vec<_>>();
    block_on_publish_runtime(async move {
        let staging = StagingAreaReadOnly::open(staging_root)
            .await
            .map_err(CrabError::from)?;
        for batch_id in &batch_ids {
            staging
                .rollback_batch(batch_id)
                .await
                .map_err(CrabError::from)?;
        }
        Ok(())
    })
}

fn publish_staging_root(git_dir: &Path) -> Result<PathBuf> {
    let common_dir = crab_git::discover::resolve_common_dir(git_dir);
    let repo_root = common_dir.parent().ok_or_else(|| {
        CrabError::Internal(format!(
            "git common directory has no parent: {}",
            common_dir.display()
        ))
    })?;
    Ok(repo_root.join(".crab").join("staging"))
}

fn block_on_publish_runtime<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let rt = tokio::runtime::Runtime::new()
                        .map_err(|e| CrabError::Internal(format!("tokio: {e}")))?;
                    rt.block_on(future)
                })
                .join()
                .map_err(|_| CrabError::Internal("publish runtime thread panicked".into()))?
        })
    } else {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CrabError::Internal(format!("tokio: {e}")))?;
        rt.block_on(future)
    }
}

fn write_pointer_entries_to_git_index(
    worktree: &Path,
    entries: &[DirectPointerEntry],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let shard_hints = crab_cache::ShardHintCache::load_sync(
        &crab_cache::shard_hints_path(),
    )
    .unwrap_or_else(|err| {
        debug!(error = %err, "failed to load shard-hint cache; mount publish pointers will omit hints");
        crab_cache::ShardHintCache::new()
    });

    let mut index_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let pointer = shard_hints.pointer_for(entry.file_hash, entry.size);
        let payload = pointer.serialize();
        let sha = write_pointer_blob(worktree, &payload)?;
        index_entries.push(GitIndexEntry {
            path: entry.path.clone(),
            mode: git_index_mode(entry.mode, &entry.abs_path).to_owned(),
            sha,
        });
    }

    publish_git_index_entries(worktree, &index_entries)
}

fn write_pointer_blob(worktree: &Path, payload: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git hash-object stdin missing".into()))?;
        stdin.write_all(payload)?;
    }

    command_child_stdout(child, "git hash-object")
}

fn publish_git_index_entries(worktree: &Path, entries: &[GitIndexEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("update-index")
        .arg("--add")
        .arg("--replace")
        .arg("-z")
        .arg("--index-info")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;

    {
        use std::io::Write;

        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git update-index stdin missing".into()))?;
        for entry in entries {
            stdin.write_all(entry.mode.as_bytes())?;
            stdin.write_all(b" ")?;
            stdin.write_all(entry.sha.as_bytes())?;
            stdin.write_all(b"\t")?;
            stdin.write_all(entry.path.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
    }

    command_child_status(child, "git update-index")
}

fn git_index_mode(mode: u32, abs_path: &Path) -> &'static str {
    if mode & 0o111 != 0 || worktree_file_is_executable(abs_path) {
        "100755"
    } else {
        "100644"
    }
}

#[cfg(unix)]
fn worktree_file_is_executable(abs_path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(abs_path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn worktree_file_is_executable(_abs_path: &Path) -> bool {
    false
}

fn finalize_committed_overlay(
    paths: &OverlayPaths,
    git_dir: &Path,
    commit_oid: &str,
    ref_name: &str,
    live_snapshot: Option<&SnapshotStore>,
) -> Result<()> {
    let snapshot_db = paths.cache_dir.join("snapshot.sqlite");
    if let Some(snapshot) = live_snapshot {
        snapshot.publish_generation_from_git(git_dir, commit_oid, ref_name)?;
    } else if snapshot_db.exists() {
        let snapshot = SnapshotStore::open_or_create(&snapshot_db)?;
        snapshot.publish_generation_from_git(git_dir, commit_oid, ref_name)?;
    }
    clear_committed_overlay(paths)?;
    Ok(())
}

fn clear_committed_overlay(paths: &OverlayPaths) -> Result<()> {
    if paths.db_path.exists() {
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir)?;
        store.clear()?;
        return Ok(());
    }

    OverlayStore::clean(&paths.db_path, &paths.upper_dir)
}

fn change_kind(kind: OverlayKind) -> OverlayChangeKind {
    match kind {
        OverlayKind::Create => OverlayChangeKind::Create,
        OverlayKind::Modify => OverlayChangeKind::Modify,
        OverlayKind::Delete => OverlayChangeKind::Delete,
        OverlayKind::Rename => OverlayChangeKind::Rename,
        OverlayKind::Mkdir => OverlayChangeKind::Mkdir,
        OverlayKind::Symlink => OverlayChangeKind::Symlink,
    }
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    let mut out = root.to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Forbidden {
                    path: format!("unsafe overlay path: {rel}"),
                });
            }
        }
    }
    Ok(out)
}

fn read_base_oid(cache_dir: &Path) -> Result<Option<String>> {
    let snapshot_db = cache_dir.join("snapshot.sqlite");
    if !snapshot_db.exists() {
        return Ok(None);
    }
    let store = SnapshotStore::open_existing(&snapshot_db)?;
    store.head_oid()
}

fn rev_parse(git_dir: &Path, rev: &str) -> Result<String> {
    command_stdout(
        Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["rev-parse", rev]),
        "git rev-parse",
    )
}

fn update_published_ref(
    git_dir: &Path,
    ref_name: &str,
    commit_oid: &str,
    expected_old_oid: Option<&str>,
    pushed: bool,
) -> Result<()> {
    let mut update_ref = Command::new("git");
    update_ref
        .arg("--git-dir")
        .arg(git_dir)
        .args(["update-ref", ref_name, commit_oid]);
    if let Some(old_oid) = expected_old_oid {
        update_ref.arg(old_oid);
    }

    match run_git(&mut update_ref, "git update-ref") {
        Ok(()) => Ok(()),
        Err(err) if pushed && expected_old_oid.is_some() => {
            debug!(
                ref_name,
                commit_oid,
                error = %err,
                "local ref moved after successful overlay push; aligning with pushed commit"
            );
            run_git(
                Command::new("git").arg("--git-dir").arg(git_dir).args([
                    "update-ref",
                    ref_name,
                    commit_oid,
                ]),
                "git update-ref",
            )
        }
        Err(err) => Err(err),
    }
}

fn ensure_git_commit_identity(worktree: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["var", "GIT_AUTHOR_IDENT"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(CrabError::Io)?;

    if output.status.success() {
        return Ok(());
    }

    Err(CrabError::Configuration {
        key: "set git config user.name and user.email before crab mount commit".to_owned(),
        origin: format!("git author identity ({})", command_diagnostics(&output)),
    })
}

fn run_git(command: &mut Command, label: &str) -> Result<()> {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "{label} failed: {}",
        command_diagnostics(&output)
    )))
}

fn command_stdout(command: &mut Command, label: &str) -> Result<String> {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "{label} failed: {}",
            command_diagnostics(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn command_child_status(child: std::process::Child, label: &str) -> Result<()> {
    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "{label} failed: {}",
        command_diagnostics(&output)
    )))
}

fn command_child_stdout(child: std::process::Child, label: &str) -> Result<String> {
    let output = child.wait_with_output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "{label} failed: {}",
            command_diagnostics(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn command_diagnostics(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr.trim(), stdout.trim());
    if combined.is_empty() {
        format!("exit status {}", output.status)
    } else {
        combined
    }
}

fn transaction_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("publish-{millis}")
}

fn write_transaction(cache_dir: &Path, transaction: &PublishTransaction) -> Result<()> {
    let dir = cache_dir.join("publish/transactions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", transaction.id));
    let bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|e| CrabError::Internal(format!("publish transaction json: {e}")))?;
    std::fs::write(path, bytes)?;
    Ok(())
}

struct PublishWorktree {
    git_dir: PathBuf,
    _root: tempfile::TempDir,
    path: PathBuf,
    registered: bool,
}

impl PublishWorktree {
    fn create(git_dir: &Path) -> Result<Self> {
        let root = tempfile::tempdir()?;
        Ok(Self {
            git_dir: git_dir.to_path_buf(),
            path: root.path().join("worktree"),
            _root: root,
            registered: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn mark_registered(&mut self) {
        self.registered = true;
    }
}

impl Drop for PublishWorktree {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let _ = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .stdin(std::process::Stdio::null())
            .output();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::engine::{BaseRenameEntry, OverlayWriter};
    use crab_types::pointer::Pointer;
    use crab_xet::hash::MerkleHash;

    fn temp_overlay() -> (tempfile::TempDir, OverlayPaths, OverlayStore) {
        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        (dir, paths, store)
    }

    #[test]
    fn inspect_overlay_reports_all_change_kinds_and_estimated_upload() {
        let (_dir, paths, store) = temp_overlay();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"new").unwrap();
        store.mkdir("dir", 0o040755).unwrap();
        store.remove("gone.txt").unwrap();

        let diff = inspect_overlay(&paths).unwrap();
        assert_eq!(diff.changes.len(), 3);
        assert_eq!(diff.estimated_upload_bytes, 3);
        assert_eq!(diff.deletion_count, 1);
    }

    #[test]
    fn inspect_and_export_overlay_ignore_macos_appledouble_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path().join("cache").as_path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"new").unwrap();
        store.create_file("._new.txt", 0o100644).unwrap();
        store.write_file("._new.txt", 0, b"sidecar").unwrap();
        store.create_file("nested/._other.txt", 0o100644).unwrap();
        store
            .write_file("nested/._other.txt", 0, b"sidecar")
            .unwrap();

        let diff = inspect_overlay(&paths).unwrap();
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].path, "new.txt");
        assert_eq!(diff.estimated_upload_bytes, 3);

        let export_dir = dir.path().join("export");
        let exported = export_overlay(&paths, &export_dir).unwrap();
        assert_eq!(exported.changes.len(), 1);
        assert_eq!(
            std::fs::read_to_string(export_dir.join("new.txt")).unwrap(),
            "new"
        );
        assert!(!export_dir.join("._new.txt").exists());
        assert!(!export_dir.join("nested/._other.txt").exists());
    }

    #[test]
    fn export_overlay_copies_backing_files_and_deletion_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path().join("cache").as_path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("nested/new.txt", 0o100644).unwrap();
        store.write_file("nested/new.txt", 0, b"new").unwrap();
        store.remove("gone.txt").unwrap();

        let export_dir = dir.path().join("export");
        let diff = export_overlay(&paths, &export_dir).unwrap();

        assert_eq!(diff.changes.len(), 2);
        assert_eq!(
            std::fs::read_to_string(export_dir.join("nested/new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join(DELETIONS_MANIFEST)).unwrap(),
            "gone.txt\n"
        );
    }

    #[test]
    fn export_overlay_from_view_copies_metadata_base_rename() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path().join("cache").as_path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store
            .rename_base_subtree(&[
                BaseRenameEntry {
                    old_path: "archive".to_owned(),
                    new_path: "moved-archive".to_owned(),
                    node_type: crate::snapshot::NodeType::Dir,
                    mode: 0o040755,
                    size: 0,
                    source_oid: None,
                },
                BaseRenameEntry {
                    old_path: "archive/model.bin".to_owned(),
                    new_path: "moved-archive/model.bin".to_owned(),
                    node_type: crate::snapshot::NodeType::File,
                    mode: 0o100644,
                    size: 11,
                    source_oid: Some("base-oid".to_owned()),
                },
            ])
            .unwrap();

        let view_root = dir.path().join("mounted");
        std::fs::create_dir_all(view_root.join("moved-archive")).unwrap();
        std::fs::write(view_root.join("moved-archive/model.bin"), b"base export").unwrap();
        let export_dir = dir.path().join("export");

        let diff = export_overlay_from_view(&paths, &export_dir, &view_root).unwrap();

        assert_eq!(diff.estimated_upload_bytes, 0);
        assert_eq!(
            std::fs::read_to_string(export_dir.join("moved-archive/model.bin")).unwrap(),
            "base export"
        );
        assert!(
            std::fs::read_to_string(export_dir.join(DELETIONS_MANIFEST))
                .unwrap()
                .contains("archive/model.bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_overlay_preserves_symlink_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path().join("cache").as_path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store
            .create_symlink("nested/link.txt", "../target.txt", 0o777)
            .unwrap();

        let export_dir = dir.path().join("export");
        let diff = export_overlay(&paths, &export_dir).unwrap();

        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].kind, OverlayChangeKind::Symlink);
        assert_eq!(
            std::fs::read_link(export_dir.join("nested/link.txt")).unwrap(),
            PathBuf::from("../target.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_overlay_preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = OverlayPaths::from_cache_dir(dir.path().join("cache").as_path());
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("bin/tool", 0o100755).unwrap();
        store.write_file("bin/tool", 0, b"#!/bin/sh\n").unwrap();

        let export_dir = dir.path().join("export");
        let diff = export_overlay(&paths, &export_dir).unwrap();

        assert_eq!(diff.changes.len(), 1);
        assert_eq!(
            std::fs::metadata(export_dir.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn reset_overlay_clears_entries_and_upper_dir() {
        let (_dir, paths, store) = temp_overlay();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"new").unwrap();

        let diff = reset_overlay(&paths).unwrap();

        assert_eq!(diff.changes.len(), 1);
        assert!(paths.db_path.exists());
        assert!(paths.upper_dir.exists());
        assert_eq!(store.dirty_count().unwrap(), 0);
        assert!(!paths.upper_dir.join("new.txt").exists());
    }

    #[test]
    fn reset_overlay_clears_an_open_store_view() {
        let (_dir, paths, live_store) = temp_overlay();
        live_store.create_file("new.txt", 0o100644).unwrap();
        live_store.write_file("new.txt", 0, b"new").unwrap();
        live_store.remove("gone.txt").unwrap();

        let diff = reset_overlay(&paths).unwrap();

        assert_eq!(diff.changes.len(), 2);
        assert_eq!(live_store.dirty_count().unwrap(), 0);
        assert!(live_store.get("new.txt").is_none());
        assert!(live_store.get("gone.txt").is_none());
        assert!(!paths.upper_dir.join("new.txt").exists());
    }

    #[test]
    fn frozen_overlay_rejects_writes_until_guard_drops() {
        let (_dir, _paths, store) = temp_overlay();

        {
            let _freeze = store.freeze_writes().unwrap();
            let err = store.create_file("new.txt", 0o100644).unwrap_err();
            assert!(matches!(err, CrabError::Forbidden { .. }));
        }

        store.create_file("new.txt", 0o100644).unwrap();
    }

    #[test]
    fn apply_records_skips_precomputed_crab_overlay_files() {
        let (_dir, _paths, store) = temp_overlay();
        store.create_file("models/model.bin", 0o100644).unwrap();
        store.write_file("models/model.bin", 0, b"large").unwrap();
        store.create_file("notes.txt", 0o100644).unwrap();
        store.write_file("notes.txt", 0, b"small").unwrap();
        let records = store.records().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let skipped = std::collections::HashSet::from(["models/model.bin".to_owned()]);

        apply_records(&records, worktree.path(), &skipped, &[]).unwrap();

        assert!(!worktree.path().join("models/model.bin").exists());
        assert_eq!(
            std::fs::read_to_string(worktree.path().join("notes.txt")).unwrap(),
            "small"
        );
    }

    #[test]
    fn overlay_changes_git_attributes_detects_root_and_nested_attrs() {
        let (_dir, _paths, store) = temp_overlay();
        store
            .create_file("models/.gitattributes", 0o100644)
            .unwrap();
        store
            .write_file("models/.gitattributes", 0, b"*.bin filter=crab\n")
            .unwrap();

        assert!(overlay_changes_git_attributes(&store.records().unwrap()));
    }

    #[test]
    fn commit_overlay_skips_create_then_delete_noop() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("scratch.bin", 0o100644).unwrap();
        store
            .write_file("scratch.bin", 0, &patterned_bytes(64 * 1024))
            .unwrap();
        store.remove("scratch.bin").unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.commit_oid.is_none());
        assert!(result.diff.changes.is_empty());
        assert!(!result.overlay_cleaned);
        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]),
            fixture.base_oid
        );
    }

    #[test]
    fn commit_overlay_creates_commit_updates_snapshot_and_cleans_overlay() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();

        let commit_oid = result.commit_oid.as_deref().unwrap();
        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["show", "refs/heads/main:new.txt"]),
            "overlay content"
        );
        assert_eq!(
            SnapshotStore::open_or_create(&paths.cache_dir.join("snapshot.sqlite"))
                .unwrap()
                .head_oid()
                .unwrap()
                .as_deref(),
            Some(commit_oid)
        );
        assert!(result.overlay_cleaned);
        assert_eq!(
            OverlayStore::open(&paths.db_path, &paths.upper_dir)
                .unwrap()
                .dirty_count()
                .unwrap(),
            0
        );
        assert!(!paths.upper_dir.join("new.txt").exists());
        assert!(
            paths
                .cache_dir
                .join("publish/transactions")
                .join(format!("{}.json", result.transaction_id))
                .exists()
        );
    }

    #[test]
    fn commit_overlay_pushes_existing_local_commit_when_overlay_is_clean() {
        let fixture = GitFixture::new();
        let push_remote = fixture.root.path().join("push-remote.git");
        git_in(
            fixture.root.path(),
            [
                "clone",
                "--bare",
                fixture.worktree.to_str().unwrap(),
                push_remote.to_str().unwrap(),
            ],
        );
        git_stdout(
            &fixture.bare_git_dir,
            ["config", "remote.origin.url", push_remote.to_str().unwrap()],
        );
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();

        let committed = commit_overlay(&OverlayCommitOptions {
            cache_dir: cache_dir.clone(),
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();
        let commit_oid = committed.commit_oid.unwrap();
        assert_eq!(
            git_stdout(&push_remote, ["rev-parse", "refs/heads/main"]),
            fixture.base_oid
        );

        let pushed = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish existing commit".to_owned(),
            push: true,
        })
        .unwrap();

        assert_eq!(pushed.commit_oid.as_deref(), Some(commit_oid.as_str()));
        assert!(pushed.pushed);
        assert_eq!(
            git_stdout(&push_remote, ["rev-parse", "refs/heads/main"]),
            commit_oid
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_overlay_does_not_checkout_unchanged_base_files() {
        let fixture = GitFixture::new();
        std::fs::write(
            fixture.worktree.join(".gitattributes"),
            "base.txt filter=checkout-probe\n",
        )
        .unwrap();
        git_in(&fixture.worktree, ["add", ".gitattributes"]);
        git_in(&fixture.worktree, ["commit", "-m", "add checkout probe"]);
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let base_oid = git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]);
        let marker = fixture.root.path().join("base-file-checked-out");
        let smudge = format!("sh -c 'touch {}; cat'", marker.display());
        git_stdout(
            &fixture.bare_git_dir,
            ["config", "filter.checkout-probe.smudge", &smudge],
        );

        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();

        commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(!marker.exists());
    }

    #[test]
    fn commit_overlay_respects_base_gitignore_without_full_checkout() {
        let fixture = GitFixture::new();
        std::fs::write(fixture.worktree.join(".gitignore"), "ignored.txt\n").unwrap();
        git_in(&fixture.worktree, ["add", ".gitignore"]);
        git_in(&fixture.worktree, ["commit", "-m", "add ignore policy"]);
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let base_oid = git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]);
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("ignored.txt", 0o100644).unwrap();
        store.write_file("ignored.txt", 0, b"ignored").unwrap();
        store.create_file("included.txt", 0o100644).unwrap();
        store.write_file("included.txt", 0, b"included").unwrap();

        commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(!git_tree_contains(
            &fixture.bare_git_dir,
            "refs/heads/main",
            "ignored.txt"
        ));
        assert!(git_tree_contains(
            &fixture.bare_git_dir,
            "refs/heads/main",
            "included.txt"
        ));
    }

    #[test]
    fn commit_overlay_push_failure_keeps_local_ref_retryable() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();

        let missing_remote = fixture.root.path().join("missing-origin.git");
        git_stdout(
            &fixture.bare_git_dir,
            [
                "config",
                "remote.origin.url",
                missing_remote.to_str().unwrap(),
            ],
        );
        let err = commit_overlay(&OverlayCommitOptions {
            cache_dir: cache_dir.clone(),
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: true,
        })
        .unwrap_err();

        assert!(err.to_string().contains("git push failed"));
        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]),
            fixture.base_oid
        );
        assert_eq!(inspect_overlay(&paths).unwrap().changes.len(), 1);

        let retry_remote = fixture.root.path().join("retry-origin.git");
        git_in(
            fixture.root.path(),
            [
                "clone",
                "--bare",
                fixture.worktree.to_str().unwrap(),
                retry_remote.to_str().unwrap(),
            ],
        );
        git_stdout(
            &fixture.bare_git_dir,
            [
                "config",
                "remote.origin.url",
                retry_remote.to_str().unwrap(),
            ],
        );

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay retry".to_owned(),
            push: true,
        })
        .unwrap();

        let commit_oid = result.commit_oid.as_deref().unwrap();
        assert!(result.pushed);
        assert!(result.overlay_cleaned);
        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]),
            commit_oid
        );
        assert_eq!(
            git_stdout(&retry_remote, ["show", "refs/heads/main:new.txt"]),
            "overlay content"
        );
    }

    #[test]
    fn commit_overlay_records_precommit_failure_and_preserves_overlay() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();

        let error = commit_overlay(&OverlayCommitOptions {
            cache_dir: cache_dir.clone(),
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/missing".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap_err();

        let transactions = read_publish_transactions(&cache_dir).unwrap();
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].status, "failed");
        assert_eq!(
            transactions[0].error.as_deref(),
            Some(error.to_string().as_str())
        );
        assert!(transactions[0].commit_oid.is_none());
        assert_eq!(inspect_overlay(&paths).unwrap().changes.len(), 1);
    }

    #[test]
    fn commit_overlay_recovers_recorded_pushed_transaction_and_cleans_overlay() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();
        let records = store.records().unwrap();

        std::fs::write(fixture.worktree.join("new.txt"), "overlay content").unwrap();
        git_in(&fixture.worktree, ["add", "new.txt"]);
        git_in(
            &fixture.worktree,
            ["commit", "-m", "already published overlay"],
        );
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let commit_oid = git_in_stdout(&fixture.worktree, ["rev-parse", "HEAD"]);
        let transaction_id = write_recorded_publish_transaction(
            &cache_dir,
            &fixture.base_oid,
            &commit_oid,
            true,
            &records,
        );

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir: cache_dir.clone(),
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay retry".to_owned(),
            push: true,
        })
        .unwrap();

        assert_eq!(result.transaction_id, transaction_id);
        assert_eq!(result.commit_oid.as_deref(), Some(commit_oid.as_str()));
        assert!(result.pushed);
        assert!(result.overlay_cleaned);
        assert_eq!(
            OverlayStore::open(&paths.db_path, &paths.upper_dir)
                .unwrap()
                .dirty_count()
                .unwrap(),
            0
        );
        assert!(!paths.upper_dir.join("new.txt").exists());
        assert_eq!(
            SnapshotStore::open_or_create(&paths.cache_dir.join("snapshot.sqlite"))
                .unwrap()
                .head_oid()
                .unwrap()
                .as_deref(),
            Some(commit_oid.as_str())
        );
        let transaction_path = cache_dir
            .join("publish/transactions")
            .join(format!("{transaction_id}.json"));
        let transaction: PublishTransaction =
            serde_json::from_slice(&std::fs::read(transaction_path).unwrap()).unwrap();
        assert_eq!(transaction.status, "pushed_cleaned");
        assert!(transaction.error.is_none());
    }

    #[test]
    fn commit_overlay_rejects_recorded_publish_when_overlay_changed_before_retry() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();
        let records = store.records().unwrap();

        std::fs::write(fixture.worktree.join("new.txt"), "overlay content").unwrap();
        git_in(&fixture.worktree, ["add", "new.txt"]);
        git_in(
            &fixture.worktree,
            ["commit", "-m", "already published overlay"],
        );
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let commit_oid = git_in_stdout(&fixture.worktree, ["rev-parse", "HEAD"]);
        write_recorded_publish_transaction(
            &cache_dir,
            &fixture.base_oid,
            &commit_oid,
            true,
            &records,
        );
        store.create_file("later.txt", 0o100644).unwrap();
        store.write_file("later.txt", 0, b"later change").unwrap();

        let err = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish changed overlay retry".to_owned(),
            push: true,
        })
        .unwrap_err();

        assert!(err.to_string().contains("base ref moved"));
        assert_eq!(inspect_overlay(&paths).unwrap().changes.len(), 2);
        assert_eq!(
            SnapshotStore::open_or_create(&paths.cache_dir.join("snapshot.sqlite"))
                .unwrap()
                .head_oid()
                .unwrap()
                .as_deref(),
            Some(fixture.base_oid.as_str())
        );
    }

    #[test]
    fn update_published_ref_aligns_after_successful_push_local_race() {
        let fixture = GitFixture::new();
        fixture.advance_main("local race");

        update_published_ref(
            &fixture.bare_git_dir,
            "refs/heads/main",
            &fixture.base_oid,
            Some(&fixture.base_oid),
            true,
        )
        .unwrap();

        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]),
            fixture.base_oid
        );
    }

    #[test]
    fn update_unpushed_ref_rejects_local_race() {
        let fixture = GitFixture::new();
        fixture.advance_main("local race");
        let moved_oid = git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]);

        let err = update_published_ref(
            &fixture.bare_git_dir,
            "refs/heads/main",
            &fixture.base_oid,
            Some(&fixture.base_oid),
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("git update-ref failed"));
        assert_eq!(
            git_stdout(&fixture.bare_git_dir, ["rev-parse", "refs/heads/main"]),
            moved_oid
        );
    }

    #[test]
    fn commit_overlay_preserves_renamed_directory_children() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.mkdir("old", 0o040755).unwrap();
        store.create_file("old/child.txt", 0o100644).unwrap();
        store
            .write_file("old/child.txt", 0, b"child content")
            .unwrap();
        store.rename("old", "new").unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish renamed directory".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.overlay_cleaned);
        assert_eq!(
            git_stdout(
                &fixture.bare_git_dir,
                ["show", "refs/heads/main:new/child.txt"]
            ),
            "child content"
        );
        assert!(!git_tree_contains(
            &fixture.bare_git_dir,
            "refs/heads/main",
            "old/child.txt"
        ));
    }

    #[test]
    fn commit_overlay_moves_base_directory_without_overlay_upload() {
        let fixture = GitFixture::new();
        std::fs::create_dir_all(fixture.worktree.join("models")).unwrap();
        let content = patterned_bytes(2 * 1024 * 1024);
        std::fs::write(fixture.worktree.join("models/model.bin"), &content).unwrap();
        git_in(&fixture.worktree, ["add", "models/model.bin"]);
        git_in(&fixture.worktree, ["commit", "-m", "add base model"]);
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let base_oid = git_in_stdout(&fixture.worktree, ["rev-parse", "HEAD"]);
        let model_oid = git_in_stdout(&fixture.worktree, ["rev-parse", "HEAD:models/model.bin"]);

        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(
                &base_oid,
                "refs/heads/main",
                &[
                    crate::snapshot::BaseNode {
                        path: "models".to_owned(),
                        node_type: crate::snapshot::NodeType::Dir,
                        mode: 0o040755,
                        object_oid: None,
                        pointer: None,
                        size: 0,
                    },
                    crate::snapshot::BaseNode {
                        path: "models/model.bin".to_owned(),
                        node_type: crate::snapshot::NodeType::File,
                        mode: 0o100644,
                        object_oid: Some(model_oid.clone()),
                        pointer: None,
                        size: content.len() as u64,
                    },
                ],
            )
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store
            .rename_base_subtree(&[
                BaseRenameEntry {
                    old_path: "models".to_owned(),
                    new_path: "renamed-models".to_owned(),
                    node_type: crate::snapshot::NodeType::Dir,
                    mode: 0o040755,
                    size: 0,
                    source_oid: None,
                },
                BaseRenameEntry {
                    old_path: "models/model.bin".to_owned(),
                    new_path: "renamed-models/model.bin".to_owned(),
                    node_type: crate::snapshot::NodeType::File,
                    mode: 0o100644,
                    size: content.len() as u64,
                    source_oid: Some(model_oid),
                },
            ])
            .unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish base directory rename".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.overlay_cleaned);
        assert_eq!(result.diff.estimated_upload_bytes, 0);
        assert_eq!(
            git_stdout_bytes(
                &fixture.bare_git_dir,
                ["show", "refs/heads/main:renamed-models/model.bin"]
            ),
            content
        );
        assert!(!git_tree_contains(
            &fixture.bare_git_dir,
            "refs/heads/main",
            "models/model.bin"
        ));
    }

    #[test]
    fn commit_overlay_writes_crab_tracked_file_as_pointer_and_stages_chunks() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        let content = patterned_bytes(2 * 1024 * 1024);
        store
            .create_file("models/.gitattributes", 0o100644)
            .unwrap();
        store
            .write_file(
                "models/.gitattributes",
                0,
                b"*.bin filter=crab diff=crab -text\n",
            )
            .unwrap();
        store.create_file("models/model.bin", 0o100644).unwrap();
        store.write_file("models/model.bin", 0, &content).unwrap();
        store.set_mode("models/model.bin", 0o100755).unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.overlay_cleaned);
        let pointer_bytes = git_stdout_bytes(
            &fixture.bare_git_dir,
            ["show", "refs/heads/main:models/model.bin"],
        );
        assert_ne!(pointer_bytes, content);
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, content.len() as u64);
        assert_eq!(pointer.file_hash, *blake3::hash(&content).as_bytes());
        assert_eq!(
            git_stdout(
                &fixture.bare_git_dir,
                ["ls-tree", "refs/heads/main", "models/model.bin"],
            )
            .split_whitespace()
            .next(),
            Some("100755")
        );

        let staging_root = publish_staging_root(&fixture.bare_git_dir).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (published_recipe, staged_chunks) = rt.block_on(async {
            let staging = crab_staging::StagingAreaReadOnly::open(staging_root)
                .await
                .unwrap();
            let file_hash = MerkleHash::from(pointer.file_hash);
            (
                staging.published_recipe_for_file(&file_hash).unwrap(),
                staging.chunks_for_file(&file_hash).unwrap(),
            )
        });
        assert!(published_recipe.is_some());
        assert!(!staged_chunks.is_empty());
    }

    #[test]
    fn commit_overlay_streams_stable_crab_file_from_overlay_backing() {
        let fixture = GitFixture::new();
        std::fs::create_dir_all(fixture.worktree.join("models")).unwrap();
        std::fs::write(
            fixture.worktree.join("models/.gitattributes"),
            "*.bin filter=crab diff=crab -text\n",
        )
        .unwrap();
        git_in(&fixture.worktree, ["add", "models/.gitattributes"]);
        git_in(&fixture.worktree, ["commit", "-m", "track models"]);
        git_in(
            &fixture.worktree,
            [
                "push",
                fixture.bare_git_dir.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        let base_oid = git_in_stdout(&fixture.worktree, ["rev-parse", "HEAD"]);

        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        let content = patterned_bytes(2 * 1024 * 1024);
        store.create_file("models/model.bin", 0o100755).unwrap();
        store.write_file("models/model.bin", 0, &content).unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish stable attrs overlay".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.overlay_cleaned);
        let pointer_bytes = git_stdout_bytes(
            &fixture.bare_git_dir,
            ["show", "refs/heads/main:models/model.bin"],
        );
        assert_ne!(pointer_bytes, content);
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, content.len() as u64);
        assert_eq!(pointer.file_hash, *blake3::hash(&content).as_bytes());
        assert_eq!(
            git_stdout(
                &fixture.bare_git_dir,
                ["ls-tree", "refs/heads/main", "models/model.bin"],
            )
            .split_whitespace()
            .next(),
            Some("100755")
        );

        let staging_root = publish_staging_root(&fixture.bare_git_dir).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let staged_chunks = rt.block_on(async {
            let staging = crab_staging::StagingAreaReadOnly::open(staging_root)
                .await
                .unwrap();
            staging
                .chunks_for_file(&MerkleHash::from(pointer.file_hash))
                .unwrap()
        });
        assert!(!staged_chunks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn commit_overlay_preserves_symlink_entry() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store
            .create_symlink("nested/link.txt", "../base.txt", 0o777)
            .unwrap();

        let result = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish symlink".to_owned(),
            push: false,
        })
        .unwrap();

        assert!(result.overlay_cleaned);
        assert_eq!(
            git_stdout(
                &fixture.bare_git_dir,
                ["ls-tree", "refs/heads/main", "nested/link.txt"],
            )
            .split_whitespace()
            .next(),
            Some("120000")
        );
        assert_eq!(
            git_stdout(
                &fixture.bare_git_dir,
                ["show", "refs/heads/main:nested/link.txt"]
            ),
            "../base.txt"
        );
    }

    #[test]
    fn commit_overlay_rejects_moved_base_ref_and_keeps_overlay() {
        let fixture = GitFixture::new();
        let cache_dir = fixture.root.path().join("cache");
        let paths = OverlayPaths::from_cache_dir(&cache_dir);
        let snapshot = SnapshotStore::open_or_create(&cache_dir.join("snapshot.sqlite")).unwrap();
        snapshot
            .publish_generation(&fixture.base_oid, "refs/heads/main", &[])
            .unwrap();
        let store = OverlayStore::open(&paths.db_path, &paths.upper_dir).unwrap();
        store.create_file("new.txt", 0o100644).unwrap();
        store.write_file("new.txt", 0, b"overlay content").unwrap();
        fixture.advance_main("remote change");

        let err = commit_overlay(&OverlayCommitOptions {
            cache_dir,
            git_dir: fixture.bare_git_dir.clone(),
            ref_name: "refs/heads/main".to_owned(),
            message: "publish overlay".to_owned(),
            push: false,
        })
        .unwrap_err();

        assert!(err.to_string().contains("base ref moved"));
        assert_eq!(inspect_overlay(&paths).unwrap().changes.len(), 1);
    }

    struct GitFixture {
        _git_env: crate::test_support::CleanGitEnvGuard,
        root: tempfile::TempDir,
        worktree: PathBuf,
        bare_git_dir: PathBuf,
        base_oid: String,
    }

    impl GitFixture {
        fn new() -> Self {
            let git_env = crate::test_support::CleanGitEnvGuard::new();
            let root = tempfile::tempdir().unwrap();
            let worktree = root.path().join("work");
            std::fs::create_dir(&worktree).unwrap();
            git_in(&worktree, ["init", "--initial-branch=main"]);
            git_in(&worktree, ["config", "user.name", "Crab Test"]);
            git_in(
                &worktree,
                ["config", "user.email", "crab-test@example.invalid"],
            );
            std::fs::write(worktree.join("base.txt"), "base").unwrap();
            git_in(&worktree, ["add", "base.txt"]);
            git_in(&worktree, ["commit", "-m", "initial"]);
            let base_oid = git_in_stdout(&worktree, ["rev-parse", "HEAD"]);
            let bare_git_dir = root.path().join("origin.git");
            git_in(
                root.path(),
                [
                    "clone",
                    "--bare",
                    worktree.to_str().unwrap(),
                    bare_git_dir.to_str().unwrap(),
                ],
            );
            git_stdout(&bare_git_dir, ["config", "user.name", "Crab Test"]);
            git_stdout(
                &bare_git_dir,
                ["config", "user.email", "crab-test@example.invalid"],
            );

            Self {
                _git_env: git_env,
                root,
                worktree,
                bare_git_dir,
                base_oid,
            }
        }

        fn advance_main(&self, content: &str) {
            std::fs::write(self.worktree.join("remote.txt"), content).unwrap();
            git_in(&self.worktree, ["add", "remote.txt"]);
            git_in(&self.worktree, ["commit", "-m", "remote change"]);
            git_in(
                &self.worktree,
                [
                    "push",
                    self.bare_git_dir.to_str().unwrap(),
                    "HEAD:refs/heads/main",
                ],
            );
        }
    }

    fn patterned_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|idx| (idx % 251) as u8).collect()
    }

    fn write_recorded_publish_transaction(
        cache_dir: &Path,
        base_oid: &str,
        commit_oid: &str,
        pushed: bool,
        records: &[OverlayRecord],
    ) -> String {
        let suffix = commit_oid.chars().take(12).collect::<String>();
        let id = format!("publish-recorded-{suffix}");
        let transaction = PublishTransaction {
            id: id.clone(),
            status: if pushed {
                "pushed".to_owned()
            } else {
                "committed".to_owned()
            },
            ref_name: "refs/heads/main".to_owned(),
            base_oid: Some(base_oid.to_owned()),
            commit_oid: Some(commit_oid.to_owned()),
            pushed,
            error: Some("cleanup failed".to_owned()),
            overlay_fingerprint: overlay_records_fingerprint(records),
        };
        write_transaction(cache_dir, &transaction).unwrap();
        id
    }

    fn git_in<const N: usize>(dir: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_in_stdout<const N: usize>(dir: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_stdout_bytes<const N: usize>(git_dir: &Path, args: [&str; N]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn git_stdout<const N: usize>(git_dir: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_tree_contains(git_dir: &Path, treeish: &str, path: &str) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["cat-file", "-e", &format!("{treeish}:{path}")])
            .output()
            .unwrap()
            .status
            .success()
    }
}
