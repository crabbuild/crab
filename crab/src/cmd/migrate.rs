//! `crab migrate` — rewrite history to move files into or out of crab tracking.
//!
//! Subcommands:
//! - `migrate import` — convert large files in history to crab pointers.
//! - `migrate export` — convert crab pointers back to full files.
//! - `migrate info`   — show which file patterns would benefit from migration.
//! - `migrate from-dvc` — convert a DVC pipeline to crab format.
//!
//! This is the crab equivalent of `git lfs migrate`. It rewrites git
//! history using `git filter-repo` (or a built-in tree walker) to replace
//! large blobs with crab pointer files, or vice versa.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crab_staging::{StagingArea, StagingBatchId};
use crab_types::pointer::Pointer;
use crab_workflow::{
    CachedCmd, DvcInventory, DvcMigrationJournal, LockedDep, LockedOut, LockedStage, Lockfile,
    MigrationReport, SOURCE_DESCRIPTOR_SCHEMA_VERSION, SourceDescriptor, convert_dvc_to_crab,
    inventory_project, save_source_descriptor,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct MigrationGitPointer {
    path: String,
    pointer: Pointer,
    executable: bool,
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CrabError::Io(error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(CrabError::Io)
    } else {
        fs::remove_file(path).map_err(CrabError::Io)
    }
}

struct MigrationFileSnapshot {
    path: PathBuf,
    existed: bool,
    bytes: Vec<u8>,
}

struct MigrationTreeSnapshot {
    root: PathBuf,
    existed: bool,
    directories: Vec<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>)>,
}

struct MigrationPublicationSnapshot {
    files: Vec<MigrationFileSnapshot>,
    pointers: MigrationTreeSnapshot,
    index: Option<MigrationFileSnapshot>,
}

const MIGRATION_POINTER_PUBLICATION_SCHEMA_VERSION: u16 = 1;
const MIGRATION_POINTER_PUBLICATION_MARKER: &str = "pointer-publication.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationPointerPublication {
    schema_version: u16,
    pointer_root: String,
    temporary: String,
    backup: String,
    phase: MigrationPointerPublicationPhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPointerPublicationPhase {
    Preparing,
    Ready,
    Committed,
}

impl MigrationPublicationSnapshot {
    fn capture(project_root: &Path, output_path: &Path) -> Result<Self> {
        let mut paths = vec![project_root.join("crab.lock"), output_path.to_owned()];
        paths.sort();
        paths.dedup();
        let files = paths
            .into_iter()
            .map(snapshot_file)
            .collect::<Result<Vec<_>>>()?;
        let pointers = snapshot_tree(&project_root.join(".crab/workflow/migration/pointers"))?;
        let index = migration_git_index_path(project_root)?
            .map(snapshot_file)
            .transpose()?;
        Ok(Self {
            files,
            pointers,
            index,
        })
    }

    fn restore(self) -> Result<()> {
        self.pointers.restore()?;
        for file in self.files {
            restore_file_snapshot(&file)?;
        }
        if let Some(index) = self.index {
            restore_file_snapshot(&index)?;
        }
        Ok(())
    }
}

impl MigrationTreeSnapshot {
    fn restore(self) -> Result<()> {
        if self.existed {
            remove_existing_path(&self.root)?;
            fs::create_dir_all(&self.root).map_err(CrabError::Io)?;
            for directory in self.directories {
                fs::create_dir_all(self.root.join(directory)).map_err(CrabError::Io)?;
            }
            for (relative, bytes) in self.files {
                let path = self.root.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(CrabError::Io)?;
                }
                restore_bytes(&path, &bytes)?;
            }
        } else {
            remove_existing_path(&self.root)?;
        }
        Ok(())
    }
}

fn migration_state_root(project_root: &Path) -> PathBuf {
    project_root.join(".crab/workflow/migration")
}

fn migration_pointer_marker_path(project_root: &Path) -> PathBuf {
    migration_state_root(project_root).join(MIGRATION_POINTER_PUBLICATION_MARKER)
}

fn write_migration_pointer_marker(path: &Path, marker: &MigrationPointerPublication) -> Result<()> {
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "dvc_migration_pointer_marker_path".to_owned(),
        origin: path.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let bytes = serde_json::to_vec(marker).map_err(|error| {
        CrabError::Internal(format!("serialize migration pointer marker: {error}"))
    })?;
    let temporary = parent.join(format!(
        ".{MIGRATION_POINTER_PUBLICATION_MARKER}.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(CrabError::Io)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    if let Err(error) = replace_migration_pointer_marker(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    Ok(())
}

fn replace_migration_pointer_marker(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, destination)
    }

    #[cfg(windows)]
    {
        let backup = destination.with_extension(format!("marker-backup-{}", uuid::Uuid::now_v7()));
        let had_destination = match fs::symlink_metadata(destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if had_destination {
            fs::rename(destination, &backup)?;
        }
        if let Err(error) = fs::rename(temporary, destination) {
            if had_destination {
                let _ = fs::rename(&backup, destination);
            }
            return Err(error);
        }
        if had_destination {
            let _ = fs::remove_file(backup);
        }
        Ok(())
    }
}

fn validate_migration_marker_component<'a>(value: &'a str, key: &str) -> Result<&'a Path> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: value.to_owned(),
        });
    }
    Ok(path)
}

fn migration_marker_paths(
    project_root: &Path,
    marker: &MigrationPointerPublication,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    if marker.schema_version != MIGRATION_POINTER_PUBLICATION_SCHEMA_VERSION {
        return Err(CrabError::Configuration {
            key: "dvc_migration_pointer_marker_schema".to_owned(),
            origin: marker.schema_version.to_string(),
        });
    }
    let state_root = migration_state_root(project_root);
    let pointer_root = state_root.join(validate_migration_marker_component(
        &marker.pointer_root,
        "dvc_migration_pointer_marker_root",
    )?);
    let temporary = state_root.join(validate_migration_marker_component(
        &marker.temporary,
        "dvc_migration_pointer_marker_temp",
    )?);
    let backup = state_root.join(validate_migration_marker_component(
        &marker.backup,
        "dvc_migration_pointer_marker_backup",
    )?);
    Ok((pointer_root, temporary, backup))
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CrabError::Io(error)),
    }
}

/// Recover a pointer-tree swap interrupted between its two directory renames.
///
/// The marker is written before any old pointer tree is moved. If the process
/// dies after the first rename, the next migration restores the backup and
/// removes the temporary tree. If the new tree is already in place, recovery
/// keeps it and only cleans the backup. This removes the partial-pointer-tree
/// crash window without pretending the surrounding Git-index publication is a
/// remote transaction.
fn recover_migration_pointer_publication(project_root: &Path) -> Result<()> {
    let marker_path = migration_pointer_marker_path(project_root);
    if let Ok(metadata) = fs::symlink_metadata(&marker_path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_marker_invalid".to_owned(),
                origin: marker_path.display().to_string(),
            });
        }
    }
    let bytes = match fs::read(&marker_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CrabError::Io(error)),
    };
    let marker: MigrationPointerPublication =
        serde_json::from_slice(&bytes).map_err(|error| CrabError::Configuration {
            key: "dvc_migration_pointer_marker_invalid".to_owned(),
            origin: error.to_string(),
        })?;
    let (pointer_root, temporary, backup) = migration_marker_paths(project_root, &marker)?;
    let root_exists = path_exists(&pointer_root)?;
    let temporary_exists = path_exists(&temporary)?;
    let backup_exists = path_exists(&backup)?;

    match (root_exists, temporary_exists, backup_exists, marker.phase) {
        // No swap started: the previous pointer tree is authoritative.
        (
            true,
            true,
            false,
            MigrationPointerPublicationPhase::Preparing | MigrationPointerPublicationPhase::Ready,
        ) => {
            remove_existing_path(&temporary)?;
        }
        (
            true,
            true,
            true,
            MigrationPointerPublicationPhase::Preparing | MigrationPointerPublicationPhase::Ready,
        ) => {
            remove_existing_path(&temporary)?;
            remove_existing_path(&backup)?;
        }
        // A first migration may have no previous pointer tree. Its
        // temporary tree can simply be discarded on restart.
        (
            false,
            true,
            false,
            MigrationPointerPublicationPhase::Preparing | MigrationPointerPublicationPhase::Ready,
        ) => {
            remove_existing_path(&temporary)?;
        }
        (
            _,
            false,
            false,
            MigrationPointerPublicationPhase::Preparing | MigrationPointerPublicationPhase::Ready,
        ) => {}
        // First rename completed: restore the old tree before retrying.
        (
            false,
            true,
            true,
            MigrationPointerPublicationPhase::Preparing | MigrationPointerPublicationPhase::Ready,
        ) => {
            fs::rename(&backup, &pointer_root).map_err(CrabError::Io)?;
            remove_existing_path(&temporary)?;
        }
        // New tree is in place. Keep it and finish cleanup.
        (
            true,
            false,
            true,
            MigrationPointerPublicationPhase::Ready | MigrationPointerPublicationPhase::Committed,
        ) => {
            remove_existing_path(&backup)?;
        }
        // A committed marker with no backup is already fully cleaned.
        (true, false, false, MigrationPointerPublicationPhase::Committed) => {}
        // A stale marker with no materialized tree is not recoverable safely.
        (false, _, _, _) => {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_recovery".to_owned(),
                origin: format!(
                    "pointer publication marker has no usable pointer tree: {}",
                    marker_path.display()
                ),
            });
        }
        (_, _, _, MigrationPointerPublicationPhase::Committed) => {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_recovery".to_owned(),
                origin: format!(
                    "inconsistent committed pointer marker: {}",
                    marker_path.display()
                ),
            });
        }
        _ => {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_recovery".to_owned(),
                origin: format!(
                    "inconsistent pointer publication marker: {}",
                    marker_path.display()
                ),
            });
        }
    }
    fs::remove_file(&marker_path).map_err(CrabError::Io)?;
    Ok(())
}

fn begin_migration_pointer_publication(
    project_root: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, MigrationPointerPublication)> {
    let state_root = migration_state_root(project_root);
    fs::create_dir_all(&state_root).map_err(CrabError::Io)?;
    let pointer_root = state_root.join("pointers");
    let temporary_name = format!(".pointers.tmp-{}", uuid::Uuid::now_v7());
    let backup_name = format!(".pointers.backup-{}", uuid::Uuid::now_v7());
    let temporary = state_root.join(&temporary_name);
    let backup = state_root.join(&backup_name);
    let marker = MigrationPointerPublication {
        schema_version: MIGRATION_POINTER_PUBLICATION_SCHEMA_VERSION,
        pointer_root: "pointers".to_owned(),
        temporary: temporary_name,
        backup: backup_name,
        phase: MigrationPointerPublicationPhase::Preparing,
    };
    write_migration_pointer_marker(&migration_pointer_marker_path(project_root), &marker)?;
    fs::create_dir_all(&temporary).map_err(CrabError::Io)?;
    Ok((pointer_root, temporary, backup, marker))
}

fn commit_migration_pointer_publication(
    project_root: &Path,
    pointer_root: &Path,
    temporary: &Path,
    backup: &Path,
    mut marker: MigrationPointerPublication,
) -> Result<()> {
    marker.phase = MigrationPointerPublicationPhase::Ready;
    write_migration_pointer_marker(&migration_pointer_marker_path(project_root), &marker)?;
    if path_exists(pointer_root)? {
        fs::rename(pointer_root, backup).map_err(CrabError::Io)?;
    }
    if let Err(error) = fs::rename(temporary, pointer_root) {
        if path_exists(backup)? {
            let _ = fs::rename(backup, pointer_root);
        }
        return Err(CrabError::Io(error));
    }
    marker.phase = MigrationPointerPublicationPhase::Committed;
    // If this final marker update fails, leave the marker and committed tree
    // in place for the next invocation to finish. Returning success avoids an
    // outer rollback misclassifying a completed directory swap as partial.
    if let Err(error) =
        write_migration_pointer_marker(&migration_pointer_marker_path(project_root), &marker)
    {
        tracing::warn!(error = %error, "migration pointer commit marker deferred to recovery");
        return Ok(());
    }
    if let Err(error) = remove_existing_path(backup) {
        tracing::warn!(path = %backup.display(), error = %error, "migration pointer backup cleanup deferred");
        return Ok(());
    }
    fs::remove_file(migration_pointer_marker_path(project_root)).map_err(CrabError::Io)
}

