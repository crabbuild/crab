//! `crab lfs dedup` — deduplicate checked-out LFS files.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::core::cow_clone;
use crate::core::error::{CrabError, Result, check_cancelled};
use crab_git::lfs_pointer::LfsPointer;
use crab_staging::StagingArea;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;

static BACKUP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const GIT_OBJECT_BATCH_SIZE: usize = 500;
const MAX_LFS_SCAN_OBJECTS: usize = 5_000_000;
const MAX_LFS_CHUNKS_PER_FILE: usize = 1_000_000;
const MAX_DELETE_ERROR_REPORTS: usize = 1_024;
const MAX_GIT_RECORD_BYTES: usize = 128 * 1024;
const MAX_GIT_BATCH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct LfsDedupOptions {
    pub dry_run: bool,
    pub test: bool,
    pub crab_cache: bool,
}

/// Run `crab lfs dedup`.
pub fn run_lfs_dedup(options: LfsDedupOptions) -> Result<()> {
    run_lfs_dedup_with_cancel(options, &CancellationToken::new())
}

/// Run LFS deduplication while honoring the optimize command's cancellation.
pub fn run_lfs_dedup_with_cancel(
    options: LfsDedupOptions,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    if options.crab_cache {
        return run_crab_cache_dedup(options.dry_run, cancel);
    }

    if options.test {
        return run_worktree_dedup_test(cancel);
    }

    run_worktree_dedup(options.dry_run, cancel)
}

fn run_worktree_dedup_test(cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    let lfs_dir = resolve_local_lfs_dir(&repo_root, &git_dir)?;
    ensure_no_lfs_extensions()?;
    ensure_cow_clone_supported(&lfs_dir)?;
    check_cancelled(cancel)?;
    println!("OK: This platform and repository support file de-duplication.");
    Ok(())
}

fn run_worktree_dedup(dry_run: bool, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    let lfs_dir = resolve_local_lfs_dir(&repo_root, &git_dir)?;
    ensure_no_lfs_extensions()?;
    ensure_worktree_clean(&repo_root)?;

    let entries = collect_head_lfs_entries_with_cancel(&repo_root, &lfs_dir, cancel)?;
    check_cancelled(cancel)?;
    if entries.is_empty() {
        println!(
            "Finished successfully.\n  De-duplicated  size: 0 bytes\n                count: 0"
        );
        return Ok(());
    }

    for entry in &entries {
        check_cancelled(cancel)?;
        verify_worktree_entry_with_cancel(&repo_root, entry, cancel)?;
    }

    if !dry_run {
        ensure_cow_clone_supported(&lfs_dir)?;
    }

    let mut total_count = 0u64;
    let mut total_size = 0u64;

    for entry in entries {
        check_cancelled(cancel)?;
        let dst = repo_root.join(&entry.path);

        if dry_run {
            println!(
                "Would de-duplicate: {} (Size: {})",
                entry.path, entry.pointer.size
            );
            total_count = total_count.saturating_add(1);
            total_size = total_size.saturating_add(entry.pointer.size);
            continue;
        }

        replace_with_cow_clone(&entry.object_path, &dst)?;
        println!("Success: {} (Size: {})", entry.path, entry.pointer.size);
        total_count = total_count.saturating_add(1);
        total_size = total_size.saturating_add(entry.pointer.size);
    }

    println!(
        "\n\nFinished successfully.\n  De-duplicated  size: {} bytes\n                count: {}",
        total_size, total_count
    );
    Ok(())
}

fn verify_worktree_entry_with_cancel(
    repo_root: &Path,
    entry: &LfsWorktreeEntry,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let object_metadata = std::fs::symlink_metadata(&entry.object_path).map_err(|source| {
        CrabError::Configuration {
            key: entry.path.clone(),
            origin: format!(
                "Git LFS object {} is unavailable: {source}",
                entry.object_path.display()
            ),
        }
    })?;
    if !object_metadata.file_type().is_file() {
        return Err(CrabError::Configuration {
            key: entry.path.clone(),
            origin: format!(
                "Git LFS object {} is not a regular file",
                entry.object_path.display()
            ),
        });
    }
    verify_lfs_file_with_cancel(&entry.object_path, &entry.pointer, &entry.path, cancel)?;

    let worktree_path = repo_root.join(&entry.path);
    let worktree_metadata =
        std::fs::symlink_metadata(&worktree_path).map_err(|source| CrabError::Configuration {
            key: entry.path.clone(),
            origin: format!("checked-out LFS file is unavailable: {source}"),
        })?;
    if !worktree_metadata.file_type().is_file() {
        return Err(CrabError::Configuration {
            key: entry.path.clone(),
            origin: "checked-out LFS path is not a regular file".to_owned(),
        });
    }
    verify_lfs_file_with_cancel(&worktree_path, &entry.pointer, &entry.path, cancel)
}

#[cfg(test)]
pub(super) fn verify_lfs_file(path: &Path, pointer: &LfsPointer, context: &str) -> Result<()> {
    verify_lfs_file_with_cancel(path, pointer, context, &CancellationToken::new())
}

