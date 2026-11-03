//! Cache warmers for the background indexer.
//!
//! Each warmer populates a specific cache entry in the SlateDB-backed
//! index store. They are designed to be cancellation-safe: each checks
//! a `CancellationToken` between I/O steps and bails early if the
//! token is cancelled.

use std::path::Path;

use serde::Serialize;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::store::IndexStore;

/// Errors specific to warmer execution.
#[derive(Debug, thiserror::Error)]
pub enum WarmerError {
    #[error("git command failed: {0}")]
    Git(String),

    #[error("store error: {0}")]
    Store(#[from] super::store::IndexStoreError),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("warmer cancelled")]
    Cancelled,
}

type Result<T> = std::result::Result<T, WarmerError>;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve HEAD to a short sha for use as the cache revision key.
async fn resolve_head(work_dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .kill_on_drop(true)
        .args(["rev-parse", "HEAD"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(|e| WarmerError::Git(format!("failed to spawn git rev-parse: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WarmerError::Git(format!(
            "git rev-parse HEAD failed: {stderr}"
        )));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(sha)
}

/// Store a serializable value in the index under the standard key format.
async fn store_json<T: Serialize>(
    store: &IndexStore,
    repo_id: &str,
    kind: &str,
    rev: &str,
    value: &T,
) -> Result<()> {
    let key = IndexStore::make_key(repo_id, kind, rev);
    let json = serde_json::to_vec(value)?;
    store.put(&key, &json).await?;
    Ok(())
}

/// Check the cancellation token; return `Err(Cancelled)` if fired.
fn check_cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        return Err(WarmerError::Cancelled);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// File summaries
// ---------------------------------------------------------------------------

/// Summary metadata for a single tracked file.
#[derive(Debug, Serialize)]
struct FileSummaryEntry {
    path: String,
    size: u64,
    extension: Option<String>,
    is_pointer: bool,
}

/// Warm the file-summary cache entry.
///
/// Runs `git ls-files -z` to enumerate tracked files, then collects
/// basic metadata (size, extension, pointer status) for each. The
/// result is stored as JSON under key `"{repo_id}:file_summary:{HEAD}"`.
pub async fn warm_file_summaries(
    store: &IndexStore,
    work_dir: &Path,
    repo_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;

    let head = resolve_head(work_dir).await?;
    check_cancelled(cancel)?;

    let output = Command::new("git")
        .kill_on_drop(true)
        .args(["ls-files", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(|e| WarmerError::Git(format!("failed to spawn git ls-files: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WarmerError::Git(format!("git ls-files failed: {stderr}")));
    }

    check_cancelled(cancel)?;

    let mut entries = Vec::new();
    for raw_path in output.stdout.split(|&b| b == 0) {
        if raw_path.is_empty() {
            continue;
        }
        check_cancelled(cancel)?;

        let rel_path = String::from_utf8_lossy(raw_path).into_owned();
        let full_path = work_dir.join(&rel_path);

        let (size, is_pointer) = match tokio::fs::metadata(&full_path).await {
            Ok(meta) => {
                let sz = meta.len();
                // Pointer files are small (≤256 bytes) and start with the version line.
                let ptr = if sz <= 256 {
                    match tokio::fs::read(&full_path).await {
                        Ok(bytes) => is_crab_pointer(&bytes),
                        Err(_) => false,
                    }
                } else {
                    false
                };
                (sz, ptr)
            }
            Err(_) => (0, false),
        };

        let extension = Path::new(&rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_owned());

        entries.push(FileSummaryEntry {
            path: rel_path,
            size,
            extension,
            is_pointer,
        });
    }

    debug!(
        repo_id = %repo_id,
        head = %head,
        file_count = entries.len(),
        "warmer: file summaries collected"
    );

    store_json(store, repo_id, "file_summary", &head, &entries).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Git log skeleton
// ---------------------------------------------------------------------------

/// A single commit in the git-log skeleton.
#[derive(Debug, Serialize)]
struct LogEntry {
    sha: String,
    author: String,
    date: String,
    subject: String,
}

/// Warm the git-log skeleton cache entry.
///
/// Runs `git log` with a compact format to build a skeleton of recent
/// commits (sha, author, date, subject — no diffs). Stores under key
/// `"{repo_id}:git_log:{HEAD}"`.
pub async fn warm_git_log(
    store: &IndexStore,
    work_dir: &Path,
    repo_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;

    let head = resolve_head(work_dir).await?;
    check_cancelled(cancel)?;

    // Use a record separator (%x1e) between fields and %x1f between entries
    // to avoid ambiguity with commit messages containing newlines.
    let format = "%H%x1e%an%x1e%aI%x1e%s";
    let output = Command::new("git")
        .kill_on_drop(true)
        .args(["log", "--format", format, "-n", "500"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(|e| WarmerError::Git(format!("failed to spawn git log: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WarmerError::Git(format!("git log failed: {stderr}")));
    }

    check_cancelled(cancel)?;

    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, '\x1e').collect();
        if parts.len() < 4 {
            continue;
        }
        entries.push(LogEntry {
            sha: parts[0].to_owned(),
            author: parts[1].to_owned(),
            date: parts[2].to_owned(),
            subject: parts[3].to_owned(),
        });
    }

    debug!(
        repo_id = %repo_id,
        head = %head,
        commit_count = entries.len(),
        "warmer: git log skeleton collected"
    );

    store_json(store, repo_id, "git_log", &head, &entries).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pointer metadata
// ---------------------------------------------------------------------------

/// Metadata for a single crab pointer file.
#[derive(Debug, Serialize)]
struct PointerMeta {
    path: String,
    file_hash: String,
    size: u64,
    has_shard_hint: bool,
}

/// Warm the pointer-metadata cache entry.
///
/// Scans tracked files for crab pointer blobs and extracts their
/// metadata (hash, size, shard-hint presence). Stores under key
/// `"{repo_id}:pointer_meta:{HEAD}"`.
pub async fn warm_pointer_metadata(
    store: &IndexStore,
    work_dir: &Path,
    repo_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;

    let head = resolve_head(work_dir).await?;
    check_cancelled(cancel)?;

    // Get the list of tracked files.
    let output = Command::new("git")
        .kill_on_drop(true)
        .args(["ls-files", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(|e| WarmerError::Git(format!("failed to spawn git ls-files: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WarmerError::Git(format!("git ls-files failed: {stderr}")));
    }

    check_cancelled(cancel)?;

    let mut pointers = Vec::new();

    for raw_path in output.stdout.split(|&b| b == 0) {
        if raw_path.is_empty() {
            continue;
        }
        check_cancelled(cancel)?;

        let rel_path = String::from_utf8_lossy(raw_path).into_owned();
        let full_path = work_dir.join(&rel_path);

        // Only read small files that could be pointers (≤256 bytes).
        let meta = match tokio::fs::metadata(&full_path).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.len() > 256 {
            continue;
        }

        let bytes = match tokio::fs::read(&full_path).await {
            Ok(b) => b,
            Err(_) => continue,
        };

        if let Some(parsed) = parse_pointer_meta(&bytes) {
            pointers.push(PointerMeta {
                path: rel_path,
                file_hash: parsed.0,
                size: parsed.1,
                has_shard_hint: parsed.2,
            });
        }
    }

    debug!(
        repo_id = %repo_id,
        head = %head,
        pointer_count = pointers.len(),
        "warmer: pointer metadata collected"
    );

    store_json(store, repo_id, "pointer_meta", &head, &pointers).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pointer detection / parsing helpers
// ---------------------------------------------------------------------------

/// The version line that identifies a crab pointer.
const POINTER_VERSION_LINE: &str = "version https://crab.dev/spec/v1";

/// Quick check: does this byte slice look like a crab pointer?
fn is_crab_pointer(bytes: &[u8]) -> bool {
    if bytes.len() > 256 {
        return false;
    }
    if bytes.last() != Some(&b'\n') {
        return false;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return false,
    };
    text.starts_with(POINTER_VERSION_LINE)
}

/// Parse pointer fields from raw bytes. Returns `(hex_hash, size, has_shard_hint)`
/// or `None` if the bytes are not a valid pointer.
fn parse_pointer_meta(bytes: &[u8]) -> Option<(String, u64, bool)> {
    if bytes.len() > 256 || bytes.last() != Some(&b'\n') {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();

    // Line 1: version
    let version = lines.next()?;
    if version != POINTER_VERSION_LINE {
        return None;
    }

    // Line 2: file-hash {64-hex}
    let hash_line = lines.next()?;
    let hex = hash_line.strip_prefix("file-hash ")?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    // Line 3: size {decimal}
    let size_line = lines.next()?;
    let size_str = size_line.strip_prefix("size ")?;
    let size: u64 = size_str.parse().ok()?;

    // Line 4 (optional): shard-hint
    let has_shard_hint = lines.next().is_some_and(|l| l.starts_with("shard-hint "));

    Some((hex.to_owned(), size, has_shard_hint))
}

/// Run all warmers for a repository. Errors from individual warmers
/// are logged but do not abort the remaining warmers.
pub async fn warm_all(
    store: &IndexStore,
    work_dir: &Path,
    repo_id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    if let Err(e) = warm_file_summaries(store, work_dir, repo_id, cancel).await {
        match e {
            WarmerError::Cancelled => return Err(e),
            other => warn!(repo_id = %repo_id, error = %other, "file summary warmer failed"),
        }
    }

    if let Err(e) = warm_git_log(store, work_dir, repo_id, cancel).await {
        match e {
            WarmerError::Cancelled => return Err(e),
            other => warn!(repo_id = %repo_id, error = %other, "git log warmer failed"),
        }
    }

    if let Err(e) = warm_pointer_metadata(store, work_dir, repo_id, cancel).await {
        match e {
            WarmerError::Cancelled => return Err(e),
            other => warn!(repo_id = %repo_id, error = %other, "pointer metadata warmer failed"),
        }
    }

    Ok(())
}