fn migration_publication_failure(
    snapshot: MigrationPublicationSnapshot,
    journal: &mut DvcMigrationJournal,
    journal_path: &Path,
    error: CrabError,
    reason: &str,
) -> CrabError {
    journal.blocking_reasons.push(reason.to_owned());
    journal.blocking_reasons.sort();
    journal.blocking_reasons.dedup();
    let rollback = snapshot.restore();
    let journal_save = journal.save_atomic(journal_path);
    match (rollback, journal_save) {
        (Ok(()), Ok(())) => error,
        (rollback, journal_save) => CrabError::Configuration {
            key: if rollback.is_err() {
                "dvc_migration_rollback_failed"
            } else {
                "dvc_migration_journal_save_failed"
            }
            .to_owned(),
            origin: format!(
                "publication failed: {error}; rollback: {}; journal: {}",
                rollback
                    .as_ref()
                    .err()
                    .map_or("ok".to_owned(), ToString::to_string),
                journal_save
                    .as_ref()
                    .err()
                    .map_or("ok".to_owned(), ToString::to_string),
            ),
        },
    }
}

fn snapshot_file(path: PathBuf) -> Result<MigrationFileSnapshot> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CrabError::Configuration {
                key: "dvc_migration_tracked_state_invalid".to_owned(),
                origin: path.display().to_string(),
            })
        }
        Ok(_) => Ok(MigrationFileSnapshot {
            bytes: fs::read(&path).map_err(CrabError::Io)?,
            path,
            existed: true,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MigrationFileSnapshot {
            path,
            existed: false,
            bytes: Vec::new(),
        }),
        Err(error) => Err(CrabError::Io(error)),
    }
}

fn snapshot_tree(root: &Path) -> Result<MigrationTreeSnapshot> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(CrabError::Configuration {
                key: "dvc_migration_pointer_state_invalid".to_owned(),
                origin: root.display().to_string(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MigrationTreeSnapshot {
            root: root.to_owned(),
            existed: false,
            directories: Vec::new(),
            files: Vec::new(),
        }),
        Err(error) => Err(CrabError::Io(error)),
        Ok(_) => {
            let mut directories = Vec::new();
            let mut files = Vec::new();
            snapshot_tree_entries(root, root, &mut directories, &mut files)?;
            Ok(MigrationTreeSnapshot {
                root: root.to_owned(),
                existed: true,
                directories,
                files,
            })
        }
    }
}

fn snapshot_tree_entries(
    root: &Path,
    directory: &Path,
    directories: &mut Vec<PathBuf>,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        let relative = path.strip_prefix(root).map_err(|error| {
            CrabError::Internal(format!("migration pointer path outside root: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_symlink".to_owned(),
                origin: path.display().to_string(),
            });
        }
        if metadata.is_dir() {
            directories.push(relative.to_owned());
            snapshot_tree_entries(root, &path, directories, files)?;
        } else if metadata.is_file() {
            files.push((relative.to_owned(), fs::read(path).map_err(CrabError::Io)?));
        } else {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_state_invalid".to_owned(),
                origin: path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn migration_git_index_path(root: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "index"])
        .current_dir(root)
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|error| CrabError::Configuration {
        key: "dvc_migration_git_index_path_invalid".to_owned(),
        origin: error.to_string(),
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(CrabError::Configuration {
            key: "dvc_migration_git_index_path_invalid".to_owned(),
            origin: "git returned an empty index path".to_owned(),
        });
    }
    let path = Path::new(value);
    Ok(Some(if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }))
}

fn restore_file_snapshot(snapshot: &MigrationFileSnapshot) -> Result<()> {
    if snapshot.existed {
        restore_bytes(&snapshot.path, &snapshot.bytes)
    } else {
        remove_existing_path(&snapshot.path)
    }
}

fn restore_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "dvc_migration_restore_path_invalid".to_owned(),
        origin: path.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let temporary = parent.join(format!(
        ".{}.rollback-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("migration"),
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(CrabError::Io)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    #[cfg(not(windows))]
    {
        return fs::rename(&temporary, path).map_err(CrabError::Io);
    }
    #[cfg(windows)]
    {
        let backup = path.with_file_name(format!(
            ".{}.rollback-backup-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("migration"),
            uuid::Uuid::now_v7()
        ));
        let had_path = fs::symlink_metadata(path).is_ok();
        if had_path {
            fs::rename(path, &backup).map_err(CrabError::Io)?;
        }
        match fs::rename(&temporary, path) {
            Ok(()) => {
                if had_path {
                    let _ = remove_existing_path(&backup);
                }
                Ok(())
            }
            Err(error) => {
                if had_path {
                    let _ = fs::rename(&backup, path);
                }
                Err(CrabError::Io(error))
            }
        }
    }
}

/// Structured-output schema for repository-aware DVC migration.
pub const DVC_MIGRATION_SCHEMA: &str = "migrate.from-dvc";

/// Options for repository-aware DVC migration.
#[derive(Debug, Clone)]
pub struct DvcMigrationOptions {
    /// Inspect and report without writing YAML, Git, Crab data, or a journal.
    pub plan: bool,
    /// Resume only when the source inventory fingerprint is unchanged.
    pub resume: bool,
    /// Structured output mode.
    pub mode: OutputMode,
    /// Explicit, credential-free DVC remote mappings in NAME=DEST form.
    pub remote_map: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct DvcMigrationResult {
    plan: bool,
    resumed: bool,
    inventory: DvcInventory,
    journal: DvcMigrationJournal,
    report: MigrationReport,
    journal_path: Option<String>,
}

/// Arguments for `crab migrate info`.
pub struct MigrateInfoArgs {
    /// Only consider files above this size threshold (bytes).
    pub above: u64,
    /// Limit output to the top N file extensions.
    pub top: usize,
}

/// Arguments for `crab migrate import`.
pub struct MigrateImportArgs {
    /// Glob patterns for files to convert to crab pointers.
    pub include: Vec<String>,
    /// Glob patterns to exclude from migration.
    pub exclude: Vec<String>,
    /// Size threshold — only migrate files above this size.
    pub above: u64,
    /// Report what would be migrated without rewriting.
    pub dry_run: bool,
    /// Rewrite all branches, not just the current one.
    pub everything: bool,
}

/// Arguments for `crab migrate export`.
pub struct MigrateExportArgs {
    /// Glob patterns for files to convert back from pointers.
    pub include: Vec<String>,
    /// Report what would be exported without rewriting.
    pub dry_run: bool,
}

/// Show migration statistics: which file types are large and would
/// benefit from crab tracking.
pub fn run_migrate_info(args: &MigrateInfoArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_migrate_info_in(&cwd, args)
}

/// Analyze the repository at `root` for migration candidates.
pub fn run_migrate_info_in(root: &Path, args: &MigrateInfoArgs) -> Result<()> {
    // Use `git rev-list --objects --all` + `git cat-file --batch-check`
    // to enumerate all blobs and their sizes.
    let output = Command::new("git")
        .args(["rev-list", "--objects", "--all"])
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git rev-list failed".into(),
            origin: root.display().to_string(),
        });
    }

    let rev_list = String::from_utf8_lossy(&output.stdout);

    // Collect (extension, total_size, count) tuples.
    let mut ext_stats: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();

    for line in rev_list.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            continue;
        }

        let oid = parts[0];
        let path = parts[1];

        // Get the blob size via cat-file.
        let cat_output = Command::new("git")
            .args(["cat-file", "-s", oid])
            .current_dir(root)
            .output()?;

        if !cat_output.status.success() {
            continue;
        }

        let size_str = String::from_utf8_lossy(&cat_output.stdout);
        let size: u64 = match size_str.trim().parse() {
            Ok(s) => s,
            Err(_) => continue,
        };

        if size < args.above {
            continue;
        }

        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("<no ext>")
            .to_owned();

        let entry = ext_stats.entry(ext).or_insert((0, 0));
        entry.0 += size;
        entry.1 += 1;
    }

    // Sort by total size descending.
    let mut sorted: Vec<(String, u64, u64)> = ext_stats
        .into_iter()
        .map(|(ext, (size, count))| (ext, size, count))
        .collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    if sorted.is_empty() {
        eprintln!("no files above {} bytes found in history", args.above);
        return Ok(());
    }

    println!("{:<15} {:>12} {:>8}", "Extension", "Total Size", "Count");
    println!("{}", "-".repeat(37));

    for (i, (ext, size, count)) in sorted.iter().enumerate() {
        if i >= args.top {
            break;
        }
        println!("*.{ext:<13} {:>12} {:>8}", format_bytes(*size), count);
    }

    Ok(())
}

/// Rewrite history to convert matching files to crab pointers.
pub fn run_migrate_import(args: &MigrateImportArgs) -> Result<()> {
    if args.include.is_empty() {
        return Err(CrabError::Configuration {
            key: "at least one --include pattern is required".into(),
            origin: "crab migrate import".into(),
        });
    }

    if args.dry_run {
        eprintln!("migrate import (dry run):");
        eprintln!("  include: {:?}", args.include);
        eprintln!("  exclude: {:?}", args.exclude);
        eprintln!("  above: {} bytes", args.above);
        eprintln!("  everything: {}", args.everything);
        eprintln!("  (no changes will be made)");
        return Ok(());
    }

    // Check that git-filter-repo is available.
    let check = Command::new("git")
        .args(["filter-repo", "--version"])
        .output();

    match check {
        Ok(o) if o.status.success() => {
            tracing::info!("git-filter-repo available, proceeding with history rewrite");
        }
        _ => {
            eprintln!(
                "error: git-filter-repo is required for history rewriting.\n\
                 Install it with: pip install git-filter-repo\n\
                 Or see: https://github.com/newren/git-filter-repo"
            );
            return Err(CrabError::Configuration {
                key: "git-filter-repo not found".into(),
                origin: "PATH".into(),
            });
        }
    }

    eprintln!(
        "crab migrate import: history rewriting is a destructive operation.\n\
         Back up your repository before proceeding.\n\
         Patterns: {:?}",
        args.include,
    );

    Err(CrabError::LfsUnsupported {
        command: "migrate import".to_owned(),
        reason: "history rewrite engine is not yet wired; no changes were made".to_owned(),
    })
}

/// Rewrite history to convert crab pointers back to full files.
pub fn run_migrate_export(args: &MigrateExportArgs) -> Result<()> {
    if args.dry_run {
        eprintln!("migrate export (dry run):");
        eprintln!("  include: {:?}", args.include);
        eprintln!("  (no changes will be made)");
        return Ok(());
    }

    Err(CrabError::LfsUnsupported {
        command: "migrate export".to_owned(),
        reason: "history rewrite engine is not yet wired; no changes were made".to_owned(),
    })
}

/// Convert a DVC pipeline (`dvc.yaml`) to `crab.yaml`.
///
/// Locates `dvc.yaml` in the given directory (or cwd), parses it,
/// converts each stage to crab format, and either writes
/// `crab.yaml` or prints to stdout.
pub fn run_migrate_from_dvc(
    dir: Option<&Path>,
    to_stdout: bool,
    output: Option<&Path>,
) -> Result<()> {
    run_migrate_from_dvc_with_options(
        dir,
        to_stdout,
        output,
        DvcMigrationOptions {
            plan: false,
            resume: false,
            mode: OutputMode::Text,
            remote_map: Vec::new(),
        },
    )
}