pub(super) fn verify_lfs_file_with_cancel(
    path: &Path,
    pointer: &LfsPointer,
    context: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let mut file = std::fs::File::open(path).map_err(CrabError::Io)?;
    let mut sha = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        check_cancelled(cancel)?;
        let read = file.read(&mut buf).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        sha.update(&buf[..read]);
        size = size.checked_add(read as u64).ok_or_else(|| {
            CrabError::Internal(format!("LFS byte count overflow while reading {context}"))
        })?;
    }
    let actual: [u8; 32] = sha.finalize().into();
    if actual != pointer.oid || size != pointer.size {
        return Err(CrabError::Configuration {
            key: context.to_owned(),
            origin: format!(
                "bytes at {} do not match the indexed LFS SHA-256 and size",
                path.display()
            ),
        });
    }
    Ok(())
}

fn run_crab_cache_dedup(dry_run: bool, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    let lfs_objects_dir = resolve_local_lfs_dir(&repo_root, &git_dir)?.join("objects");

    if !lfs_objects_dir.is_dir() {
        eprintln!("dedup: no local LFS objects found");
        return Ok(());
    }

    let lfs_objects = collect_local_lfs_objects_with_cancel(&lfs_objects_dir, cancel)?;
    if lfs_objects.is_empty() {
        eprintln!("dedup: no local LFS objects found");
        return Ok(());
    }

    let crab_pointers = collect_crab_pointers_with_cancel(&repo_root, cancel)?;
    if crab_pointers.is_empty() {
        eprintln!("dedup: no Crab pointers found; leaving LFS cache unchanged");
        return Ok(());
    }

    let report = super::block_on_runtime(verify_duplicates(
        repo_root.clone(),
        lfs_objects,
        crab_pointers,
        cancel.clone(),
    ))?;

    if report.duplicates.is_empty() {
        eprintln!(
            "dedup: no byte-identical Crab duplicates verified ({} skipped)",
            report.skipped
        );
        return Ok(());
    }

    let total_bytes = report.duplicates.iter().fold(0u64, |total, duplicate| {
        total.saturating_add(duplicate.size)
    });
    if dry_run {
        eprintln!(
            "dedup: would remove {} local LFS object(s), {}",
            report.duplicates.len(),
            format_size(total_bytes)
        );
        for duplicate in &report.duplicates {
            check_cancelled(cancel)?;
            eprintln!("  {}  {}", &duplicate.oid_hex[..10], duplicate.path);
        }
        if report.skipped > 0 {
            eprintln!(
                "dedup: skipped {} object(s) without Crab proof",
                report.skipped
            );
        }
        return Ok(());
    }

    let mut deleted = 0u64;
    let mut deleted_bytes = 0u64;
    let mut delete_errors = Vec::new();
    let mut delete_errors_omitted = 0u64;
    for duplicate in &report.duplicates {
        check_cancelled(cancel)?;
        match std::fs::remove_file(&duplicate.object_path) {
            Ok(()) => {
                deleted = deleted.saturating_add(1);
                deleted_bytes = deleted_bytes.saturating_add(duplicate.size);
            }
            Err(e) => {
                tracing::warn!(
                    oid = %duplicate.oid_hex,
                    error = %e,
                    "failed to remove duplicate LFS object"
                );
                if delete_errors.len() < MAX_DELETE_ERROR_REPORTS {
                    delete_errors.push(format!("{}: {e}", duplicate.object_path.display()));
                } else {
                    delete_errors_omitted = delete_errors_omitted.saturating_add(1);
                }
            }
        }
    }
    cleanup_empty_dirs_with_cancel(&lfs_objects_dir, cancel)?;
    eprintln!(
        "dedup: removed {deleted} local LFS object(s), {}",
        format_size(deleted_bytes)
    );
    if report.skipped > 0 {
        eprintln!(
            "dedup: skipped {} object(s) without Crab proof",
            report.skipped
        );
    }
    if !delete_errors.is_empty() || delete_errors_omitted > 0 {
        return Err(CrabError::Internal(format!(
            "failed to remove {} verified duplicate LFS object(s): {}{}",
            (delete_errors.len() as u64).saturating_add(delete_errors_omitted),
            delete_errors.join("; "),
            if delete_errors_omitted > 0 {
                format!(" ({} additional failures omitted)", delete_errors_omitted)
            } else {
                String::new()
            }
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct LfsWorktreeEntry {
    path: String,
    pointer: LfsPointer,
    object_path: PathBuf,
}

#[derive(Debug)]
struct LocalLfsObject {
    oid_hex: String,
    object_path: PathBuf,
    size: u64,
}

#[derive(Debug)]
struct CrabPointerRef {
    path: String,
    pointer: Pointer,
}

#[derive(Debug)]
struct Duplicate {
    oid_hex: String,
    object_path: PathBuf,
    path: String,
    size: u64,
}

#[derive(Debug, Default)]
struct DedupReport {
    duplicates: Vec<Duplicate>,
    skipped: u64,
}

fn ensure_no_lfs_extensions() -> Result<()> {
    if crate::lfs::extension::configured_extensions()?.is_empty() {
        return Ok(());
    }

    Err(CrabError::Configuration {
        key: "lfs.extension".to_owned(),
        origin: "Git LFS extensions are configured and therefore de-duplication cannot be used"
            .to_owned(),
    })
}

fn ensure_worktree_clean(repo_root: &Path) -> Result<()> {
    let output = git_command(repo_root)
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git status: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.is_empty() {
        return Ok(());
    }

    Err(CrabError::Configuration {
        key: "working tree is dirty".to_owned(),
        origin: "commit or reset changes before running `crab lfs dedup`".to_owned(),
    })
}

fn collect_head_lfs_entries_with_cancel(
    repo_root: &Path,
    lfs_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<LfsWorktreeEntry>> {
    check_cancelled(cancel)?;
    let blob_refs = head_blob_refs(repo_root)?;
    let mut entries = Vec::new();

    for (blob, path) in batch_read_blobs_with_cancel(repo_root, &blob_refs, cancel)? {
        check_cancelled(cancel)?;
        let Ok(pointer) = LfsPointer::parse(&blob) else {
            continue;
        };
        if pointer.size == 0 {
            continue;
        }
        let oid_hex = crab_git::lfs_pointer::hex_encode(&pointer.oid);
        entries.push(LfsWorktreeEntry {
            path,
            pointer,
            object_path: local_lfs_object_path(lfs_dir, &oid_hex),
        });
    }

    Ok(entries)
}

fn head_blob_refs(repo_root: &Path) -> Result<Vec<(String, String)>> {
    let mut child = git_command(repo_root)
        .args(["ls-tree", "-r", "-z", "-l", "HEAD"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git ls-tree stdout missing".to_owned()))?;
    let scan = parse_head_blob_refs(std::io::BufReader::new(stdout));
    if scan.is_err() {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git ls-tree failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") || stderr.contains("unknown revision") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!("git ls-tree failed: {stderr}")));
    }

    scan
}

fn parse_head_blob_refs(reader: impl BufRead) -> Result<Vec<(String, String)>> {
    let mut refs = Vec::new();
    for record in reader.split(0) {
        let record = record.map_err(CrabError::Io)?;
        if record.is_empty() {
            continue;
        }
        if record.len() > MAX_GIT_RECORD_BYTES {
            return Err(CrabError::Configuration {
                key: "lfs Git tree record size".to_owned(),
                origin: format!(
                    "Git tree record exceeds the safety limit of {MAX_GIT_RECORD_BYTES} bytes"
                ),
            });
        }
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let meta = std::str::from_utf8(&record[..tab]).map_err(|error| {
            CrabError::Internal(format!("git ls-tree returned invalid metadata: {error}"))
        })?;
        let path = String::from_utf8(record[tab + 1..].to_vec()).map_err(|error| {
            CrabError::Configuration {
                key: "LFS path".to_owned(),
                origin: format!("Git path is not valid UTF-8: {error}"),
            }
        })?;
        let parts: Vec<&str> = meta.split_whitespace().collect();
        let is_regular_file = parts
            .first()
            .is_some_and(|mode| matches!(*mode, "100644" | "100755"));
        let is_small_blob = parts
            .get(3)
            .and_then(|size| size.parse::<usize>().ok())
            .is_some_and(|size| size <= crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE);
        if parts.len() >= 4 && is_regular_file && parts[1] == "blob" && is_small_blob {
            if refs.len() >= MAX_LFS_SCAN_OBJECTS {
                return Err(CrabError::Configuration {
                    key: "lfs dedup object count".to_owned(),
                    origin: format!(
                        "HEAD contains more than {MAX_LFS_SCAN_OBJECTS} candidate blobs; refusing an unbounded dedup scan"
                    ),
                });
            }
            refs.push((parts[2].to_owned(), path));
        }
    }

    Ok(refs)
}

fn local_lfs_object_path(lfs_dir: &Path, oid_hex: &str) -> PathBuf {
    lfs_dir
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

fn ensure_cow_clone_supported(lfs_dir: &Path) -> Result<()> {
    let tmp = lfs_dir.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(CrabError::Io)?;
    let stem = format!(
        "dedup-test-{}-{}",
        std::process::id(),
        BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let src = tmp.join(format!("{stem}.src"));
    let dst = tmp.join(format!("{stem}.dst"));
    std::fs::write(&src, b"crab-lfs-dedup").map_err(CrabError::Io)?;
    let result = cow_clone::clone_file(&src, &dst);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    result.map_err(|source| CrabError::Configuration {
        key: "dedup unsupported".to_owned(),
        origin: format!("this system does not support copy-on-write file cloning: {source}"),
    })
}

fn resolve_local_lfs_dir(repo_root: &Path, common_git_dir: &Path) -> Result<PathBuf> {
    let config = crate::lfs::config::LfsConfig::resolve(repo_root)?;
    Ok(config.storage_dir(common_git_dir))
}

fn replace_with_cow_clone(src: &Path, dst: &Path) -> Result<()> {
    let original = std::fs::metadata(dst).map_err(CrabError::Io)?;
    let backup = backup_path(dst);
    std::fs::rename(dst, &backup).map_err(CrabError::Io)?;

    let clone_result = cow_clone::clone_file(src, dst)
        .and_then(|()| std::fs::set_permissions(dst, original.permissions()));
    match clone_result {
        Ok(()) => std::fs::remove_file(&backup).map_err(CrabError::Io),
        Err(source) => {
            if let Err(cleanup_error) = std::fs::remove_file(dst)
                && cleanup_error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(CrabError::Internal(format!(
                    "dedup clone failed for {}; removing the incomplete clone also failed: {cleanup_error}; original remains at {}",
                    dst.display(),
                    backup.display()
                )));
            }
            std::fs::rename(&backup, dst).map_err(|restore_error| {
                CrabError::Internal(format!(
                    "dedup clone failed for {}: {source}; restoring the original from {} also failed: {restore_error}",
                    dst.display(),
                    backup.display()
                ))
            })?;
            Err(CrabError::Configuration {
                key: "dedup clone failed".to_owned(),
                origin: source.to_string(),
            })
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    loop {
        let candidate = path.with_file_name(format!(
            ".{file_name}.crab-lfs-dedup-backup.{}.{}",
            std::process::id(),
            BACKUP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
}

async fn verify_duplicates(
    repo_root: PathBuf,
    lfs_objects: Vec<LocalLfsObject>,
    crab_pointers: Vec<CrabPointerRef>,
    cancel: CancellationToken,
) -> Result<DedupReport> {
    check_cancelled(&cancel)?;
    let staging_root = repo_root.join(".crab").join("staging");
    if !staging_root.is_dir() {
        return Ok(DedupReport {
            duplicates: Vec::new(),
            skipped: lfs_objects.len() as u64,
        });
    }

    let staging = StagingArea::open(staging_root).await?;
    let result =
        verify_duplicates_with_staging(&staging, lfs_objects, crab_pointers, &cancel).await;
    let close_result = staging.close().await;
    match (result, close_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(close_error)) => Err(close_error.into()),
        (Err(primary), Err(close_error)) => {
            tracing::warn!(error = %close_error, "failed to close LFS dedup staging after an earlier error");
            Err(primary)
        }
    }
}

async fn verify_duplicates_with_staging(
    staging: &StagingArea,
    lfs_objects: Vec<LocalLfsObject>,
    crab_pointers: Vec<CrabPointerRef>,
    cancel: &CancellationToken,
) -> Result<DedupReport> {
    let mut by_hash: HashMap<([u8; 32], u64), CrabPointerRef> = HashMap::new();
    for pointer_ref in crab_pointers {
        by_hash
            .entry((pointer_ref.pointer.file_hash, pointer_ref.pointer.size))
            .or_insert(pointer_ref);
    }

    let mut report = DedupReport::default();
    let mut seen_oids = HashSet::new();

    for object in &lfs_objects {
        check_cancelled(cancel)?;
        let digest = match hash_lfs_object_with_cancel(object, cancel) {
            Ok(digest) => digest,
            Err(CrabError::Cancelled) => return Err(CrabError::Cancelled),
            Err(e) => {
                tracing::warn!(oid = %object.oid_hex, error = %e, "skipping unreadable LFS object");
                report.skipped = report.skipped.saturating_add(1);
                continue;
            }
        };

        if digest.sha256_hex != object.oid_hex || digest.size != object.size {
            tracing::warn!(
                oid = %object.oid_hex,
                actual = %digest.sha256_hex,
                "skipping corrupt local LFS object"
            );
            report.skipped = report.skipped.saturating_add(1);
            continue;
        }

        let Some(pointer_ref) = by_hash.get(&(digest.blake3, digest.size)) else {
            report.skipped = report.skipped.saturating_add(1);
            continue;
        };

        let matched = staging_matches_lfs_object_with_cancel(
            staging,
            &pointer_ref.pointer,
            &object.object_path,
            cancel,
        )
        .await?
        .then(|| pointer_ref.path.clone());

        if let Some(path) = matched {
            if seen_oids.insert(object.oid_hex.clone()) {
                report.duplicates.push(Duplicate {
                    oid_hex: object.oid_hex.clone(),
                    object_path: object.object_path.clone(),
                    path,
                    size: object.size,
                });
            }
        } else {
            report.skipped = report.skipped.saturating_add(1);
        }
    }

    Ok(report)
}

#[derive(Debug)]
struct LfsDigest {
    sha256_hex: String,
    blake3: [u8; 32],
    size: u64,
}

#[cfg(test)]
fn hash_lfs_object(object: &LocalLfsObject) -> Result<LfsDigest> {
    hash_lfs_object_with_cancel(object, &CancellationToken::new())
}

fn hash_lfs_object_with_cancel(
    object: &LocalLfsObject,
    cancel: &CancellationToken,
) -> Result<LfsDigest> {
    check_cancelled(cancel)?;
    let mut file = std::fs::File::open(&object.object_path).map_err(CrabError::Io)?;
    let mut sha = Sha256::new();
    let mut b3 = blake3::Hasher::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        check_cancelled(cancel)?;
        let n = file.read(&mut buf).map_err(CrabError::Io)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        b3.update(&buf[..n]);
        size = size.checked_add(n as u64).ok_or_else(|| {
            CrabError::Internal(format!(
                "LFS byte count overflow while reading {}",
                object.object_path.display()
            ))
        })?;
    }
    let sha256: [u8; 32] = sha.finalize().into();
    Ok(LfsDigest {
        sha256_hex: crab_git::lfs_pointer::hex_encode(&sha256),
        blake3: *b3.finalize().as_bytes(),
        size,
    })
}

async fn staging_matches_lfs_object_with_cancel(
    staging: &StagingArea,
    pointer: &Pointer,
    object_path: &Path,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    let file_hash = MerkleHash::from(pointer.file_hash);
    let chunk_hashes = staging.chunks_for_file(&file_hash)?;
    if chunk_hashes.len() > MAX_LFS_CHUNKS_PER_FILE {
        return Err(CrabError::Configuration {
            key: "lfs dedup chunks per file".to_owned(),
            origin: format!(
                "staging file {} contains {} chunks; bounded dedup supports at most {MAX_LFS_CHUNKS_PER_FILE}",
                file_hash,
                chunk_hashes.len()
            ),
        });
    }
    if chunk_hashes.is_empty() {
        return Ok(false);
    }

    let mut file = std::fs::File::open(object_path).map_err(CrabError::Io)?;
    let mut total = 0u64;

    for chunk_hash in &chunk_hashes {
        check_cancelled(cancel)?;
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CrabError::Cancelled),
            result = staging.get_chunk(chunk_hash) => result?,
        };
        let Some(chunk) = chunk else {
            return Ok(false);
        };
        let mut buf = vec![0u8; chunk.len()];
        file.read_exact(&mut buf).map_err(CrabError::Io)?;
        if buf != chunk.as_ref() {
            return Ok(false);
        }
        total = total.saturating_add(chunk.len() as u64);
    }

    let mut extra = [0u8; 1];
    let eof = file.read(&mut extra).map_err(CrabError::Io)? == 0;
    Ok(eof && total == pointer.size)
}

fn collect_local_lfs_objects_with_cancel(
    lfs_objects_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<LocalLfsObject>> {
    check_cancelled(cancel)?;
    let mut objects = Vec::new();

    for aa_entry in std::fs::read_dir(lfs_objects_dir)
        .map_err(|e| CrabError::Internal(format!("failed to read LFS objects dir: {e}")))?
    {
        check_cancelled(cancel)?;
        let aa_entry = aa_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
        let aa_path = aa_entry.path();
        let aa_name = aa_entry.file_name().to_string_lossy().to_string();
        if !aa_entry.file_type().map_err(CrabError::Io)?.is_dir() || !is_lower_hex_fanout(&aa_name)
        {
            continue;
        }

        for bb_entry in
            std::fs::read_dir(&aa_path).map_err(|e| CrabError::Internal(format!("{e}")))?
        {
            check_cancelled(cancel)?;
            let bb_entry = bb_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
            let bb_path = bb_entry.path();
            let bb_name = bb_entry.file_name().to_string_lossy().to_string();
            if !bb_entry.file_type().map_err(CrabError::Io)?.is_dir()
                || !is_lower_hex_fanout(&bb_name)
            {
                continue;
            }

            for obj_entry in
                std::fs::read_dir(&bb_path).map_err(|e| CrabError::Internal(format!("{e}")))?
            {
                check_cancelled(cancel)?;
                let obj_entry =
                    obj_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
                let name = obj_entry.file_name().to_string_lossy().to_string();
                if name.len() != 64
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    || name[..2] != aa_name
                    || name[2..4] != bb_name
                    || !obj_entry.file_type().map_err(CrabError::Io)?.is_file()
                {
                    continue;
                }
                let object_path = obj_entry.path();
                let meta = std::fs::symlink_metadata(&object_path).map_err(CrabError::Io)?;
                if !meta.file_type().is_file() {
                    continue;
                }
                if objects.len() >= MAX_LFS_SCAN_OBJECTS {
                    return Err(CrabError::Configuration {
                        key: "lfs dedup object count".to_owned(),
                        origin: format!(
                            "local LFS object inventory exceeds the safety limit of {MAX_LFS_SCAN_OBJECTS}"
                        ),
                    });
                }
                objects.push(LocalLfsObject {
                    oid_hex: name,
                    object_path,
                    size: meta.len(),
                });
            }
        }
    }

    Ok(objects)
}

fn is_lower_hex_fanout(value: &str) -> bool {
    value.len() == 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
fn collect_crab_pointers(repo_root: &Path) -> Result<Vec<CrabPointerRef>> {
    collect_crab_pointers_with_cancel(repo_root, &CancellationToken::new())
}

fn collect_crab_pointers_with_cancel(
    repo_root: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<CrabPointerRef>> {
    check_cancelled(cancel)?;
    let mut pointers = Vec::new();
    let mut seen = HashSet::new();
    visit_index_blob_ref_batches(repo_root, |blob_refs| {
        check_cancelled(cancel)?;
        collect_crab_pointers_from_refs_with_cancel(
            repo_root,
            blob_refs,
            &mut pointers,
            &mut seen,
            cancel,
        )
    })?;
    visit_reachable_blob_ref_batches(repo_root, |blob_refs| {
        check_cancelled(cancel)?;
        collect_crab_pointers_from_refs_with_cancel(
            repo_root,
            blob_refs,
            &mut pointers,
            &mut seen,
            cancel,
        )
    })?;
    Ok(pointers)
}

fn collect_crab_pointers_from_refs_with_cancel(
    repo_root: &Path,
    blob_refs: &[(String, String)],
    pointers: &mut Vec<CrabPointerRef>,
    seen: &mut HashSet<([u8; 32], u64)>,
    cancel: &CancellationToken,
) -> Result<()> {
    for (blob, path) in batch_read_blobs_with_cancel(repo_root, blob_refs, cancel)? {
        check_cancelled(cancel)?;
        if let Ok(pointer) = Pointer::parse(&blob)
            && pointer.size > 0
            && seen.insert((pointer.file_hash, pointer.size))
        {
            if pointers.len() >= MAX_LFS_SCAN_OBJECTS {
                return Err(CrabError::Configuration {
                    key: "lfs dedup pointer count".to_owned(),
                    origin: format!(
                        "Crab pointer inventory exceeds the safety limit of {MAX_LFS_SCAN_OBJECTS}"
                    ),
                });
            }
            pointers.push(CrabPointerRef { path, pointer });
        }
    }
    Ok(())
}

fn visit_index_blob_ref_batches(
    repo_root: &Path,
    mut visit: impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<()> {
    let mut child = git_command(repo_root)
        .args(["ls-files", "-s", "-z"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-files: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git ls-files stdout missing".to_owned()))?;
    let scan = visit_index_blob_refs(std::io::BufReader::new(stdout), &mut visit);
    if scan.is_err() {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git ls-files failed: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    scan
}

fn visit_index_blob_refs(
    reader: impl BufRead,
    visit: &mut impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<()> {
    let mut refs = Vec::with_capacity(GIT_OBJECT_BATCH_SIZE);
    for record in reader.split(0) {
        let record = record.map_err(CrabError::Io)?;
        if record.is_empty() {
            continue;
        }
        if record.len() > MAX_GIT_RECORD_BYTES {
            return Err(CrabError::Configuration {
                key: "lfs Git index record size".to_owned(),
                origin: format!(
                    "Git index record exceeds the safety limit of {MAX_GIT_RECORD_BYTES} bytes"
                ),
            });
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(CrabError::Internal(
                "git ls-files returned a malformed index record".to_owned(),
            ));
        };
        let meta = std::str::from_utf8(&record[..tab]).map_err(|error| {
            CrabError::Internal(format!("git ls-files returned invalid metadata: {error}"))
        })?;
        let path = String::from_utf8(record[tab + 1..].to_vec()).map_err(|error| {
            CrabError::Configuration {
                key: "LFS path".to_owned(),
                origin: format!("Git path is not valid UTF-8: {error}"),
            }
        })?;
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(CrabError::Internal(format!(
                "git ls-files returned malformed metadata {meta:?}"
            )));
        }
        if parts[2] != "0" {
            return Err(CrabError::Configuration {
                key: path,
                origin: "cannot deduplicate the LFS cache while the index has unresolved entries"
                    .to_owned(),
            });
        }
        if matches!(parts[0], "100644" | "100755") {
            refs.push((parts[1].to_owned(), path.clone()));
            if refs.len() == GIT_OBJECT_BATCH_SIZE {
                visit(&refs)?;
                refs.clear();
            }
        }
    }
    if !refs.is_empty() {
        visit(&refs)?;
    }
    Ok(())
}

fn visit_reachable_blob_ref_batches(
    repo_root: &Path,
    mut visit: impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<()> {
    let mut child = git_command(repo_root)
        .args(["rev-list", "--all", "--objects", "--no-object-names"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git rev-list stdout missing".to_owned()))?;
    let scan = visit_reachable_blob_refs(std::io::BufReader::new(stdout), &mut visit);
    if scan.is_err() {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git rev-list failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("bad default revision") || stderr.contains("does not have any commits") {
            return Ok(());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }
    scan
}

fn visit_reachable_blob_refs(
    reader: impl BufRead,
    visit: &mut impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<()> {
    let mut refs = Vec::with_capacity(GIT_OBJECT_BATCH_SIZE);
    for line in reader.split(b'\n') {
        let line = line.map_err(CrabError::Io)?;
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_GIT_RECORD_BYTES {
            return Err(CrabError::Configuration {
                key: "lfs Git object record size".to_owned(),
                origin: format!(
                    "Git object record exceeds the safety limit of {MAX_GIT_RECORD_BYTES} bytes"
                ),
            });
        }
        let hash = std::str::from_utf8(&line).map_err(|error| {
            CrabError::Internal(format!(
                "git rev-list returned an invalid object ID: {error}"
            ))
        })?;
        if !is_git_object_id(hash) {
            return Err(CrabError::Internal(format!(
                "git rev-list returned malformed object ID {hash:?}"
            )));
        }
        refs.push((hash.to_owned(), String::from("<reachable object>")));
        if refs.len() == GIT_OBJECT_BATCH_SIZE {
            visit(&refs)?;
            refs.clear();
        }
    }
    if !refs.is_empty() {
        visit(&refs)?;
    }
    Ok(())
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
pub(super) fn batch_read_blobs(
    repo_root: &Path,
    blob_refs: &[(String, String)],
) -> Result<Vec<(Vec<u8>, String)>> {
    batch_read_blobs_with_cancel(repo_root, blob_refs, &CancellationToken::new())
}

pub(super) fn batch_read_blobs_with_cancel(
    repo_root: &Path,
    blob_refs: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<Vec<(Vec<u8>, String)>> {
    check_cancelled(cancel)?;
    if blob_refs.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for chunk in blob_refs.chunks(GIT_OBJECT_BATCH_SIZE) {
        check_cancelled(cancel)?;
        let small_refs = small_blob_refs(repo_root, chunk)?;
        if small_refs.is_empty() {
            continue;
        }
        let input = small_refs
            .iter()
            .map(|(hash, _)| hash.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut child = git_command(repo_root)
            .args(["cat-file", "--batch"])
            .current_dir(repo_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CrabError::Internal(format!("failed to spawn git cat-file: {e}")))?;
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin.write_all(input.as_bytes()).map_err(CrabError::Io)?;
        }
        drop(child.stdin.take());
        let output = child
            .wait_with_output()
            .map_err(|e| CrabError::Internal(format!("git cat-file failed: {e}")))?;
        if !output.status.success() {
            return Err(CrabError::Internal(format!(
                "git cat-file --batch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        if output.stdout.len() > MAX_GIT_BATCH_OUTPUT_BYTES {
            return Err(CrabError::Configuration {
                key: "lfs Git blob response size".to_owned(),
                origin: format!(
                    "Git blob response exceeds the safety limit of {MAX_GIT_BATCH_OUTPUT_BYTES} bytes"
                ),
            });
        }
        parse_batch_output(&output.stdout, &small_refs, &mut out)?;
    }
    Ok(out)
}

fn small_blob_refs(repo_root: &Path, refs: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let input = refs
        .iter()
        .map(|(hash, _)| hash.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut child = git_command(repo_root)
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git cat-file: {e}")))?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(input.as_bytes()).map_err(CrabError::Io)?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git cat-file --batch-check failed: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git cat-file --batch-check failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_BATCH_OUTPUT_BYTES {
        return Err(CrabError::Configuration {
            key: "lfs Git blob metadata response size".to_owned(),
            origin: format!(
                "Git blob metadata response exceeds the safety limit of {MAX_GIT_BATCH_OUTPUT_BYTES} bytes"
            ),
        });
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
        CrabError::Internal(format!(
            "git cat-file --batch-check returned invalid UTF-8: {error}"
        ))
    })?;
    let lines = stdout.lines().collect::<Vec<_>>();
    if lines.len() != refs.len() {
        return Err(CrabError::Internal(format!(
            "git cat-file --batch-check returned {} responses for {} objects",
            lines.len(),
            refs.len()
        )));
    }

    let mut small = Vec::new();
    for (line, reference) in lines.into_iter().zip(refs) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(CrabError::Internal(format!(
                "git cat-file --batch-check returned malformed response {line:?}"
            )));
        }
        let size = fields[2].parse::<usize>().map_err(|error| {
            CrabError::Internal(format!(
                "git cat-file returned invalid object size: {error}"
            ))
        })?;
        if fields[1] == "blob" && size <= crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE {
            small.push(reference.clone());
        }
    }
    Ok(small)
}

fn parse_batch_output(
    stdout: &[u8],
    refs: &[(String, String)],
    out: &mut Vec<(Vec<u8>, String)>,
) -> Result<()> {
    if stdout.len() > MAX_GIT_BATCH_OUTPUT_BYTES {
        return Err(CrabError::Configuration {
            key: "lfs Git blob response size".to_owned(),
            origin: format!(
                "Git blob response exceeds the safety limit of {MAX_GIT_BATCH_OUTPUT_BYTES} bytes"
            ),
        });
    }
    let mut pos = 0;
    let mut index = 0;
    while pos < stdout.len() && index < refs.len() {
        let Some(header_rel_end) = stdout[pos..].iter().position(|&b| b == b'\n') else {
            return Err(CrabError::Internal(
                "git cat-file batch response ended inside a header".to_owned(),
            ));
        };
        let header_end = pos + header_rel_end;
        let header = std::str::from_utf8(&stdout[pos..header_end]).map_err(|error| {
            CrabError::Internal(format!("git cat-file returned invalid header: {error}"))
        })?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 3 || parts[1] != "blob" {
            return Err(CrabError::Internal(format!(
                "git cat-file returned unexpected response {header:?}"
            )));
        }
        let size = parts[2].parse::<usize>().map_err(|error| {
            CrabError::Internal(format!(
                "git cat-file returned invalid object size: {error}"
            ))
        })?;
        let content_start = header_end + 1;
        let content_end = content_start
            .checked_add(size)
            .ok_or_else(|| CrabError::Internal("git cat-file object size overflow".to_owned()))?;
        if content_end >= stdout.len() || stdout[content_end] != b'\n' {
            return Err(CrabError::Internal(
                "git cat-file returned a truncated object response".to_owned(),
            ));
        }
        out.push((
            stdout[content_start..content_end].to_vec(),
            refs[index].1.clone(),
        ));
        pos = content_end + 1;
        index += 1;
    }
    if index != refs.len() || pos != stdout.len() {
        return Err(CrabError::Internal(format!(
            "git cat-file returned {index} complete responses for {} objects",
            refs.len()
        )));
    }
    Ok(())
}

fn discover_repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    Ok(crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?.current_worktree_root)
}

fn git_command(repo_root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_QUARANTINE_PATH")
        .env_remove("GIT_NAMESPACE");
    command
}

fn discover_git_dir(repo_root: &Path) -> Result<PathBuf> {
    crate::git::discover::discover_common_git_dir_from(repo_root)
}

fn cleanup_empty_dirs_with_cancel(
    lfs_objects_dir: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    if let Ok(top_entries) = std::fs::read_dir(lfs_objects_dir) {
        for aa_entry in top_entries.flatten() {
            check_cancelled(cancel)?;
            let aa_path = aa_entry.path();
            if !aa_path.is_dir() {
                continue;
            }
            if let Ok(bb_entries) = std::fs::read_dir(&aa_path) {
                for bb_entry in bb_entries.flatten() {
                    check_cancelled(cancel)?;
                    let bb_path = bb_entry.path();
                    if bb_path.is_dir() {
                        let _ = std::fs::remove_dir(&bb_path);
                    }
                }
            }
            let _ = std::fs::remove_dir(&aa_path);
        }
    }
    Ok(())
}

fn format_size(bytes: u64) -> String {
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
    use crate::test::git_repo::GIT_DIR_MUTEX;

    fn temp_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn dedup_without_local_objects_is_noop() {
        let _guard = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_git_repo();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = run_lfs_dedup(LfsDedupOptions {
            crab_cache: true,
            ..LfsDedupOptions::default()
        });
        std::env::set_current_dir(cwd).unwrap();

        assert!(result.is_ok());
    }

    #[test]
    fn hash_lfs_object_reports_expected_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"dedup me";
        let sha: [u8; 32] = Sha256::digest(data).into();
        let oid = crab_git::lfs_pointer::hex_encode(&sha);
        let path = dir.path().join(&oid);
        std::fs::write(&path, data).unwrap();
        let digest = hash_lfs_object(&LocalLfsObject {
            oid_hex: oid.clone(),
            object_path: path,
            size: data.len() as u64,
        })
        .unwrap();
        assert_eq!(digest.sha256_hex, oid);
        assert_eq!(digest.blake3, *blake3::hash(data).as_bytes());
        assert_eq!(digest.size, data.len() as u64);
    }

    #[test]
    fn verify_lfs_file_rejects_corrupt_cache_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let expected = b"expected bytes";
        let actual = b"corrupt bytes";
        let oid: [u8; 32] = Sha256::digest(expected).into();
        let path = dir.path().join("object");
        std::fs::write(&path, actual).unwrap();

        let error = verify_lfs_file(
            &path,
            &LfsPointer {
                oid,
                size: expected.len() as u64,
                extensions: Vec::new(),
            },
            "model.bin",
        )
        .unwrap_err();

        assert!(error.to_string().contains("indexed LFS SHA-256 and size"));
    }

    #[test]
    fn batch_read_blobs_filters_large_git_objects() {
        let _guard = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_git_repo();
        let large = vec![b'x'; crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE + 1];
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child.stdin.as_mut().unwrap().write_all(&large).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let oid = String::from_utf8(output.stdout).unwrap().trim().to_owned();

        let blobs = batch_read_blobs(
            dir.path(),
            &[(oid, String::from("large-not-a-pointer.bin"))],
        )
        .unwrap();

        assert!(blobs.is_empty());
    }

    #[test]
    fn worktree_dedup_replaces_from_verified_lfs_object() {
        let _guard = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_git_repo();
        for args in [
            ["config", "user.email", "test@example.com"],
            ["config", "user.name", "Test"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let content = b"verified LFS worktree content";
        let oid: [u8; 32] = Sha256::digest(content).into();
        let pointer = LfsPointer {
            oid,
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        let worktree_path = dir.path().join("model.bin");
        std::fs::write(&worktree_path, pointer.serialize()).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "model.bin"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "add LFS pointer"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let git_dir = dir.path().join(".git");
        let object_path = local_lfs_object_path(
            &git_dir.join("lfs"),
            &crab_git::lfs_pointer::hex_encode(&oid),
        );
        std::fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        std::fs::write(&object_path, content).unwrap();
        std::fs::write(&worktree_path, content).unwrap();
        assert!(
            Command::new("git")
                .args(["update-index", "--assume-unchanged", "model.bin"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        if ensure_cow_clone_supported(&git_dir).is_err() {
            return;
        }

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = run_worktree_dedup(false, &CancellationToken::new());
        std::env::set_current_dir(original_cwd).unwrap();

        result.unwrap();
        assert_eq!(std::fs::read(worktree_path).unwrap(), content);
    }

    #[test]
    fn collect_crab_pointers_includes_index() {
        let _guard = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_git_repo();
        let data = b"indexed crab pointer";
        let pointer = Pointer {
            file_hash: *blake3::hash(data).as_bytes(),
            size: data.len() as u64,
            shard_hint: None,
        };
        std::fs::write(dir.path().join("model.bin"), pointer.serialize()).unwrap();
        Command::new("git")
            .args(["add", "model.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let pointers = collect_crab_pointers(dir.path()).unwrap();
        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].path, "model.bin");
    }

    #[test]
    fn head_blob_refs_parses_nul_separated_ls_tree_records() {
        let _guard = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_git_repo();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("model.bin"), b"content").unwrap();
        Command::new("git")
            .args(["add", "model.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let refs = head_blob_refs(dir.path()).unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "model.bin");
    }

    #[test]
    fn backup_path_stays_next_to_target() {
        let backup = backup_path(Path::new("dir/model.bin"));
        assert_eq!(backup.parent(), Some(Path::new("dir")));
        assert!(
            backup
                .to_string_lossy()
                .contains(".model.bin.crab-lfs-dedup-backup.")
        );
    }

    #[test]
    fn optimize_dedup_honors_cancellation_before_repository_discovery() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = run_lfs_dedup_with_cancel(LfsDedupOptions::default(), &cancel).unwrap_err();
        assert!(matches!(error, CrabError::Cancelled));
    }
}
