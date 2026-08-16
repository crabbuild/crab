//! `crab lfs dedup` — deduplicate checked-out LFS files.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::core::cow_clone;
use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::LfsPointer;
use crab_staging::StagingArea;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;

#[derive(Debug, Clone, Copy, Default)]
pub struct LfsDedupOptions {
    pub dry_run: bool,
    pub test: bool,
    pub crab_cache: bool,
}

/// Run `crab lfs dedup`.
pub fn run_lfs_dedup(options: LfsDedupOptions) -> Result<()> {
    if options.crab_cache {
        return run_crab_cache_dedup(options.dry_run);
    }

    if options.test {
        return run_worktree_dedup_test();
    }

    run_worktree_dedup(options.dry_run)
}

fn run_worktree_dedup_test() -> Result<()> {
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    ensure_no_lfs_extensions()?;
    ensure_cow_clone_supported(&git_dir)?;
    println!("OK: This platform and repository support file de-duplication.");
    Ok(())
}

fn run_worktree_dedup(dry_run: bool) -> Result<()> {
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    ensure_no_lfs_extensions()?;
    ensure_worktree_clean(&repo_root)?;
    ensure_cow_clone_supported(&git_dir)?;

    let entries = collect_head_lfs_entries(&repo_root, &git_dir)?;
    if entries.is_empty() {
        println!(
            "Finished successfully.\n  De-duplicated  size: 0 bytes\n                count: 0"
        );
        return Ok(());
    }

    let mut total_count = 0u64;
    let mut total_size = 0u64;

    for entry in entries {
        if !entry.object_path.is_file() {
            eprintln!(
                "Skipped: {} (Size: {})\n          Git LFS object file does not exist",
                entry.path, entry.pointer.size
            );
            continue;
        }

        let dst = repo_root.join(&entry.path);
        if !dst.is_file() {
            eprintln!("Skipped: {} (Size: {})", entry.path, entry.pointer.size);
            continue;
        }

        if dry_run {
            println!(
                "Would de-duplicate: {} (Size: {})",
                entry.path, entry.pointer.size
            );
            total_count += 1;
            total_size += entry.pointer.size;
            continue;
        }

        match replace_with_cow_clone(&entry.object_path, &dst) {
            Ok(()) => {
                println!("Success: {} (Size: {})", entry.path, entry.pointer.size);
                total_count += 1;
                total_size += entry.pointer.size;
            }
            Err(e) => {
                eprintln!(
                    "Skipped: {} (Size: {})\n          {}",
                    entry.path, entry.pointer.size, e
                );
            }
        }
    }

    println!(
        "\n\nFinished successfully.\n  De-duplicated  size: {} bytes\n                count: {}",
        total_size, total_count
    );
    Ok(())
}

fn run_crab_cache_dedup(dry_run: bool) -> Result<()> {
    let repo_root = discover_repo_root()?;
    let git_dir = discover_git_dir(&repo_root)?;
    let lfs_objects_dir = git_dir.join("lfs").join("objects");

    if !lfs_objects_dir.is_dir() {
        eprintln!("dedup: no local LFS objects found");
        return Ok(());
    }

    let lfs_objects = collect_local_lfs_objects(&lfs_objects_dir)?;
    if lfs_objects.is_empty() {
        eprintln!("dedup: no local LFS objects found");
        return Ok(());
    }

    let crab_pointers = collect_crab_pointers(&repo_root)?;
    if crab_pointers.is_empty() {
        eprintln!("dedup: no Crab pointers found; leaving LFS cache unchanged");
        return Ok(());
    }

    let report = super::block_on_runtime(verify_duplicates(
        repo_root.clone(),
        lfs_objects,
        crab_pointers,
    ))?;

    if report.duplicates.is_empty() {
        eprintln!(
            "dedup: no byte-identical Crab duplicates verified ({} skipped)",
            report.skipped
        );
        return Ok(());
    }

    let total_bytes: u64 = report.duplicates.iter().map(|d| d.size).sum();
    if dry_run {
        eprintln!(
            "dedup: would remove {} local LFS object(s), {}",
            report.duplicates.len(),
            format_size(total_bytes)
        );
        for duplicate in &report.duplicates {
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
    for duplicate in &report.duplicates {
        match std::fs::remove_file(&duplicate.object_path) {
            Ok(()) => {
                deleted += 1;
                deleted_bytes += duplicate.size;
            }
            Err(e) => {
                tracing::warn!(
                    oid = %duplicate.oid_hex,
                    error = %e,
                    "failed to remove duplicate LFS object"
                );
            }
        }
    }
    cleanup_empty_dirs(&lfs_objects_dir);
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
    let output = Command::new("git")
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

fn collect_head_lfs_entries(repo_root: &Path, git_dir: &Path) -> Result<Vec<LfsWorktreeEntry>> {
    let blob_refs = head_blob_refs(repo_root)?;
    let mut entries = Vec::new();

    for (blob, path) in batch_read_blobs(repo_root, &blob_refs)? {
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
            object_path: local_lfs_object_path(git_dir, &oid_hex),
        });
    }

    Ok(entries)
}

fn head_blob_refs(repo_root: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "-z", "HEAD"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") || stderr.contains("unknown revision") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!("git ls-tree failed: {stderr}")));
    }

    let mut refs = Vec::new();
    for record in output.stdout.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&record[..tab]);
        let path = String::from_utf8_lossy(&record[tab + 1..]).to_string();
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() >= 3 && parts[1] == "blob" {
            refs.push((parts[2].to_owned(), path));
        }
    }

    Ok(refs)
}