/// Inventory and convert a DVC project without deleting any DVC state.
pub fn run_migrate_from_dvc_with_options(
    dir: Option<&Path>,
    to_stdout: bool,
    output: Option<&Path>,
    options: DvcMigrationOptions,
) -> Result<()> {
    if to_stdout && (options.plan || options.resume) {
        return Err(CrabError::Configuration {
            key: "--stdout cannot be combined with --plan or --resume".into(),
            origin: DVC_MIGRATION_SCHEMA.into(),
        });
    }
    if to_stdout && options.mode.is_machine() {
        return Err(CrabError::Configuration {
            key: "--stdout cannot be combined with --json or --jsonl".into(),
            origin: DVC_MIGRATION_SCHEMA.into(),
        });
    }
    let dvc_path = locate_dvc_yaml(dir)?;
    let project_root = dvc_path.parent().ok_or_else(|| CrabError::Configuration {
        key: "dvc.yaml has no project root".into(),
        origin: dvc_path.display().to_string(),
    })?;
    let dvc_content = std::fs::read_to_string(&dvc_path).map_err(CrabError::Io)?;

    let (yaml_content, mut report) = convert_dvc_to_crab(&dvc_content)?;
    // Validate the generated document before stdout or any migration state is
    // touched. DVC supports stage constructs that can be serialized but are
    // not executable Crab schema, so conversion must fail closed here.
    crab_workflow::parse_yaml(&yaml_content).map_err(|error| CrabError::Configuration {
        key: "dvc_migration_output_invalid".into(),
        origin: error.to_string(),
    })?;

    if to_stdout {
        for warning in &report.warnings {
            eprintln!("warning [{}]: {}", warning.stage, warning.message);
        }
        print!("{yaml_content}");
    } else {
        let mut inventory = inventory_project(project_root)?;
        apply_remote_mappings(&mut inventory, &options.remote_map)?;
        let journal_path = project_root.join(".crab/workflow/migration/dvc.json");
        let (mut journal, resumed) = if options.resume {
            let journal = DvcMigrationJournal::load(&journal_path, &inventory.fingerprint)?;
            let expected = DvcMigrationJournal::from_inventory(&inventory);
            let journal_keys = journal
                .entries
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>();
            let expected_keys = expected
                .entries
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>();
            if journal_keys != expected_keys {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_inventory_changed".to_owned(),
                    origin: "journal entry set differs from current DVC inventory".to_owned(),
                });
            }
            (journal, true)
        } else {
            (DvcMigrationJournal::from_inventory(&inventory), false)
        };
        let out_path = match output {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => project_root.join(p),
            None => dvc_path.with_file_name("crab.yaml"),
        };
        validate_crab_yaml_output_path(project_root, &out_path)?;
        if !options.plan {
            ensure_relative_state_path(
                project_root,
                &migration_state_root(project_root),
                "dvc_migration_state",
            )?;
            recover_migration_pointer_publication(project_root)?;
            let blocking_findings = inventory
                .findings
                .iter()
                .filter(|finding| finding.blocking && !transfer_warning(&finding.code))
                .map(|finding| finding.code.as_str())
                .collect::<Vec<_>>();
            if !blocking_findings.is_empty() {
                journal.save_atomic(&journal_path)?;
                return Err(CrabError::Configuration {
                    key: "dvc_migration_preflight_blocked".into(),
                    origin: blocking_findings.join(","),
                });
            }
            if let Err(error) = ingest_verified_sources(project_root, &inventory, &mut journal) {
                if let Err(save_error) = journal.save_atomic(&journal_path) {
                    return Err(CrabError::Configuration {
                        key: "dvc_migration_journal_save_failed".into(),
                        origin: format!(
                            "transfer failed: {error}; journal save failed: {save_error}"
                        ),
                    });
                }
                return Err(error);
            }
            if let Some(entry) = journal.entries.iter().find(|entry| {
                !matches!(
                    entry.state,
                    crab_workflow::VerificationState::Verified
                        | crab_workflow::VerificationState::Accounted
                )
            }) {
                let reason = entry
                    .error_code
                    .as_deref()
                    .unwrap_or("dvc_source_unverified");
                journal.save_atomic(&journal_path)?;
                return Err(CrabError::Configuration {
                    key: "dvc_migration_data_unverified".to_owned(),
                    origin: format!("{}: {reason}", entry.key),
                });
            }
            // This checkout-local restore proves the canonical pointer and
            // staging bytes, but it is not a fresh clone from the configured
            // Crab remote. Keep cutover blocked until a remote push/clone/
            // hydrate verifier supplies that evidence.
            let verification = verify_clean_clone(project_root, &inventory, &journal)?;
            journal
                .blocking_reasons
                .push("dvc_remote_clean_clone_unverified".to_owned());
            journal.blocking_reasons.sort();
            journal.blocking_reasons.dedup();
            let publication = MigrationPublicationSnapshot::capture(project_root, &out_path)?;
            if let Err(error) = write_canonical_migration_state(
                project_root,
                &yaml_content,
                &inventory,
                &mut journal,
            ) {
                return Err(migration_publication_failure(
                    publication,
                    &mut journal,
                    &journal_path,
                    error,
                    "dvc_canonical_state_unpublished",
                ));
            }
            // The generated workflow is part of the cutover state. Write it
            // before setting the journal's safe flag so an output-path or
            // filesystem failure cannot leave a journal claiming that the
            // source may be removed while `crab.yaml` is still absent.
            if let Err(error) = write_crab_yaml(project_root, &yaml_content, &out_path) {
                return Err(migration_publication_failure(
                    publication,
                    &mut journal,
                    &journal_path,
                    error,
                    "dvc_generated_yaml_unpublished",
                ));
            }
            if inventory
                .remotes
                .iter()
                .any(|remote| remote.destination.is_some())
            {
                let remote_evidence = verify_mapped_remote_destinations(
                    project_root,
                    &out_path,
                    &inventory,
                    &journal,
                )
                .map_err(|error| {
                    journal
                        .blocking_reasons
                        .push("dvc_remote_clean_clone_unverified".to_owned());
                    journal.blocking_reasons.sort();
                    journal.blocking_reasons.dedup();
                    journal.save_atomic(&journal_path).ok();
                    error
                })?;
                journal.remote_verifications = remote_evidence;
                journal.blocking_reasons.retain(|reason| {
                    reason != "dvc_remote_destination_unverified"
                        && reason != "dvc_remote_clean_clone_unverified"
                });
            }
            if journal.git_index_published
                && !journal
                    .blocking_reasons
                    .iter()
                    .any(|reason| reason == "dvc_remote_clean_clone_unverified")
            {
                journal
                    .mark_cutover_verified(verification)
                    .map_err(|error| CrabError::Configuration {
                        key: "dvc_migration_cutover_unverified".to_owned(),
                        origin: error.to_string(),
                    })?;
            }
            journal.save_atomic(&journal_path)?;
            report.output_path = Some(out_path);
        }
        emit_dvc_migration_result(
            options.mode,
            DvcMigrationResult {
                plan: options.plan,
                resumed,
                inventory,
                journal,
                report,
                journal_path: (!options.plan).then(|| journal_path.display().to_string()),
            },
        );
        return Ok(());
    }

    print_migration_report(&report);
    Ok(())
}

fn write_canonical_migration_state(
    project_root: &Path,
    yaml_content: &str,
    inventory: &DvcInventory,
    journal: &mut DvcMigrationJournal,
) -> Result<()> {
    let pointer_root = project_root.join(".crab/workflow/migration/pointers");
    ensure_relative_state_path(project_root, &pointer_root, "dvc_migration_pointer_state")?;
    let (pointer_root, temporary, backup, marker) =
        begin_migration_pointer_publication(project_root)?;
    let mut git_pointers = Vec::new();
    for output in &inventory.outputs {
        let key = format!(
            "{}:{}:{}",
            output.declaration,
            output.path,
            output.dvc_md5.as_deref().unwrap_or("<missing>")
        );
        let Some(entry) = journal.entries.iter().find(|entry| entry.key == key) else {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_entry_missing".to_owned(),
                origin: key,
            });
        };
        let Some(hash) = entry.crab_hash.as_deref().and_then(parse_crab_hash) else {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_hash_missing".to_owned(),
                origin: output.path.clone(),
            });
        };
        let source = project_root
            .join(".crab/workflow/migration/objects")
            .join(encode_hex(&hash))
            .join("payload");
        if output.directory {
            write_directory_pointers(
                &source,
                &temporary.join(&output.path),
                &output.path,
                &mut git_pointers,
            )?;
        } else {
            let size = entry.bytes.ok_or_else(|| CrabError::Configuration {
                key: "dvc_migration_pointer_size_missing".to_owned(),
                origin: output.path.clone(),
            })?;
            let pointer = Pointer {
                file_hash: hash,
                size,
                shard_hint: None,
            };
            write_pointer_atomic(&temporary.join(&output.path), &pointer)?;
            git_pointers.push(MigrationGitPointer {
                path: output.path.clone(),
                pointer,
                executable: output.isexec,
            });
        }
    }
    write_migration_lock(project_root, yaml_content, inventory, journal)?;
    if let Ok(context) = crate::git::worktree::WorktreeContext::resolve_from_path(project_root) {
        let pointer_count = git_pointers.len();
        stage_and_publish_migration_data(
            project_root,
            &context.shared_staging_dir(),
            inventory,
            journal,
            git_pointers,
        )?;
        tracing::debug!(
            worktree = %context.current_worktree_root.display(),
            pointers = pointer_count,
            "published migrated Crab pointers to Git index"
        );
    } else {
        journal
            .blocking_reasons
            .push("dvc_git_repository_missing".to_owned());
        journal.blocking_reasons.sort();
        journal.blocking_reasons.dedup();
    }
    commit_migration_pointer_publication(project_root, &pointer_root, &temporary, &backup, marker)
}

fn stage_and_publish_migration_data(
    project_root: &Path,
    staging_root: &Path,
    inventory: &DvcInventory,
    journal: &mut DvcMigrationJournal,
    git_pointers: Vec<MigrationGitPointer>,
) -> Result<()> {
    let mut files = Vec::new();
    let objects_root = project_root.join(".crab/workflow/migration/objects");
    for output in &inventory.outputs {
        let key = format!(
            "{}:{}:{}",
            output.declaration,
            output.path,
            output.dvc_md5.as_deref().unwrap_or("<missing>")
        );
        let entry = journal
            .entries
            .iter()
            .find(|entry| entry.key == key)
            .ok_or_else(|| CrabError::Configuration {
                key: "dvc_migration_staging_entry_missing".to_owned(),
                origin: output.path.clone(),
            })?;
        let hash = entry
            .crab_hash
            .as_deref()
            .and_then(parse_crab_hash)
            .ok_or_else(|| CrabError::Configuration {
                key: "dvc_migration_staging_hash_missing".to_owned(),
                origin: output.path.clone(),
            })?;
        let source = objects_root.join(encode_hex(&hash)).join("payload");
        if output.directory {
            collect_native_stage_files(&source, Path::new(&output.path), &mut files)?;
        } else {
            let metadata = fs::symlink_metadata(&source).map_err(CrabError::Io)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_staging_source_invalid".to_owned(),
                    origin: output.path.clone(),
                });
            }
            files.push((source, PathBuf::from(&output.path)));
        }
    }

    let root = project_root.to_path_buf();
    let staging_root = staging_root.to_path_buf();
    run_migration_async(async move {
        let staging = StagingArea::open(staging_root)
            .await
            .map_err(CrabError::from)?;
        let cancel = CancellationToken::new();
        let mut batches: Vec<StagingBatchId> = Vec::with_capacity(files.len());
        for (source, repo_path) in files {
            let staged = crab_staging::stream::stage_file_streaming_as(
                &source,
                &root,
                &repo_path,
                &staging,
                crab_staging::stream::StreamStageProgress::default(),
                &cancel,
            )
            .await
            .map_err(CrabError::from)?;
            batches.push(staged.batch_id);
        }
        staging.flush_pending().await.map_err(CrabError::from)?;
        publish_migration_pointers(&root, &git_pointers)?;
        for batch in batches {
            staging
                .mark_batch_published(&batch)
                .map_err(CrabError::from)?;
        }
        Ok(())
    })?;
    journal.staging_flushed = true;
    journal.git_index_published = true;
    Ok(())
}

fn collect_native_stage_files(
    source: &Path,
    repo_prefix: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(CrabError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: "dvc_migration_staging_symlink".to_owned(),
            origin: source.display().to_string(),
        });
    }
    if metadata.is_file() {
        files.push((source.to_path_buf(), repo_prefix.to_path_buf()));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(CrabError::Configuration {
            key: "dvc_migration_staging_source_invalid".to_owned(),
            origin: source.display().to_string(),
        });
    }
    let mut has_entry = false;
    for entry in fs::read_dir(source).map_err(CrabError::Io)? {
        has_entry = true;
        let entry = entry.map_err(CrabError::Io)?;
        let name = entry.file_name();
        let child_source = entry.path();
        let child_repo_path = repo_prefix.join(name);
        collect_native_stage_files(&child_source, &child_repo_path, files)?;
    }
    if !has_entry {
        return Err(CrabError::Configuration {
            key: "dvc_migration_empty_directory".to_owned(),
            origin: source.display().to_string(),
        });
    }
    Ok(())
}

fn run_migration_async<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(CrabError::Io)?
                .block_on(future)
        })
        .join()
        .map_err(|_| CrabError::Internal("migration staging worker panicked".to_owned()))?;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CrabError::Io)?
        .block_on(future)
}

fn write_directory_pointers(
    source: &Path,
    destination: &Path,
    git_prefix: &str,
    git_pointers: &mut Vec<MigrationGitPointer>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(CrabError::Io)?;
    if !metadata.is_dir() {
        return Err(CrabError::Configuration {
            key: "dvc_migration_directory_payload_invalid".to_owned(),
            origin: source.display().to_string(),
        });
    }
    let mut has_entry = false;
    for entry in fs::read_dir(source).map_err(CrabError::Io)? {
        has_entry = true;
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let relative = path.strip_prefix(source).map_err(|error| {
            CrabError::Internal(format!("directory pointer path outside source: {error}"))
        })?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_directory_symlink".to_owned(),
                origin: path.display().to_string(),
            });
        }
        if metadata.is_dir() {
            write_directory_pointers(&path, &target, git_prefix, git_pointers)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_directory_entry_invalid".to_owned(),
                origin: path.display().to_string(),
            });
        }
        let (hash, size) = hash_file(&path)?;
        let pointer = Pointer {
            file_hash: hash,
            size,
            shard_hint: None,
        };
        write_pointer_atomic(&target, &pointer)?;
        let relative = relative_path_for_git(Path::new(git_prefix), relative)?;
        git_pointers.push(MigrationGitPointer {
            path: relative,
            pointer,
            executable: is_executable(&path).map_err(CrabError::Io)?,
        });
    }
    if !has_entry {
        return Err(CrabError::Configuration {
            key: "dvc_migration_empty_directory".to_owned(),
            origin: source.display().to_string(),
        });
    }
    Ok(())
}

fn write_pointer_atomic(path: &Path, pointer: &Pointer) -> Result<()> {
    if let Ok(existing) = fs::symlink_metadata(path) {
        if existing.file_type().is_symlink() || !existing.is_file() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_destination_invalid".to_owned(),
                origin: path.display().to_string(),
            });
        }
        if Pointer::parse(&fs::read(path).map_err(CrabError::Io)?)
            .ok()
            .as_ref()
            == Some(pointer)
        {
            return Ok(());
        }
        return Err(CrabError::Configuration {
            key: "dvc_migration_pointer_collision".to_owned(),
            origin: path.display().to_string(),
        });
    }
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "dvc_migration_pointer_path_invalid".to_owned(),
        origin: path.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("pointer"),
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let result =
        fs::write(&temporary, pointer.serialize()).and_then(|()| fs::rename(&temporary, path));
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    Ok(())
}

fn relative_path_for_git(prefix: &Path, relative: &Path) -> Result<String> {
    let path = prefix.join(relative);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(CrabError::Configuration {
            key: "dvc_migration_pointer_path_invalid".to_owned(),
            origin: path.display().to_string(),
        });
    }
    let value = path.to_string_lossy().replace('\\', "/");
    if value
        .bytes()
        .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
    {
        return Err(CrabError::Configuration {
            key: "dvc_migration_pointer_path_invalid".to_owned(),
            origin: value,
        });
    }
    Ok(value)
}