fn local_lfs_object_path(git_dir: &Path, oid_hex: &str) -> PathBuf {
    git_dir
        .join("lfs")
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

fn ensure_cow_clone_supported(git_dir: &Path) -> Result<()> {
    let tmp = git_dir.join("lfs").join("tmp");
    std::fs::create_dir_all(&tmp).map_err(CrabError::Io)?;
    let stem = format!("dedup-test-{}", std::process::id());
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

fn replace_with_cow_clone(src: &Path, dst: &Path) -> Result<()> {
    let original = std::fs::metadata(dst).map_err(CrabError::Io)?;
    let backup = backup_path(dst);
    std::fs::rename(dst, &backup).map_err(CrabError::Io)?;

    let clone_result = cow_clone::clone_file(src, dst)
        .and_then(|()| std::fs::set_permissions(dst, original.permissions()));
    match clone_result {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Err(source) => {
            let _ = std::fs::remove_file(dst);
            let _ = std::fs::rename(&backup, dst);
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
    path.with_file_name(format!(
        ".{file_name}.crab-lfs-dedup-backup.{}",
        std::process::id()
    ))
}

async fn verify_duplicates(
    repo_root: PathBuf,
    lfs_objects: Vec<LocalLfsObject>,
    crab_pointers: Vec<CrabPointerRef>,
) -> Result<DedupReport> {
    let staging_root = repo_root.join(".crab").join("staging");
    if !staging_root.is_dir() {
        return Ok(DedupReport {
            duplicates: Vec::new(),
            skipped: lfs_objects.len() as u64,
        });
    }

    let staging = StagingArea::open(staging_root).await?;
    let mut by_hash: HashMap<([u8; 32], u64), Vec<CrabPointerRef>> = HashMap::new();
    for pointer_ref in crab_pointers {
        by_hash
            .entry((pointer_ref.pointer.file_hash, pointer_ref.pointer.size))
            .or_default()
            .push(pointer_ref);
    }

    let mut report = DedupReport::default();
    let mut seen_oids = HashSet::new();

    for object in &lfs_objects {
        let digest = match hash_lfs_object(object) {
            Ok(digest) => digest,
            Err(e) => {
                tracing::warn!(oid = %object.oid_hex, error = %e, "skipping unreadable LFS object");
                report.skipped += 1;
                continue;
            }
        };

        if digest.sha256_hex != object.oid_hex {
            tracing::warn!(
                oid = %object.oid_hex,
                actual = %digest.sha256_hex,
                "skipping corrupt local LFS object"
            );
            report.skipped += 1;
            continue;
        }

        let Some(pointer_refs) = by_hash.get(&(digest.blake3, object.size)) else {
            report.skipped += 1;
            continue;
        };

        let mut matched = None;
        for pointer_ref in pointer_refs {
            if staging_matches_lfs_object(&staging, &pointer_ref.pointer, &object.object_path)
                .await?
            {
                matched = Some(pointer_ref.path.clone());
                break;
            }
        }

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
            report.skipped += 1;
        }
    }

    staging.close().await?;
    Ok(report)
}

#[derive(Debug)]
struct LfsDigest {
    sha256_hex: String,
    blake3: [u8; 32],
}

fn hash_lfs_object(object: &LocalLfsObject) -> Result<LfsDigest> {
    let mut file = std::fs::File::open(&object.object_path).map_err(CrabError::Io)?;
    let mut sha = Sha256::new();
    let mut b3 = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(CrabError::Io)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        b3.update(&buf[..n]);
    }
    let sha256: [u8; 32] = sha.finalize().into();
    Ok(LfsDigest {
        sha256_hex: crab_git::lfs_pointer::hex_encode(&sha256),
        blake3: *b3.finalize().as_bytes(),
    })
}

async fn staging_matches_lfs_object(
    staging: &StagingArea,
    pointer: &Pointer,
    object_path: &Path,
) -> Result<bool> {
    let file_hash = MerkleHash::from(pointer.file_hash);
    let chunk_hashes = staging.chunks_for_file(&file_hash)?;
    if chunk_hashes.is_empty() {
        return Ok(false);
    }

    let mut file = std::fs::File::open(object_path).map_err(CrabError::Io)?;
    let mut total = 0u64;

    for chunk_hash in &chunk_hashes {
        let Some(chunk) = staging.get_chunk(chunk_hash).await? else {
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

fn collect_local_lfs_objects(lfs_objects_dir: &Path) -> Result<Vec<LocalLfsObject>> {
    let mut objects = Vec::new();

    for aa_entry in std::fs::read_dir(lfs_objects_dir)
        .map_err(|e| CrabError::Internal(format!("failed to read LFS objects dir: {e}")))?
    {
        let aa_entry = aa_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
        let aa_path = aa_entry.path();
        if !aa_path.is_dir() {
            continue;
        }

        for bb_entry in
            std::fs::read_dir(&aa_path).map_err(|e| CrabError::Internal(format!("{e}")))?
        {
            let bb_entry = bb_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
            let bb_path = bb_entry.path();
            if !bb_path.is_dir() {
                continue;
            }

            for obj_entry in
                std::fs::read_dir(&bb_path).map_err(|e| CrabError::Internal(format!("{e}")))?
            {
                let obj_entry =
                    obj_entry.map_err(|e| CrabError::Internal(format!("dir entry: {e}")))?;
                let name = obj_entry.file_name().to_string_lossy().to_string();
                if name.len() != 64 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let object_path = obj_entry.path();
                let meta = std::fs::metadata(&object_path).map_err(CrabError::Io)?;
                objects.push(LocalLfsObject {
                    oid_hex: name.to_lowercase(),
                    object_path,
                    size: meta.len(),
                });
            }
        }
    }

    Ok(objects)
}

fn collect_crab_pointers(repo_root: &Path) -> Result<Vec<CrabPointerRef>> {
    let mut blob_refs = index_blob_refs(repo_root)?;
    blob_refs.extend(reachable_blob_refs(repo_root)?);
    blob_refs.sort();
    blob_refs.dedup();

    let mut pointers = Vec::new();
    for (blob, path) in batch_read_blobs(repo_root, &blob_refs)? {
        if let Ok(pointer) = Pointer::parse(&blob)
            && pointer.size > 0
        {
            pointers.push(CrabPointerRef { path, pointer });
        }
    }
    Ok(pointers)
}

fn index_blob_refs(repo_root: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["ls-files", "-s"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-files: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let mut refs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() >= 2 {
            refs.push((parts[1].to_owned(), path.to_owned()));
        }
    }
    Ok(refs)
}

fn reachable_blob_refs(repo_root: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["rev-list", "--all", "--objects"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("bad default revision") || stderr.contains("does not have any commits") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(hash, path)| (hash.to_owned(), path.to_owned()))
        .collect())
}

fn batch_read_blobs(
    repo_root: &Path,
    blob_refs: &[(String, String)],
) -> Result<Vec<(Vec<u8>, String)>> {
    if blob_refs.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for chunk in blob_refs.chunks(500) {
        let input = chunk
            .iter()
            .map(|(hash, _)| hash.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut child = Command::new("git")
            .args(["cat-file", "--batch"])
            .current_dir(repo_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
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
        parse_batch_output(&output.stdout, chunk, &mut out);
    }
    Ok(out)
}

fn parse_batch_output(stdout: &[u8], refs: &[(String, String)], out: &mut Vec<(Vec<u8>, String)>) {
    let mut pos = 0;
    let mut index = 0;
    while pos < stdout.len() && index < refs.len() {
        let Some(header_rel_end) = stdout[pos..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header_end = pos + header_rel_end;
        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            index += 1;
            continue;
        }
        let Ok(size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            index += 1;
            continue;
        };
        let content_start = header_end + 1;
        let content_end = content_start + size;
        if content_end > stdout.len() {
            break;
        }
        if parts[1] == "blob" {
            out.push((
                stdout[content_start..content_end].to_vec(),
                refs[index].1.clone(),
            ));
        }
        pos = content_end + 1;
        index += 1;
    }
}

fn discover_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to discover repo root: {e}")))?;
    if !output.status.success() {
        return Err(CrabError::Internal("not inside a git repository".into()));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn discover_git_dir(repo_root: &Path) -> Result<PathBuf> {
    crate::git::discover::discover_common_git_dir_from(repo_root)
}

fn cleanup_empty_dirs(lfs_objects_dir: &Path) {
    if let Ok(top_entries) = std::fs::read_dir(lfs_objects_dir) {
        for aa_entry in top_entries.flatten() {
            let aa_path = aa_entry.path();
            if !aa_path.is_dir() {
                continue;
            }
            if let Ok(bb_entries) = std::fs::read_dir(&aa_path) {
                for bb_entry in bb_entries.flatten() {
                    let bb_path = bb_entry.path();
                    if bb_path.is_dir() {
                        let _ = std::fs::remove_dir(&bb_path);
                    }
                }
            }
            let _ = std::fs::remove_dir(&aa_path);
        }
    }
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

    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _guard = CWD_LOCK.lock().unwrap();
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
    }

    #[test]
    fn collect_crab_pointers_includes_index() {
        let _guard = CWD_LOCK.lock().unwrap();
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
        let _guard = CWD_LOCK.lock().unwrap();
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
}