fn publish_migration_pointers(root: &Path, pointers: &[MigrationGitPointer]) -> Result<()> {
    if pointers.is_empty() {
        return Ok(());
    }
    let mut index_info = Vec::new();
    let mut paths = std::collections::BTreeSet::new();
    for entry in pointers {
        if !paths.insert(entry.path.as_str()) {
            return Err(CrabError::Configuration {
                key: "dvc_migration_pointer_collision".to_owned(),
                origin: entry.path.clone(),
            });
        }
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CrabError::Io)?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git hash-object stdin unavailable".to_owned()))?
            .write_all(&entry.pointer.serialize())
            .map_err(CrabError::Io)?;
        let output = child.wait_with_output().map_err(CrabError::Io)?;
        if !output.status.success() {
            return Err(CrabError::Configuration {
                key: "dvc_migration_git_pointer_write".to_owned(),
                origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let object_id = String::from_utf8(output.stdout)
            .map_err(|error| {
                CrabError::Internal(format!("git hash-object output invalid: {error}"))
            })?
            .trim()
            .to_owned();
        if object_id.len() != 40 || !object_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CrabError::Internal(
                "git hash-object returned an invalid object id".to_owned(),
            ));
        }
        let mode = if entry.executable { "100755" } else { "100644" };
        index_info.extend_from_slice(mode.as_bytes());
        index_info.push(b' ');
        index_info.extend_from_slice(object_id.as_bytes());
        index_info.push(b'\t');
        index_info.extend_from_slice(entry.path.as_bytes());
        index_info.push(0);
    }

    let mut update = Command::new("git")
        .args(["update-index", "--add", "--replace", "-z", "--index-info"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    update
        .stdin
        .as_mut()
        .ok_or_else(|| CrabError::Internal("git update-index stdin unavailable".to_owned()))?
        .write_all(&index_info)
        .map_err(CrabError::Io)?;
    let output = update.wait_with_output().map_err(CrabError::Io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: "dvc_migration_git_index_write".to_owned(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn parse_crab_hash(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("b3:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0_u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

struct MigrationDepResolver {
    stage_outputs: BTreeMap<(String, String), [u8; 32]>,
}

impl crab_workflow::executor::DepResolver for MigrationDepResolver {
    fn resolve_stage_out(
        &self,
        stage: &crab_workflow::StageName,
        out: &Path,
    ) -> crab_workflow::Result<Option<[u8; 32]>> {
        Ok(self
            .stage_outputs
            .get(&(
                stage.as_str().to_owned(),
                out.to_string_lossy().replace('\\', "/"),
            ))
            .copied())
    }
}

fn migration_env(stage: &crab_workflow::Stage) -> BTreeMap<String, String> {
    match &stage.env {
        crab_workflow::EnvSpec::Allowlist(vars) => vars
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect(),
        crab_workflow::EnvSpec::Inherit | crab_workflow::EnvSpec::Empty => BTreeMap::new(),
    }
}

fn migration_locked_deps(
    project_root: &Path,
    stage: &crab_workflow::Stage,
    dep_hashes: &BTreeMap<String, [u8; 32]>,
) -> Result<Vec<LockedDep>> {
    let mut deps = Vec::new();
    for dep in &stage.deps {
        let (key, path, size) = match dep {
            crab_workflow::Dep::Path(path) => {
                let key = if path.is_absolute() {
                    path.to_string_lossy().into_owned()
                } else if let Some(wdir) = stage.wdir.as_deref() {
                    wdir.join(path).to_string_lossy().into_owned()
                } else {
                    path.to_string_lossy().into_owned()
                };
                let absolute = if path.is_absolute() {
                    path.clone()
                } else {
                    project_root
                        .join(stage.wdir.as_deref().unwrap_or_else(|| Path::new("")))
                        .join(path)
                };
                let size = fs::metadata(&absolute).map_err(CrabError::Io)?.len();
                (key.clone(), PathBuf::from(key), size)
            }
            crab_workflow::Dep::StageOut { stage, out } => {
                let key = format!("{}:{}", stage.as_str(), out.to_string_lossy());
                (key.clone(), PathBuf::from(key), 0)
            }
            crab_workflow::Dep::Url { url, .. } => (url.clone(), PathBuf::from(url), 0),
            crab_workflow::Dep::CrabRef { .. }
            | crab_workflow::Dep::GitRef { .. }
            | crab_workflow::Dep::OciImage { .. } => {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_lock_dependency_unresolved".to_owned(),
                    origin: stage.name.as_str().to_owned(),
                });
            }
        };
        let Some(hash) = dep_hashes.get(&key).copied() else {
            return Err(CrabError::Configuration {
                key: "dvc_migration_lock_dependency_unresolved".to_owned(),
                origin: key,
            });
        };
        deps.push(LockedDep { path, hash, size });
    }
    Ok(deps)
}

fn write_migration_lock(
    project_root: &Path,
    yaml_content: &str,
    inventory: &DvcInventory,
    journal: &DvcMigrationJournal,
) -> Result<()> {
    let workflow = crab_workflow::parse_at(&project_root.join("crab.yaml"), yaml_content).map_err(
        |error| CrabError::Configuration {
            key: "dvc_migration_lock_workflow_invalid".to_owned(),
            origin: error.to_string(),
        },
    )?;
    let mut stage_outputs = BTreeMap::new();
    for output in &inventory.outputs {
        let Some((_, stage_name)) = output.declaration.split_once("#stages.") else {
            continue;
        };
        let key = format!(
            "{}:{}:{}",
            output.declaration,
            output.path,
            output.dvc_md5.as_deref().unwrap_or("<missing>")
        );
        let Some(entry) = journal.entries.iter().find(|entry| entry.key == key) else {
            continue;
        };
        let Some(hash) = entry.crab_hash.as_deref().and_then(parse_crab_hash) else {
            continue;
        };
        stage_outputs.insert((stage_name.to_owned(), output.path.clone()), hash);
    }
    let dep_resolver = MigrationDepResolver { stage_outputs };
    let mut lockfile = Lockfile::new();
    for stage in workflow.stages.values() {
        if stage.deps.iter().any(|dep| {
            matches!(
                dep,
                crab_workflow::Dep::CrabRef { .. }
                    | crab_workflow::Dep::GitRef { .. }
                    | crab_workflow::Dep::OciImage { .. }
                    | crab_workflow::Dep::Url { digest: None, .. }
            )
        }) {
            return Err(CrabError::Configuration {
                key: "dvc_migration_lock_dependency_unresolved".to_owned(),
                origin: stage.name.as_str().to_owned(),
            });
        }
        let dep_hashes = crab_workflow::executor::resolve_dep_hashes_with_wdir(
            &stage.name,
            &stage.deps,
            project_root,
            &dep_resolver,
            stage.wdir.as_deref(),
        )
        .map_err(|error| CrabError::Configuration {
            key: "dvc_migration_lock_dependency_unresolved".to_owned(),
            origin: error.to_string(),
        })?;
        let params = crab_workflow::params::resolve_stage_param_values_with_wdir(
            project_root,
            &workflow.params,
            &stage.params,
            stage.name.as_str(),
            stage.wdir.as_deref(),
        )
        .map_err(|error| CrabError::Configuration {
            key: "dvc_migration_lock_params_unresolved".to_owned(),
            origin: error.to_string(),
        })?;
        let mut outs = Vec::new();
        for declared in &stage.outs {
            let path = declared.path.to_string_lossy().replace('\\', "/");
            let Some(output) = inventory.outputs.iter().find(|output| output.path == path) else {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_lock_output_missing".to_owned(),
                    origin: path,
                });
            };
            let key = format!(
                "{}:{}:{}",
                output.declaration,
                output.path,
                output.dvc_md5.as_deref().unwrap_or("<missing>")
            );
            let entry = journal
                .entries
                .iter()
                .find(|entry| entry.key == key)
                .ok_or_else(|| CrabError::Configuration {
                    key: "dvc_migration_lock_entry_missing".to_owned(),
                    origin: key.clone(),
                })?;
            let hash = parse_crab_hash(entry.crab_hash.as_deref().ok_or_else(|| {
                CrabError::Configuration {
                    key: "dvc_migration_lock_hash_missing".to_owned(),
                    origin: path.clone(),
                }
            })?)
            .ok_or_else(|| CrabError::Configuration {
                key: "dvc_migration_lock_hash_invalid".to_owned(),
                origin: path.clone(),
            })?;
            outs.push(LockedOut {
                path: declared.path.clone(),
                kind: declared.kind,
                hash,
                size: entry.bytes.unwrap_or(0),
                mode: if output.isexec { 0o755 } else { 0o644 },
            });
        }
        let cached_cmd = match &stage.cmd {
            crab_workflow::Cmd::Argv(argv) => CachedCmd::Argv { argv: argv.clone() },
            crab_workflow::Cmd::Shell(shell) => CachedCmd::Shell {
                shell: shell.clone(),
            },
            crab_workflow::Cmd::ShellList(commands) => CachedCmd::ShellList {
                commands: commands.clone(),
            },
        };
        let resolved = crab_workflow::hasher::ResolvedStage {
            stage: stage.clone(),
            dep_hashes: dep_hashes.clone(),
            params: params.clone(),
            env: stage.env.clone(),
            cmd: stage.cmd.clone(),
            outs: stage.outs.clone(),
        };
        let stage_hash = crab_workflow::hasher::compute(&resolved);
        let name = stage.name.clone();
        lockfile.stages.insert(
            name,
            LockedStage {
                stage_hash,
                cmd: cached_cmd,
                deps: migration_locked_deps(project_root, stage, &dep_hashes)?,
                params,
                env: migration_env(stage),
                outs,
                metrics: Vec::new(),
                plots: Vec::new(),
                executed_at: "migration".to_owned(),
                duration_ms: 0,
                host_fingerprint: "migration".to_owned(),
                attempts: 1,
                source: "Local".to_owned(),
            },
        );
    }
    lockfile
        .save(&project_root.join("crab.lock"))
        .map_err(|error| CrabError::Configuration {
            key: "dvc_migration_lock_write".to_owned(),
            origin: error.to_string(),
        })
}

fn transfer_warning(code: &str) -> bool {
    matches!(
        code,
        // A verified working-tree copy can be imported even when its old
        // DVC cache entry is absent. The journal remains unsafe to cut over
        // until a clean clone proves every Crab object.
        "dvc_cache_object_missing"
            | "dvc_output_materialized_missing"
            // These findings stay provisional through transfer so the
            // journal can name the exact output and return the shared
            // data-unverified error without writing canonical YAML.
            | "dvc_output_checksum_missing"
            | "dvc_lock_output_missing"
    )
}

fn ingest_verified_sources(
    project_root: &Path,
    inventory: &DvcInventory,
    journal: &mut DvcMigrationJournal,
) -> Result<()> {
    let objects_root = project_root.join(".crab/workflow/migration/objects");
    for entry in &mut journal.entries {
        if !matches!(
            entry.source_kind.as_str(),
            "working-tree" | "local-cache" | "remote"
        ) {
            account_inventory_record(project_root, inventory, entry)?;
            continue;
        }
        let Some(output) = inventory.outputs.iter().find(|output| {
            format!(
                "{}:{}:{}",
                output.declaration,
                output.path,
                output.dvc_md5.as_deref().unwrap_or("<missing>")
            ) == entry.key
        }) else {
            entry.state = crab_workflow::VerificationState::Unsupported(
                "inventory output disappeared before transfer".to_owned(),
            );
            entry.error_code = Some("dvc_source_missing".to_owned());
            continue;
        };
        let lock_identity_missing = inventory.findings.iter().any(|finding| {
            finding.code == "dvc_lock_output_missing"
                && finding.source.as_deref() == Some(output.path.as_str())
        });
        if output.dvc_md5.is_none() || lock_identity_missing {
            entry.state = crab_workflow::VerificationState::PresentUnverified;
            entry.error_code = Some(
                if lock_identity_missing {
                    "dvc_lock_output_missing"
                } else {
                    "dvc_output_checksum_missing"
                }
                .to_owned(),
            );
            continue;
        }
        let source_kind = if output.materialized == crab_workflow::VerificationState::Verified {
            "working-tree"
        } else {
            "local-cache"
        };
        let source = if output.materialized == crab_workflow::VerificationState::Verified {
            project_root.join(&output.path)
        } else if output.cache == crab_workflow::VerificationState::Verified {
            let locator = output.cache_locator.as_deref().unwrap_or(&output.path);
            let locator = Path::new(locator);
            if locator.is_absolute() {
                locator.to_path_buf()
            } else {
                project_root.join(locator)
            }
        } else {
            entry.state = crab_workflow::VerificationState::Missing;
            entry.error_code = Some("dvc_source_missing".to_owned());
            continue;
        };
        if !source.exists() {
            entry.state = crab_workflow::VerificationState::Missing;
            entry.error_code = Some("dvc_source_missing".to_owned());
            continue;
        }
        let source_locator = entry.source.clone();
        let mut reconstructed_directory = TemporaryDirectoryCleanup::default();
        let source_for_hash = if output.directory
            && output.materialized != crab_workflow::VerificationState::Verified
        {
            let temporary_root = objects_root.join(".tmp");
            fs::create_dir_all(&temporary_root).map_err(CrabError::Io)?;
            let temporary = temporary_root.join(format!(
                "{}-{}",
                std::process::id(),
                entry.key.bytes().fold(0_u64, |hash, byte| {
                    hash.wrapping_mul(16_777_619).wrapping_add(u64::from(byte))
                })
            ));
            reconstructed_directory.path = Some(temporary.clone());
            crab_workflow::materialize_cached_directory(&source, &temporary)?;
            temporary
        } else {
            source.clone()
        };
        let (hash, bytes) = hash_migration_path(&source_for_hash)?;
        let object = objects_root.join(encode_hex(&hash)).join("payload");
        let object_existed = fs::symlink_metadata(&object).is_ok();
        if let Err(error) = crab_workflow::snapshot_payload(&source_for_hash, &object) {
            if object_existed {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_object_collision".into(),
                    origin: error.to_string(),
                });
            }
            return Err(error.into());
        }
        let (snapshot_hash, snapshot_bytes) = hash_migration_path(&object)?;
        let (live_hash, live_bytes) = hash_migration_path(&source_for_hash)?;
        if snapshot_hash != hash
            || snapshot_bytes != bytes
            || live_hash != hash
            || live_bytes != bytes
        {
            return Err(CrabError::Configuration {
                key: "dvc_migration_source_changed".to_owned(),
                origin: output.path.clone(),
            });
        }
        entry.source = source_locator;
        source_kind.clone_into(&mut entry.source_kind);
        entry.crab_hash = Some(format!("b3:{}", encode_hex(&hash)));
        entry.bytes = Some(bytes);
        entry.state = crab_workflow::VerificationState::Verified;
        entry.error_code = None;
        if let Some(provenance) = entry.provenance.as_ref() {
            let descriptor_id = blake3::hash(
                format!(
                    "{}\0{}\0{}",
                    provenance.kind, provenance.locator, output.path
                )
                .as_bytes(),
            )
            .to_hex()
            .to_string();
            let mut metadata = std::collections::BTreeMap::new();
            if let Some(source_path) = provenance.source_path.as_ref() {
                metadata.insert("source_path".to_owned(), source_path.clone());
            }
            let descriptor = SourceDescriptor {
                schema_version: SOURCE_DESCRIPTOR_SCHEMA_VERSION,
                id: descriptor_id.clone(),
                kind: provenance.kind.clone(),
                locator: provenance.locator.clone(),
                revision: provenance.revision.clone(),
                // DVC's MD5 is a source checksum, not a provider validator.
                // Keep it in the migration journal and use the Crab digest
                // below for content identity; never reinterpret MD5 as an
                // HTTP/object-store freshness token.
                validator: None,
                content_hash: format!("b3:{}", encode_hex(&hash)),
                size: bytes,
                target: output.path.clone(),
                metadata,
            };
            save_source_descriptor(
                &project_root
                    .join(".crab/workflow/sources")
                    .join(format!("{descriptor_id}.json")),
                &descriptor,
            )?;
        }
        reconstructed_directory.remove_now()?;
    }
    journal
        .blocking_reasons
        .retain(|reason| reason != "transfer_pending");
    journal
        .blocking_reasons
        .retain(|reason| !transfer_warning(reason));
    journal.blocking_reasons.sort();
    journal.blocking_reasons.dedup();
    Ok(())
}

fn account_inventory_record(
    project_root: &Path,
    inventory: &DvcInventory,
    entry: &mut crab_workflow::DvcJournalEntry,
) -> Result<()> {
    if entry.source_kind == "remote-descriptor" {
        let Some(name) = entry.key.strip_prefix("remote:") else {
            entry.state = crab_workflow::VerificationState::Unsupported(
                "remote inventory key is malformed".to_owned(),
            );
            entry.error_code = Some("dvc_remote_descriptor_invalid".to_owned());
            return Ok(());
        };
        let Some(remote) = inventory.remotes.iter().find(|remote| remote.name == name) else {
            entry.state = crab_workflow::VerificationState::Missing;
            entry.error_code = Some("dvc_remote_descriptor_missing".to_owned());
            return Ok(());
        };
        let bytes = serde_json::to_vec(remote).map_err(|error| {
            CrabError::Internal(format!("serialize redacted DVC remote descriptor: {error}"))
        })?;
        entry.crab_hash = Some(format!("b3:{}", blake3::hash(&bytes).to_hex()));
        entry.bytes = Some(bytes.len() as u64);
        entry.state = crab_workflow::VerificationState::Accounted;
        entry.error_code = None;
        return Ok(());
    }

    // DVC metadata and run-cache records can contain URLs, environment values,
    // or command arguments with credentials. Account their source identity by
    // digest only; never copy those files into Crab-owned migration objects.
    if matches!(entry.source_kind.as_str(), "metadata" | "run-cache") {
        let source = Path::new(&entry.source);
        let source = if source.is_absolute() {
            source.to_path_buf()
        } else {
            project_root.join(source)
        };
        let metadata = fs::symlink_metadata(&source).map_err(CrabError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            entry.state = crab_workflow::VerificationState::Unsupported(
                "inventory record is not a regular file".to_owned(),
            );
            entry.error_code = Some("dvc_inventory_record_type_unsupported".to_owned());
            return Ok(());
        }
        let (hash, bytes) = hash_file(&source)?;
        entry.crab_hash = Some(format!("b3:{}", encode_hex(&hash)));
        entry.bytes = Some(bytes);
        entry.state = crab_workflow::VerificationState::Accounted;
        entry.error_code = None;
        return Ok(());
    }

    let source = Path::new(&entry.source);
    let source = if source.is_absolute() {
        source.to_path_buf()
    } else {
        project_root.join(source)
    };
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            entry.state = crab_workflow::VerificationState::Unsupported(
                "inventory record is not a regular file".to_owned(),
            );
            entry.error_code = Some("dvc_inventory_record_type_unsupported".to_owned());
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            entry.state = crab_workflow::VerificationState::Missing;
            entry.error_code = Some("dvc_inventory_record_missing".to_owned());
            return Ok(());
        }
        Err(error) => return Err(CrabError::Io(error)),
    };
    let (hash, bytes) = hash_file(&source)?;
    let account_root = project_root.join(".crab/workflow/migration/records");
    let object = account_root
        .join(entry.source_kind.as_str())
        .join(blake3::hash(entry.key.as_bytes()).to_hex())
        .join("payload");
    crab_workflow::snapshot_payload(&source, &object)?;
    entry.crab_hash = Some(format!("b3:{}", encode_hex(&hash)));
    entry.bytes = Some(bytes.max(metadata.len()));
    entry.state = crab_workflow::VerificationState::Accounted;
    entry.error_code = None;
    Ok(())
}

/// Push the migration's staged index as a temporary ref, clone that ref via
/// the real Crab transport, and compare every migrated output before a mapped
/// DVC destination can stop blocking cutover.
///
/// The temporary ref is created from a throw-away index that contains the
/// generated workflow and lock files in addition to the pointer entries. The
/// user's current branch, index, and working tree are never rewritten. Remote
/// cleanup is part of the transaction: a failed delete keeps the migration
/// unsafe rather than silently leaving an untracked verification branch.
fn verify_mapped_remote_destinations(
    project_root: &Path,
    output_path: &Path,
    inventory: &DvcInventory,
    journal: &DvcMigrationJournal,
) -> Result<BTreeMap<String, String>> {
    let mappings = inventory
        .remotes
        .iter()
        .filter_map(|remote| {
            remote
                .destination
                .as_ref()
                .map(|destination| (remote.name.clone(), destination.clone()))
        })
        .collect::<Vec<_>>();
    if mappings.is_empty() {
        return Ok(BTreeMap::new());
    }

    let temporary_ref = format!("refs/heads/crab-migration-verify-{}", uuid::Uuid::now_v7());
    let temporary_branch = temporary_ref
        .strip_prefix("refs/heads/")
        .ok_or_else(|| CrabError::Internal("temporary migration ref is not a branch".to_owned()))?
        .to_owned();
    let commit = create_migration_verification_commit(project_root, output_path)?;
    git_update_ref(project_root, &temporary_ref, Some(&commit))?;

    let inventory_for_async = inventory.clone();
    let journal_for_async = journal.clone();
    let branch_for_async = temporary_branch;
    let temporary_ref_for_async = temporary_ref.clone();
    let mappings_for_async = mappings.clone();
    let verification = run_migration_in_dir(project_root, async move {
        let cancel = CancellationToken::new();
        let mut evidence = BTreeMap::new();
        let mut operation_error = None;
        for (name, destination) in &mappings_for_async {
            let result = verify_one_mapped_destination(
                &inventory_for_async,
                &journal_for_async,
                &branch_for_async,
                &temporary_ref_for_async,
                &destination,
                &cancel,
            )
            .await;
            match result {
                Ok(digest) => {
                    evidence.insert(name.clone(), digest);
                }
                Err(error) => {
                    operation_error = Some(error);
                    break;
                }
            }
        }
        let cleanup =
            cleanup_remote_verification_ref(&temporary_ref_for_async, &mappings_for_async, &cancel)
                .await;
        match (operation_error, cleanup) {
            (Some(error), Ok(())) => Err(error),
            (Some(error), Err(cleanup_error)) => Err(CrabError::Configuration {
                key: "dvc_remote_verification_cleanup_failed".to_owned(),
                origin: format!("verification failed: {error}; cleanup failed: {cleanup_error}"),
            }),
            (None, Err(error)) => Err(error),
            (None, Ok(())) => Ok(evidence),
        }
    });

    let local_cleanup = git_update_ref(project_root, &temporary_ref, None);
    match (verification, local_cleanup) {
        (Ok(evidence), Ok(())) => Ok(evidence),
        (Ok(_), Err(error)) => Err(CrabError::Configuration {
            key: "dvc_remote_verification_local_cleanup_failed".to_owned(),
            origin: error.to_string(),
        }),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(CrabError::Configuration {
            key: "dvc_remote_verification_local_cleanup_failed".to_owned(),
            origin: format!("verification failed: {error}; cleanup failed: {cleanup_error}"),
        }),
    }
}

async fn verify_one_mapped_destination(
    inventory: &DvcInventory,
    journal: &DvcMigrationJournal,
    branch: &str,
    temporary_ref: &str,
    destination: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    if !destination.starts_with("crab://") {
        return Err(CrabError::Configuration {
            key: "dvc_remote_destination_unsupported".to_owned(),
            origin: destination.to_owned(),
        });
    }

    let push_args = migration_push_args(
        destination,
        vec![format!("{temporary_ref}:{temporary_ref}")],
        false,
    );
    crate::cmd::push::run_push_without_terminal_output(&push_args, cancel).await?;

    let clone_parent = tempfile::tempdir().map_err(CrabError::Io)?;
    let clone_name = PathBuf::from("clean-clone");
    let clone_args = crate::cmd::clone::CloneArgs {
        url: destination.to_owned(),
        directory: Some(clone_name.clone()),
        branch: Some(branch.to_owned()),
        depth: None,
        lazy: false,
        include: Vec::new(),
        exclude: Vec::new(),
        sync_chunk_index: false,
        mode: OutputMode::Json,
    };
    crate::cmd::clone::run_clone_in(clone_parent.path(), &clone_args, cancel).await?;
    let clone_root = clone_parent.path().join(clone_name);
    verify_remote_clone_outputs(&clone_root, inventory, journal)
}

async fn cleanup_remote_verification_ref(
    temporary_ref: &str,
    mappings: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<()> {
    let mut first_error = None;
    for (_, destination) in mappings {
        if !destination.starts_with("crab://") {
            continue;
        }
        let args = migration_push_args(destination, vec![format!(":{temporary_ref}")], true);
        if let Err(error) = crate::cmd::push::run_push_without_terminal_output(&args, cancel).await
        {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn migration_push_args(
    destination: &str,
    refspecs: Vec<String>,
    force: bool,
) -> crate::cmd::push::PushArgs {
    crate::cmd::push::PushArgs {
        remote: Some(destination.to_owned()),
        refspecs,
        upload_concurrency: None,
        lock_wait_secs: None,
        manifest_cas_retries: None,
        rebase_on_non_fast_forward: false,
        rebase_retry_limit: 0,
        dry_run: false,
        force,
        follow_tags: false,
        verbose: false,
        no_incremental: false,
        no_color: true,
        json: false,
        jsonl: false,
    }
}

fn create_migration_verification_commit(project_root: &Path, output_path: &Path) -> Result<String> {
    let index_path =
        migration_git_index_path(project_root)?.ok_or_else(|| CrabError::Configuration {
            key: "dvc_remote_verification_git_index_missing".to_owned(),
            origin: project_root.display().to_string(),
        })?;
    let relative_output =
        output_path
            .strip_prefix(project_root)
            .map_err(|error| CrabError::Configuration {
                key: "dvc_remote_verification_output_invalid".to_owned(),
                origin: error.to_string(),
            })?;
    let relative_output = relative_output
        .to_str()
        .ok_or_else(|| CrabError::Configuration {
            key: "dvc_remote_verification_output_invalid".to_owned(),
            origin: output_path.display().to_string(),
        })?;
    let temporary_index = project_root.join(format!(
        ".crab/workflow/migration/.index-verify-{}",
        uuid::Uuid::now_v7()
    ));
    if let Some(parent) = temporary_index.parent() {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    fs::copy(&index_path, &temporary_index).map_err(CrabError::Io)?;
    let result = (|| {
        git_index_command(
            project_root,
            &temporary_index,
            &["update-index", "--add", "--", relative_output],
        )?;
        if project_root.join("crab.lock").is_file() {
            git_index_command(
                project_root,
                &temporary_index,
                &["update-index", "--add", "--", "crab.lock"],
            )?;
        }
        let tree = git_index_output(project_root, &temporary_index, &["write-tree"])?;
        let tree = tree.trim();
        if tree.len() != 40 || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CrabError::Configuration {
                key: "dvc_remote_verification_tree_invalid".to_owned(),
                origin: tree.to_owned(),
            });
        }
        let parent = Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(project_root)
            .output()
            .map_err(CrabError::Io)?;
        let parent = parent
            .status
            .success()
            .then(|| String::from_utf8_lossy(&parent.stdout).trim().to_owned());
        let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
        if let Some(parent) = parent.as_deref() {
            args.extend(["-p".to_owned(), parent.to_owned()]);
        }
        let mut command = Command::new("git");
        command.args(&args).current_dir(project_root);
        command.env("GIT_AUTHOR_NAME", "Crab migration verifier");
        command.env("GIT_AUTHOR_EMAIL", "crab-migration-verifier@invalid");
        command.env("GIT_COMMITTER_NAME", "Crab migration verifier");
        command.env("GIT_COMMITTER_EMAIL", "crab-migration-verifier@invalid");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CrabError::Io)?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("git commit-tree stdin unavailable".to_owned()))?
            .write_all(b"Crab DVC migration clean-clone verification\n")
            .map_err(CrabError::Io)?;
        let output = child.wait_with_output().map_err(CrabError::Io)?;
        if !output.status.success() {
            return Err(CrabError::Configuration {
                key: "dvc_remote_verification_commit_failed".to_owned(),
                origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let commit = String::from_utf8(output.stdout)
            .map_err(|error| CrabError::Configuration {
                key: "dvc_remote_verification_commit_invalid".to_owned(),
                origin: error.to_string(),
            })?
            .trim()
            .to_owned();
        if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CrabError::Configuration {
                key: "dvc_remote_verification_commit_invalid".to_owned(),
                origin: commit,
            });
        }
        Ok(commit)
    })();
    let cleanup = fs::remove_file(&temporary_index).map_err(CrabError::Io);
    match (result, cleanup) {
        (Ok(commit), Ok(())) => Ok(commit),
        (Err(error), Ok(())) => Err(error),
        (Ok(_) | Err(_), Err(error)) => Err(error),
    }
}

fn git_index_command(project_root: &Path, index: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .current_dir(project_root)
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: "dvc_remote_verification_index_failed".to_owned(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn git_index_output(project_root: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .current_dir(project_root)
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|error| CrabError::Configuration {
            key: "dvc_remote_verification_git_output_invalid".to_owned(),
            origin: error.to_string(),
        })
    } else {
        Err(CrabError::Configuration {
            key: "dvc_remote_verification_index_failed".to_owned(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn git_update_ref(project_root: &Path, reference: &str, value: Option<&str>) -> Result<()> {
    let args = value.map_or_else(
        || vec!["update-ref", "-d", reference],
        |value| vec!["update-ref", reference, value],
    );
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: "dvc_remote_verification_ref_failed".to_owned(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn run_migration_in_dir<F, T>(root: &Path, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let root = root.to_path_buf();
    let operation = async move {
        let previous = std::env::current_dir().map_err(CrabError::Io)?;
        std::env::set_current_dir(&root).map_err(CrabError::Io)?;
        let result = future.await;
        let restore = std::env::set_current_dir(previous).map_err(CrabError::Io);
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_) | Err(_), Err(error)) => Err(error),
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(operation))
        }
        Ok(_) => Err(CrabError::Configuration {
            key: "dvc_remote_verification_runtime".to_owned(),
            origin: "remote verification requires a multi-thread Tokio runtime".to_owned(),
        }),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(CrabError::Io)?
            .block_on(operation),
    }
}

fn verify_remote_clone_outputs(
    clone_root: &Path,
    inventory: &DvcInventory,
    journal: &DvcMigrationJournal,
) -> Result<String> {
    let mut evidence = Vec::new();
    for entry in &journal.entries {
        let Some(output) = inventory.outputs.iter().find(|output| {
            format!(
                "{}:{}:{}",
                output.declaration,
                output.path,
                output.dvc_md5.as_deref().unwrap_or("<missing>")
            ) == entry.key
        }) else {
            continue;
        };
        let Some(expected) = entry.crab_hash.as_deref() else {
            return Err(CrabError::Configuration {
                key: "dvc_remote_verification_hash_missing".to_owned(),
                origin: entry.key.clone(),
            });
        };
        let materialized = clone_root.join(&output.path);
        let (actual, size) = hash_migration_path(&materialized)?;
        let executable = is_executable(&materialized).map_err(CrabError::Io)?;
        if format!("b3:{}", encode_hex(&actual)) != expected
            || entry.bytes != Some(size)
            || (!output.directory && executable != output.isexec)
        {
            return Err(CrabError::Configuration {
                key: "dvc_remote_verification_mismatch".to_owned(),
                origin: output.path.clone(),
            });
        }
        evidence.push(format!(
            "{}\0{}\0{}\0{}",
            output.path, expected, size, output.isexec
        ));
    }
    evidence.sort();
    Ok(blake3::hash(evidence.join("\n").as_bytes())
        .to_hex()
        .to_string())
}

fn verify_clean_clone(
    project_root: &Path,
    inventory: &DvcInventory,
    journal: &DvcMigrationJournal,
) -> Result<String> {
    let verification_root = project_root.join(format!(
        ".crab/workflow/migration/clean-clone-{}",
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&verification_root).map_err(CrabError::Io)?;
    let result = (|| {
        let objects_root = project_root.join(".crab/workflow/migration/objects");
        let mut evidence = Vec::new();
        for entry in &journal.entries {
            let Some(output) = inventory.outputs.iter().find(|output| {
                format!(
                    "{}:{}:{}",
                    output.declaration,
                    output.path,
                    output.dvc_md5.as_deref().unwrap_or("<missing>")
                ) == entry.key
            }) else {
                continue;
            };
            let Some(crab_hash) = entry.crab_hash.as_deref() else {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_clean_clone_missing_hash".to_owned(),
                    origin: entry.key.clone(),
                });
            };
            if !matches!(entry.state, crab_workflow::VerificationState::Verified) {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_clean_clone_unverified".to_owned(),
                    origin: entry.key.clone(),
                });
            }
            let digest = crab_hash
                .strip_prefix("b3:")
                .ok_or_else(|| CrabError::Configuration {
                    key: "dvc_migration_clean_clone_hash_invalid".to_owned(),
                    origin: entry.key.clone(),
                })?;
            let source = objects_root.join(digest).join("payload");
            let destination = verification_root.join(&output.path);
            crab_workflow::snapshot_payload(&source, &destination)?;
            let (actual, size) = if destination.is_file() {
                hash_file(&destination)?
            } else {
                let tree = crab_workflow::hasher::hash_directory(&destination, false)?;
                (
                    tree.hash,
                    tree.manifest.iter().map(|entry| entry.size).sum(),
                )
            };
            // DVC's top-level `isexec` applies to a file output. Directory
            // manifests carry executable bits per member; the canonical tree
            // hash above already verifies those modes, while the directory's
            // own filesystem mode is not part of the DVC output contract.
            let mode_matches = destination.is_dir()
                || is_executable(&destination).map_err(CrabError::Io)? == output.isexec;
            if format!("b3:{}", encode_hex(&actual)) != crab_hash
                || entry.bytes != Some(size)
                || !mode_matches
            {
                return Err(CrabError::Configuration {
                    key: "dvc_migration_clean_clone_mismatch".to_owned(),
                    origin: output.path.clone(),
                });
            }
            evidence.push(format!(
                "{}\0{}\0{}\0{}",
                output.path, crab_hash, size, output.isexec
            ));
        }
        evidence.sort();
        Ok(blake3::hash(evidence.join("\n").as_bytes())
            .to_hex()
            .to_string())
    })();
    let cleanup = fs::remove_dir_all(&verification_root).map_err(CrabError::Io);
    match (result, cleanup) {
        (Ok(digest), Ok(())) => Ok(digest),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

#[derive(Default)]
struct TemporaryDirectoryCleanup {
    path: Option<PathBuf>,
}

impl TemporaryDirectoryCleanup {
    fn remove_now(&mut self) -> Result<()> {
        let Some(path) = self.path.take() else {
            return Ok(());
        };
        fs::remove_dir_all(path).map_err(CrabError::Io)
    }
}

impl Drop for TemporaryDirectoryCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = File::open(path).map_err(CrabError::Io)?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    Ok((hasher.finalize().into(), bytes))
}

fn hash_migration_path(path: &Path) -> Result<([u8; 32], u64)> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: "dvc_migration_source_symlink".to_owned(),
            origin: path.display().to_string(),
        });
    }
    if metadata.is_file() {
        return hash_file(path);
    }
    if metadata.is_dir() {
        let tree = crab_workflow::hasher::hash_directory(path, false)?;
        let bytes = tree.manifest.iter().map(|entry| entry.size).sum();
        return Ok((tree.hash, bytes));
    }
    Err(CrabError::Configuration {
        key: "dvc_migration_source_type_invalid".to_owned(),
        origin: path.display().to_string(),
    })
}

fn is_executable(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = fs::metadata(path)?;
        Ok(false)
    }
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from(*bytes).to_hex().to_string()
}

fn emit_dvc_migration_result(mode: OutputMode, result: DvcMigrationResult) {
    match mode {
        OutputMode::Text => print_dvc_inventory_report(&result),
        OutputMode::Json => emit_json(DVC_MIGRATION_SCHEMA, "1.0", result),
        OutputMode::Jsonl => {
            let mut stream = JsonlStream::new("migrate.from-dvc.event", "1.0", std::io::stdout());
            stream.emit_result(result);
        }
    }
}

fn apply_remote_mappings(inventory: &mut DvcInventory, mappings: &[String]) -> Result<()> {
    let mut parsed = std::collections::BTreeMap::new();
    for mapping in mappings {
        let Some((name, destination)) = mapping.split_once('=') else {
            return Err(CrabError::Configuration {
                key: "dvc_remote_map_invalid".into(),
                origin: mapping.clone(),
            });
        };
        let destination = destination.trim();
        let lower_destination = destination.to_ascii_lowercase();
        let destination_has_secret = destination.contains('@')
            || [
                "token=",
                "secret=",
                "password=",
                "access_key=",
                "signature=",
                "credential=",
                "x-amz-",
            ]
            .iter()
            .any(|marker| lower_destination.contains(marker));
        let destination_url_has_query = url::Url::parse(destination)
            .ok()
            .is_some_and(|url| url.query().is_some() || url.fragment().is_some());
        if name.trim().is_empty()
            || destination.is_empty()
            || destination_has_secret
            || destination_url_has_query
        {
            return Err(CrabError::Configuration {
                key: "dvc_remote_map_secret_or_empty".into(),
                origin: name.to_owned(),
            });
        }
        if parsed
            .insert(name.trim().to_owned(), destination.to_owned())
            .is_some()
        {
            return Err(CrabError::Configuration {
                key: "dvc_remote_map_duplicate".into(),
                origin: name.to_owned(),
            });
        }
    }
    for remote in &mut inventory.remotes {
        if let Some(destination) = parsed.remove(&remote.name) {
            remote.destination = Some(destination);
            // A mapping only identifies the intended Crab destination. Until
            // the destination has been resolved, populated, and verified by a
            // live transfer, it cannot make DVC removal safe.
            "mapped;destination-unverified".clone_into(&mut remote.capability);
            inventory.findings.retain(|finding| {
                !(finding.code == "dvc_remote_unmapped"
                    && finding.source.as_deref() == Some(remote.name.as_str()))
            });
            inventory.findings.push(crab_workflow::DvcFinding {
                code: "dvc_remote_destination_unverified".to_owned(),
                source: Some(remote.name.clone()),
                detail: "Crab destination mapping is recorded but has not been live-validated or populated by migration".to_owned(),
                blocking: true,
            });
        }
    }
    for (name, _) in parsed {
        inventory.findings.push(crab_workflow::DvcFinding {
            code: "dvc_remote_map_unknown".to_owned(),
            source: Some(name.clone()),
            detail: "mapping names a remote that is not present in DVC config".to_owned(),
            blocking: true,
        });
    }
    inventory.findings.sort_by(|left, right| {
        left.code
            .cmp(&right.code)
            .then_with(|| left.source.cmp(&right.source))
    });
    let remotes = serde_json::to_vec(&inventory.remotes)
        .map_err(|error| CrabError::Internal(format!("serialize DVC remote mappings: {error}")))?;
    let mut fingerprint = Vec::with_capacity(inventory.fingerprint.len() + remotes.len() + 1);
    fingerprint.extend_from_slice(inventory.fingerprint.as_bytes());
    fingerprint.push(0);
    fingerprint.extend_from_slice(&remotes);
    inventory.fingerprint = blake3::hash(&fingerprint).to_hex().to_string();
    Ok(())
}

fn print_dvc_inventory_report(result: &DvcMigrationResult) {
    print_migration_report(&result.report);
    println!(
        "DVC metadata files: {}",
        result.inventory.metadata_files.len()
    );
    println!("DVC outputs: {}", result.inventory.outputs.len());
    println!("DVC cache objects: {}", result.inventory.cache_object_count);
    println!("DVC remotes: {}", result.inventory.remotes.len());
    println!(
        "Safe to remove .dvc/: {}",
        result.journal.safe_to_remove_dvc
    );
    if result.plan {
        println!("Plan only: no Crab YAML, journal, Git, or data mutations were made.");
    } else if let Some(path) = &result.journal_path {
        println!("Migration journal: {path}");
    }
    if !result.inventory.findings.is_empty() {
        println!("Findings ({}):", result.inventory.findings.len());
        for finding in &result.inventory.findings {
            let source = finding
                .source
                .as_deref()
                .map(|value| format!(" [{value}]"))
                .unwrap_or_default();
            println!("  {}{}: {}", finding.code, source, finding.detail);
        }
    }
}

fn locate_dvc_yaml(dir: Option<&Path>) -> Result<PathBuf> {
    let base = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(CrabError::Io)?,
    };
    let candidate = base.join("dvc.yaml");
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(CrabError::Configuration {
            key: "dvc.yaml not found".into(),
            origin: base.display().to_string(),
        })
    }
}

fn ensure_relative_state_path(root: &Path, path: &Path, key: &str) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| CrabError::Configuration {
            key: format!("{key}_outside_project"),
            origin: path.display().to_string(),
        })?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(CrabError::Configuration {
                key: format!("{key}_symlink"),
                origin: current.display().to_string(),
            });
        }
    }
    Ok(())
}

fn write_crab_yaml(project_root: &Path, content: &str, output_path: &Path) -> Result<()> {
    validate_crab_yaml_output_path(project_root, output_path)?;
    let parent = output_path
        .parent()
        .ok_or_else(|| CrabError::Configuration {
            key: "dvc_migration_output_parent".into(),
            origin: output_path.display().to_string(),
        })?;
    std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("crab.yaml"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(CrabError::Io)?;
    if let Err(error) = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }

    let output_exists = match std::fs::symlink_metadata(output_path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(CrabError::Io(error));
        }
    };
    let backup = output_exists.then(|| {
        parent.join(format!(
            ".{}.backup-{}",
            output_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("crab.yaml"),
            uuid::Uuid::now_v7()
        ))
    });
    if let Some(backup) = &backup
        && let Err(error) = std::fs::rename(output_path, backup)
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    if let Err(error) = std::fs::rename(&temporary, output_path) {
        if let Some(backup) = &backup {
            let _ = std::fs::rename(backup, output_path);
        }
        let _ = std::fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    if let Some(backup) = backup
        && let Err(error) = std::fs::remove_file(&backup)
    {
        tracing::warn!(path = %backup.display(), error = %error, "migration backup cleanup deferred");
    }
    Ok(())
}

fn validate_crab_yaml_output_path(project_root: &Path, output_path: &Path) -> Result<()> {
    if output_path.is_absolute() && !output_path.starts_with(project_root) {
        return Err(CrabError::Configuration {
            key: "dvc_migration_output_outside_project".to_owned(),
            origin: output_path.display().to_string(),
        });
    }
    ensure_relative_state_path(project_root, output_path, "dvc_migration_output_path")?;
    Ok(())
}

fn print_migration_report(report: &MigrationReport) {
    println!("Migration Report");
    println!("{}", "=".repeat(50));
    println!("Stages converted: {}", report.stages_converted);

    if let Some(path) = &report.output_path {
        println!("Output written to: {}", path.display());
    }

    if report.warnings.is_empty() {
        println!("Warnings: none");
    } else {
        println!("Warnings ({}):", report.warnings.len());
        for warning in &report.warnings {
            println!("  [{}] {}", warning.stage, warning.message);
        }
    }
    println!("{}", "=".repeat(50));
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migrate_import_requires_include_patterns() {
        let args = MigrateImportArgs {
            include: vec![],
            exclude: vec![],
            above: 0,
            dry_run: false,
            everything: false,
        };
        let result = run_migrate_import(&args);
        assert!(result.is_err());
    }

    #[test]
    fn migrate_import_dry_run_succeeds() {
        let args = MigrateImportArgs {
            include: vec!["*.bin".into()],
            exclude: vec![],
            above: 1024,
            dry_run: true,
            everything: false,
        };
        let result = run_migrate_import(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn migrate_export_dry_run_succeeds() {
        let args = MigrateExportArgs {
            include: vec!["*.bin".into()],
            dry_run: true,
        };
        let result = run_migrate_export(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn migrate_export_non_dry_run_fails_closed_until_rewrite_engine_exists() {
        let args = MigrateExportArgs {
            include: vec!["*.bin".into()],
            dry_run: false,
        };
        let result = run_migrate_export(&args);
        assert!(matches!(result, Err(CrabError::LfsUnsupported { .. })));
    }

    #[test]
    fn migrate_from_dvc_writes_parseable_crab_yaml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            r"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl:
          md5: 20f35e630daf44dbfa4c3f68f5399d8c
",
        )
        .unwrap();
        std::fs::write(tmp.path().join("model.pkl"), b"model").unwrap();

        let output = tmp.path().join("crab.yaml");
        run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap();

        let yaml = std::fs::read_to_string(output).unwrap();
        let workflow = crab_workflow::parse_yaml(&yaml).unwrap();
        assert!(
            workflow
                .stages
                .contains_key(&crab_workflow::StageName::parse("train").unwrap())
        );
        assert!(!migration_pointer_marker_path(tmp.path()).exists());
        assert!(
            !tmp.path()
                .join(".crab/workflow/migration")
                .read_dir()
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".pointers.")
                })
        );
    }

    #[test]
    fn migrate_from_dvc_uses_cache_only_source_locator() {
        let tmp = TempDir::new().unwrap();
        let bytes = b"cached-model";
        let md5 = "8809ab4086400c77a1c322f42ced3ef7";
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            "stages:\n  train:\n    cmd: python train.py\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("model.pkl.dvc"),
            format!(
                "outs:\n  - path: model.pkl\n    md5: {md5}\n    size: {}\n",
                bytes.len()
            ),
        )
        .unwrap();
        let cache = tmp
            .path()
            .join(format!(".dvc/cache/files/md5/{}/{}", &md5[..2], &md5[2..]));
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, bytes).unwrap();

        let output = tmp.path().join("crab.yaml");
        run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap();
        let journal =
            std::fs::read_to_string(tmp.path().join(".crab/workflow/migration/dvc.json")).unwrap();
        assert!(journal.contains("\"source_kind\": \"local-cache\""));
        assert!(tmp.path().join(".crab/workflow/migration/objects").is_dir());
    }

    #[test]
    fn migration_accounts_secret_bearing_metadata_without_copying_it() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".dvc")).unwrap();
        std::fs::write(
            tmp.path().join(".dvc/config.local"),
            "[core]\ncredential_hint = super-secret\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            "stages:\n  train:\n    cmd: python train.py\n    outs:\n      - path: model.pkl\n        md5: 20f35e630daf44dbfa4c3f68f5399d8c\n        size: 5\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("model.pkl"), b"model").unwrap();

        run_migrate_from_dvc(Some(tmp.path()), false, None).unwrap();

        let migration_root = tmp.path().join(".crab/workflow/migration");
        let mut stack = vec![migration_root.clone()];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.is_file() {
                    let bytes = std::fs::read(path).unwrap();
                    assert!(
                        !bytes
                            .windows(b"super-secret".len())
                            .any(|window| { window == b"super-secret" })
                    );
                }
            }
        }
    }

    #[test]
    fn migrate_from_dvc_publishes_canonical_pointer_to_git_index() {
        let tmp = TempDir::new().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        if !init.success() {
            return;
        }
        let content = b"model";
        let md5 = "20f35e630daf44dbfa4c3f68f5399d8c";
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            format!(
                "stages:\n  train:\n    cmd: python train.py\n    outs:\n      - path: model.pkl\n        md5: {md5}\n        size: {}\n",
                content.len()
            ),
        )
        .unwrap();
        std::fs::write(tmp.path().join("model.pkl"), content).unwrap();

        run_migrate_from_dvc(Some(tmp.path()), false, None).unwrap();

        let indexed = Command::new("git")
            .args(["show", ":model.pkl"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(indexed.status.success());
        let pointer = Pointer::parse(&indexed.stdout).unwrap();
        assert_eq!(pointer.size, content.len() as u64);
        assert_eq!(pointer.file_hash, *blake3::hash(content).as_bytes());

        let journal: DvcMigrationJournal = serde_json::from_slice(
            &std::fs::read(tmp.path().join(".crab/workflow/migration/dvc.json")).unwrap(),
        )
        .unwrap();
        assert!(journal.git_index_published);
        assert!(!journal.safe_to_remove_dvc);
        assert!(
            journal
                .blocking_reasons
                .contains(&"dvc_remote_clean_clone_unverified".to_owned())
        );
        assert!(tmp.path().join("dvc.yaml").is_file());
    }

    #[test]
    fn clean_clone_verification_accepts_directory_output_mode() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("member.bin"), b"member").unwrap();
        let tree = crab_workflow::hasher::hash_directory(&source, false).unwrap();
        let crab_hash = format!("b3:{}", encode_hex(&tree.hash));
        let payload = tmp
            .path()
            .join(".crab/workflow/migration/objects")
            .join(encode_hex(&tree.hash))
            .join("payload");
        std::fs::create_dir_all(payload.parent().unwrap()).unwrap();
        std::fs::rename(source, &payload).unwrap();
        let dvc_md5 = "20f35e630daf44dbfa4c3f68f5399d8c".to_owned();
        let key = format!("pointer:data:{}", dvc_md5);
        let inventory = DvcInventory {
            schema_version: crab_workflow::DVC_INVENTORY_SCHEMA_VERSION,
            metadata_files: Vec::new(),
            outputs: vec![crab_workflow::DvcOutputRecord {
                declaration: "pointer".to_owned(),
                path: "data".to_owned(),
                dvc_md5: Some(dvc_md5.clone()),
                size: Some(6),
                directory: true,
                isexec: false,
                materialized: crab_workflow::VerificationState::Verified,
                cache: crab_workflow::VerificationState::Missing,
                provenance: None,
                cache_locator: None,
                materialized_bytes: Some(6),
            }],
            remotes: Vec::new(),
            lock_records: Vec::new(),
            cache_object_count: 0,
            cache_objects: Vec::new(),
            cache_roots: Vec::new(),
            run_cache_files: Vec::new(),
            ignore_files: Vec::new(),
            fingerprint: "fingerprint".to_owned(),
            findings: Vec::new(),
            safe_to_remove_dvc: false,
        };
        let journal = DvcMigrationJournal {
            schema_version: crab_workflow::DVC_MIGRATION_JOURNAL_SCHEMA_VERSION,
            inventory_fingerprint: inventory.fingerprint.clone(),
            entries: vec![crab_workflow::DvcJournalEntry {
                key,
                source: "data".to_owned(),
                dvc_md5: Some(dvc_md5),
                crab_hash: Some(crab_hash),
                state: crab_workflow::VerificationState::Verified,
                error_code: None,
                source_kind: "working-tree".to_owned(),
                bytes: Some(6),
                provenance: None,
            }],
            blocking_reasons: Vec::new(),
            safe_to_remove_dvc: false,
            cutover_verification: None,
            git_index_published: false,
            staging_flushed: false,
            remote_verifications: BTreeMap::new(),
        };

        assert!(verify_clean_clone(tmp.path(), &inventory, &journal).is_ok());
    }

    #[test]
    fn migration_publication_failure_restores_tracked_state() {
        let tmp = TempDir::new().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        if !init.success() {
            return;
        }
        let content = b"model";
        let md5 = "20f35e630daf44dbfa4c3f68f5399d8c";
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            format!(
                "stages:\n  train:\n    cmd: python train.py\n    outs:\n      - path: model.pkl\n        md5: {md5}\n        size: {}\n",
                content.len()
            ),
        )
        .unwrap();
        std::fs::write(tmp.path().join("model.pkl"), content).unwrap();
        std::fs::write(tmp.path().join("crab.lock"), b"prior-lock\n").unwrap();
        std::fs::write(tmp.path().join("blocked"), b"not-a-directory\n").unwrap();

        let output = tmp.path().join("blocked/crab.yaml");
        let error = run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap_err();
        assert!(matches!(
            error,
            CrabError::Io(_) | CrabError::Configuration { .. }
        ));
        assert_eq!(
            std::fs::read(tmp.path().join("crab.lock")).unwrap(),
            b"prior-lock\n"
        );
        assert!(
            !tmp.path()
                .join(".crab/workflow/migration/pointers/model.pkl")
                .exists()
        );
        let indexed = Command::new("git")
            .args(["ls-files", "--error-unmatch", "model.pkl"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(!indexed.status.success());
    }

    #[test]
    fn migration_pointer_recovery_restores_tree_after_first_rename() {
        let tmp = TempDir::new().unwrap();
        let state = migration_state_root(tmp.path());
        fs::create_dir_all(&state).unwrap();
        let pointer_root = state.join("pointers");
        let temporary = state.join(".pointers.tmp-test");
        let backup = state.join(".pointers.backup-test");
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("old"), b"old").unwrap();
        fs::create_dir_all(&temporary).unwrap();
        fs::write(temporary.join("new"), b"new").unwrap();
        let marker = MigrationPointerPublication {
            schema_version: MIGRATION_POINTER_PUBLICATION_SCHEMA_VERSION,
            pointer_root: "pointers".to_owned(),
            temporary: ".pointers.tmp-test".to_owned(),
            backup: ".pointers.backup-test".to_owned(),
            phase: MigrationPointerPublicationPhase::Ready,
        };
        write_migration_pointer_marker(&migration_pointer_marker_path(tmp.path()), &marker)
            .unwrap();

        // Simulate the process dying after pointers -> backup and before
        // temporary -> pointers.
        assert!(!pointer_root.exists());
        recover_migration_pointer_publication(tmp.path()).unwrap();
        assert_eq!(fs::read(pointer_root.join("old")).unwrap(), b"old");
        assert!(!temporary.exists());
        assert!(!backup.exists());
        assert!(!migration_pointer_marker_path(tmp.path()).exists());
    }

    #[test]
    fn migration_pointer_recovery_keeps_new_tree_after_second_rename() {
        let tmp = TempDir::new().unwrap();
        let state = migration_state_root(tmp.path());
        fs::create_dir_all(&state).unwrap();
        let pointer_root = state.join("pointers");
        let backup = state.join(".pointers.backup-test");
        fs::create_dir_all(&pointer_root).unwrap();
        fs::write(pointer_root.join("new"), b"new").unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("old"), b"old").unwrap();
        let marker = MigrationPointerPublication {
            schema_version: MIGRATION_POINTER_PUBLICATION_SCHEMA_VERSION,
            pointer_root: "pointers".to_owned(),
            temporary: ".pointers.tmp-test".to_owned(),
            backup: ".pointers.backup-test".to_owned(),
            phase: MigrationPointerPublicationPhase::Ready,
        };
        write_migration_pointer_marker(&migration_pointer_marker_path(tmp.path()), &marker)
            .unwrap();

        recover_migration_pointer_publication(tmp.path()).unwrap();
        assert_eq!(fs::read(pointer_root.join("new")).unwrap(), b"new");
        assert!(!backup.exists());
        assert!(!migration_pointer_marker_path(tmp.path()).exists());
    }

    #[test]
    fn remote_mapping_stays_blocked_until_destination_is_verified() {
        let mut inventory = DvcInventory {
            schema_version: crab_workflow::DVC_INVENTORY_SCHEMA_VERSION,
            metadata_files: Vec::new(),
            outputs: Vec::new(),
            remotes: vec![crab_workflow::DvcRemoteDescriptor {
                name: "origin".to_owned(),
                locator: "s3://bucket/path".to_owned(),
                scheme: "s3".to_owned(),
                default: true,
                source_config: ".dvc/config".to_owned(),
                credential_source: "environment".to_owned(),
                capability: "unmapped".to_owned(),
                destination: None,
            }],
            lock_records: Vec::new(),
            cache_object_count: 0,
            cache_objects: Vec::new(),
            cache_roots: Vec::new(),
            run_cache_files: Vec::new(),
            ignore_files: Vec::new(),
            fingerprint: "fingerprint".to_owned(),
            findings: vec![crab_workflow::DvcFinding {
                code: "dvc_remote_unmapped".to_owned(),
                source: Some("origin".to_owned()),
                detail: "mapping required".to_owned(),
                blocking: true,
            }],
            safe_to_remove_dvc: false,
        };

        apply_remote_mappings(&mut inventory, &["origin=crab://repo".to_owned()]).unwrap();

        assert_eq!(
            inventory.remotes[0].capability,
            "mapped;destination-unverified"
        );
        assert!(inventory.findings.iter().any(|finding| {
            finding.code == "dvc_remote_destination_unverified" && finding.blocking
        }));
        assert!(
            !inventory
                .findings
                .iter()
                .any(|finding| { finding.code == "dvc_remote_unmapped" })
        );
    }

    #[test]
    fn migrate_from_dvc_rejects_checkpoint_without_partial_output() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            r"
stages:
  train:
    cmd: python train.py
    outs:
      - model.pkl:
          checkpoint: true
",
        )
        .unwrap();

        let output = tmp.path().join("crab.yaml");
        std::fs::write(&output, "sentinel\n").unwrap();

        let error = run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap_err();

        assert!(matches!(
            error,
            CrabError::Configuration { key, .. } if key == "dvc_checkpoint_unsupported"
        ));
        assert_eq!(std::fs::read_to_string(output).unwrap(), "sentinel\n");
    }

    #[test]
    fn migrate_from_dvc_rejects_unverified_data_without_writing_yaml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("dvc.yaml"),
            r"
stages:
  train:
    cmd: python train.py
    outs:
      - missing.bin
",
        )
        .unwrap();

        let output = tmp.path().join("crab.yaml");
        std::fs::write(&output, "sentinel\n").unwrap();

        let error = run_migrate_from_dvc(Some(tmp.path()), false, Some(&output)).unwrap_err();

        assert!(matches!(
            error,
            CrabError::Configuration { key, .. } if key == "dvc_migration_data_unverified"
        ));
        assert_eq!(std::fs::read_to_string(output).unwrap(), "sentinel\n");
        assert!(
            tmp.path()
                .join(".crab/workflow/migration/dvc.json")
                .is_file()
        );
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }
}
