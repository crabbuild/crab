//! History rewriting for LFS migration.
//!
//! Implements full history rewriting via the same `git fast-export` →
//! transform → `git fast-import` shape used by `git lfs migrate import/export`.
//!
//! The `import` operation rewrites every commit so that blobs matching the
//! include pattern are replaced with LFS pointers, `.gitattributes` is
//! updated in each commit, and original content is uploaded to the remote
//! LFS store.
//!
//! The `export` operation reverses this: LFS pointers matching the pattern
//! are replaced with their original content (downloaded from the store or
//! local cache), and `.gitattributes` is updated with Git LFS-style untrack
//! overrides. With `--to-crab`, LFS tracking is replaced by Crab tracking.
//!
//! Also provides `migrate info` for analyzing repository contents before
//! migration.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::{Component, Path};
use std::process::Command;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_lfs::LfsObjectStore;
use crab_staging::StagingArea;
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;

const DEFAULT_MIGRATE_INFO_TOP: usize = 5;
const LFS_OBJECTS_PATTERN: &str = "LFS Objects";

/// Result of `migrate info` — one row per file extension or pattern.
#[derive(Debug, Clone)]
pub struct MigrateInfoEntry {
    /// File extension or pattern (e.g. `*.bin`, `*.psd`).
    pub pattern: String,
    /// Number of files matching this pattern across history.
    pub file_count: u64,
    /// Total size in bytes of all matching files.
    pub total_size: u64,
}

/// Result of `migrate info --pointers` — one row per LFS pointer in HEAD.
#[derive(Debug, Clone)]
pub struct PointerInfoEntry {
    /// File path in the tree.
    pub path: String,
    /// LFS OID (hex-encoded SHA-256).
    pub oid: String,
    /// Declared size in the pointer.
    pub size: u64,
}

pub struct MigrateImportOptions<'a> {
    pub include: Option<&'a str>,
    pub exclude: Option<&'a str>,
    pub above: Option<&'a str>,
    pub fixup: bool,
    pub no_rewrite: bool,
    pub no_rewrite_files: Vec<String>,
    pub message: Option<&'a str>,
    pub object_map: Option<&'a str>,
    pub refs: MigrateRefSelection,
    pub yes: bool,
    pub verbose: bool,
    pub from_crab: bool,
}

pub struct MigrateExportOptions<'a> {
    pub include: &'a str,
    pub exclude: Option<&'a str>,
    pub object_map: Option<&'a str>,
    pub remote: Option<&'a str>,
    pub refs: MigrateRefSelection,
    pub yes: bool,
    pub verbose: bool,
    pub to_crab: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MigrateInfoOptions<'a> {
    pub above: Option<&'a str>,
    pub include: Option<&'a str>,
    pub exclude: Option<&'a str>,
    pub top: Option<usize>,
    pub unit: Option<&'a str>,
    pub pointer_mode: MigrateInfoPointerMode,
    pub fixup: bool,
    pub refs: &'a MigrateRefSelection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrateRefSelection {
    pub everything: bool,
    pub include_refs: Vec<String>,
    pub exclude_refs: Vec<String>,
    pub branches: Vec<String>,
    pub skip_fetch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateInfoPointerMode {
    /// Count existing LFS pointers by their referenced object sizes.
    Follow,
    /// Count existing LFS pointer blobs as ordinary files.
    NoFollow,
    /// Ignore existing LFS pointer blobs.
    Ignore,
    /// Legacy Crab behavior: print pointer rows from HEAD.
    PointersOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeUnit {
    B,
    Kb,
    Mb,
    Gb,
    Tb,
    Pb,
    KiB,
    MiB,
    GiB,
    TiB,
    PiB,
}

// ---------------------------------------------------------------------------
// Precondition checks & ref helpers
// ---------------------------------------------------------------------------

fn require_clean_working_tree(allow_dirty: bool) -> Result<()> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| CrabError::LfsMigrationFailed {
            reason: format!("failed to check working tree status: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::LfsMigrationFailed {
            reason: format!("git status failed: {stderr}"),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        if allow_dirty {
            eprintln!(
                "warning: --yes supplied; continuing with a dirty working tree that may be overwritten"
            );
            return Ok(());
        }
        return Err(CrabError::LfsMigrationFailed {
            reason: "working tree is not clean — commit or stash changes before migrating"
                .to_owned(),
        });
    }

    Ok(())
}

fn get_current_branch() -> Result<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        .map_err(|e| mig_err(format!("failed to get current branch: {e}")))?;

    if !output.status.success() {
        return Err(mig_err(
            "HEAD is detached — checkout a branch before migrating",
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RevisionSelection {
    args: Vec<String>,
    positive_refs: Vec<String>,
    excluded_refs: Vec<String>,
    all_refs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RefStateScope {
    All,
    Refs(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRefs {
    revision_args: Vec<String>,
    state_scope: RefStateScope,
    scope_label: String,
}

#[cfg(test)]
fn build_revision_selection(
    selection: &MigrateRefSelection,
    default_ref: Option<String>,
) -> Result<RevisionSelection> {
    build_revision_selection_with_remote_refs(selection, default_ref, &[])
}

fn build_revision_selection_with_remote_refs(
    selection: &MigrateRefSelection,
    default_ref: Option<String>,
    remote_refs: &[String],
) -> Result<RevisionSelection> {
    let mut positive_refs = Vec::new();
    let mut excluded_refs = Vec::new();

    for branch in &selection.branches {
        if let Some(excluded) = branch.strip_prefix('^') {
            if !excluded.is_empty() {
                push_unique(&mut excluded_refs, excluded.to_owned());
            }
        } else {
            push_unique(&mut positive_refs, branch.clone());
        }
    }

    for refname in &selection.include_refs {
        push_unique(&mut positive_refs, refname.clone());
    }
    for refname in &selection.exclude_refs {
        push_unique(&mut excluded_refs, refname.clone());
    }

    if selection.everything {
        if !positive_refs.is_empty() {
            return Err(mig_err(
                "migrate --everything cannot be combined with branch operands or --include-ref",
            ));
        }

        let mut args = vec!["--all".to_owned()];
        args.extend(excluded_refs.iter().map(|refname| format!("^{refname}")));
        return Ok(RevisionSelection {
            args,
            positive_refs,
            excluded_refs,
            all_refs: true,
        });
    }

    if positive_refs.is_empty() {
        let default_ref = default_ref.ok_or_else(|| {
            mig_err("migrate needs a branch, --include-ref, or --everything scope")
        })?;
        push_unique(&mut positive_refs, default_ref);
    }

    if should_exclude_remote_refs(selection) {
        for refname in remote_refs {
            if !positive_refs.contains(refname) {
                push_unique(&mut excluded_refs, refname.clone());
            }
        }
    }

    let mut args = positive_refs.clone();
    args.extend(excluded_refs.iter().map(|refname| format!("^{refname}")));

    Ok(RevisionSelection {
        args,
        positive_refs,
        excluded_refs,
        all_refs: false,
    })
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn resolve_ref_selection(selection: &MigrateRefSelection) -> Result<ResolvedRefs> {
    refresh_remote_refs_for_migrate(selection)?;

    let default_ref = if selection.everything {
        None
    } else {
        Some(get_current_branch()?)
    };
    let remote_refs = if should_exclude_remote_refs(selection) {
        remote_tracking_refs()?
    } else {
        Vec::new()
    };
    let revision = build_revision_selection_with_remote_refs(selection, default_ref, &remote_refs)?;
    let scope_label = describe_revision_scope(&revision);

    if revision.all_refs {
        return Ok(ResolvedRefs {
            revision_args: revision.args,
            state_scope: RefStateScope::All,
            scope_label,
        });
    }

    let state_refs = resolve_symbolic_refs(&revision.positive_refs)?;

    Ok(ResolvedRefs {
        revision_args: revision.args,
        state_scope: RefStateScope::Refs(state_refs),
        scope_label,
    })
}

fn refresh_remote_refs_for_migrate(selection: &MigrateRefSelection) -> Result<()> {
    if !should_refresh_remote_refs(selection) {
        return Ok(());
    }

    let remotes = git_remote_names()?;
    for remote in remotes {
        fetch_remote_refs(&remote)?;
    }
    Ok(())
}

fn should_refresh_remote_refs(selection: &MigrateRefSelection) -> bool {
    !selection.skip_fetch && selection.include_refs.is_empty() && selection.exclude_refs.is_empty()
}

fn should_exclude_remote_refs(selection: &MigrateRefSelection) -> bool {
    !selection.everything && selection.include_refs.is_empty() && selection.exclude_refs.is_empty()
}

fn git_remote_names() -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["remote"])
        .output()
        .map_err(|e| mig_err(format!("failed to list git remotes: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git remote failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_git_remote_names(&stdout))
}

fn remote_tracking_refs() -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/remotes"])
        .output()
        .map_err(|e| mig_err(format!("failed to list remote-tracking refs: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!(
            "git for-each-ref refs/remotes failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_git_remote_names(&stdout))
}

fn parse_git_remote_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn fetch_remote_refs(remote: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["fetch", "--prune", "--quiet", remote])
        .output()
        .map_err(|e| mig_err(format!("failed to fetch refs from remote {remote}: {e}")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(mig_err(format!(
        "failed to fetch refs from remote {remote}: {stderr}"
    )))
}

fn describe_revision_scope(selection: &RevisionSelection) -> String {
    let mut scope = if selection.all_refs {
        "all refs".to_owned()
    } else {
        selection.positive_refs.join(", ")
    };

    if !selection.excluded_refs.is_empty() {
        scope.push_str("; excluding ");
        scope.push_str(&selection.excluded_refs.join(", "));
    }

    scope
}

fn resolve_symbolic_refs(refs: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for refname in refs {
        let output = Command::new("git")
            .args(["rev-parse", "--symbolic-full-name", "--verify", refname])
            .output()
            .map_err(|e| mig_err(format!("failed to resolve ref {refname}: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(mig_err(format!(
                "failed to resolve ref {refname}: {stderr}"
            )));
        }

        let full_name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if full_name.starts_with("refs/") && !resolved.contains(&full_name) {
            resolved.push(full_name);
        }
    }

    Ok(resolved)
}

fn save_ref_state(scope: &RefStateScope) -> Result<Vec<(String, String)>> {
    let output = match scope {
        RefStateScope::All => Command::new("git")
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output(),
        RefStateScope::Refs(refs) => {
            if refs.is_empty() {
                return Ok(Vec::new());
            }
            Command::new("git")
                .args(["for-each-ref", "--format=%(refname) %(objectname)"])
                .args(refs)
                .output()
        }
    };

    let output = output.map_err(|e| mig_err(format!("failed to save ref state: {e}")))?;
    let mut state = Vec::new();

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some((refname, hash)) = line.split_once(' ') {
                state.push((refname.to_owned(), hash.to_owned()));
            }
        }
    }

    Ok(state)
}

fn restore_ref_state(state: &[(String, String)]) {
    // Best-effort ref restoration. If individual `git update-ref`
    // calls fail (e.g., repository state is corrupt after a partial
    // `fast-import`), we log and continue — the user can use
    // `git reflog` and `git update-ref` manually to recover. See
    // finding CR9-F8.
    for (refname, hash) in state {
        match Command::new("git")
            .args(["update-ref", refname, hash])
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    refname = %refname,
                    hash = %hash,
                    stderr = %stderr.trim(),
                    "failed to restore ref during migration rollback"
                );
                eprintln!(
                    "warning: failed to restore {refname} -> {hash}. \
                     Use `git reflog` and `git update-ref` to recover manually."
                );
            }
            Err(e) => {
                tracing::warn!(
                    refname = %refname,
                    error = %e,
                    "failed to invoke git update-ref during rollback"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fast-export stream types
// ---------------------------------------------------------------------------

/// A file operation within a commit.
#[derive(Debug, Clone)]
struct FileOp {
    kind: FileOpKind,
}

#[derive(Debug, Clone)]
enum FileOpKind {
    /// `M <mode> <dataref> <path>`.
    Modify {
        mode: String,
        dataref: String,
        path: String,
    },
    /// `M <mode> inline <path>` with inline data.
    ModifyInline {
        mode: String,
        path: String,
        data: Vec<u8>,
    },
    /// `D <path>`.
    Delete { path: String },
    /// deleteall, C, R, N — passed through as raw text.
    Passthrough { raw: String },
}

// ---------------------------------------------------------------------------
// Stream parsing helpers
// ---------------------------------------------------------------------------

fn parse_mark_line(line: &str) -> Option<u64> {
    line.strip_prefix("mark :")?.parse::<u64>().ok()
}

fn parse_data_line(line: &str) -> Option<usize> {
    line.strip_prefix("data ")?.parse::<usize>().ok()
}

fn read_exact_bytes(reader: &mut dyn BufRead, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf).map_err(|e| {
        mig_err(format!(
            "failed to read {size} bytes from fast-export stream: {e}"
        ))
    })?;
    Ok(buf)
}

fn consume_optional_lf(reader: &mut dyn BufRead) -> Result<()> {
    let buf = reader.fill_buf().map_err(io_err)?;
    if !buf.is_empty() && buf[0] == b'\n' {
        reader.consume(1);
    }
    Ok(())
}

fn unquote_path(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        let inner = &s[1..s.len() - 1];
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('\\') | None => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some(other) => {
                        result.push('\\');
                        result.push(other);
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    } else {
        s.to_owned()
    }
}

fn quote_path(s: &str) -> String {
    if s.contains(' ') || s.contains('"') || s.contains('\\') || s.contains('\n') {
        let mut result = String::with_capacity(s.len() + 2);
        result.push('"');
        for c in s.chars() {
            match c {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\t' => result.push_str("\\t"),
                _ => result.push(c),
            }
        }
        result.push('"');
        result
    } else {
        s.to_owned()
    }
}

fn io_err(e: std::io::Error) -> CrabError {
    CrabError::LfsMigrationFailed {
        reason: format!("I/O error: {e}"),
    }
}

fn mig_err(reason: impl Into<String>) -> CrabError {
    CrabError::LfsMigrationFailed {
        reason: reason.into(),
    }
}

fn is_lfs_pointer(content: &[u8]) -> bool {
    if content.len() > crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE {
        return false;
    }
    matches!(crab_git::classify(content), crab_git::PointerKind::Lfs(_))
}

fn parse_crab_pointer(content: &[u8]) -> Option<Pointer> {
    Pointer::parse(content).ok()
}

fn import_source_size(content: &[u8], from_crab: bool) -> Option<u64> {
    if from_crab {
        return parse_crab_pointer(content).map(|pointer| pointer.size);
    }

    if is_lfs_pointer(content) {
        None
    } else {
        Some(content.len() as u64)
    }
}

fn lfs_pointer_for_content(
    content: &[u8],
    remote_store: Option<&Arc<LfsObjectStore>>,
    context: &str,
) -> Result<Vec<u8>> {
    let oid: [u8; 32] = Sha256::digest(content).into();
    let pointer = LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };

    if let Some(store) = remote_store {
        let content_bytes = bytes::Bytes::copy_from_slice(content);
        let store = Arc::clone(store);
        crate::cmd::lfs::block_on_runtime(async move {
            store
                .put(&oid, content_bytes)
                .await
                .map_err(CrabError::from)
        })
        .map_err(|e| mig_err(format!("failed to upload {context}: {e}")))?;
    }

    cache_lfs_object(&oid, content.len() as u64, content)?;
    Ok(pointer.serialize())
}

fn resolve_crab_hydrator(operation: &str) -> Result<crate::cmd::hydrate::ShardHydrator> {
    let remote_url = crate::cmd::lfs::store_setup::read_repo_remote_url()?;
    let config = crate::core::config::Config::resolve_local().unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let layout = crate::cmd::lfs::block_on_runtime(async {
        crate::cmd::lfs::store_setup::resolve_crab_read_layout(
            &remote_url,
            operation,
            &config,
            &cancel,
        )
        .await
    })?;
    let caching_store = crab_cache_store::CachingStore::new(layout.store().clone(), &config.cache)
        .map_err(CrabError::from)?;
    crate::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(caching_store, layout, &config)
}

fn resolve_crab_content(
    pointer_bytes: &[u8],
    hydrator: &crate::cmd::hydrate::ShardHydrator,
) -> Result<Vec<u8>> {
    crate::cmd::lfs::block_on_runtime(async {
        hydrator.reconstruct_from_pointer(pointer_bytes).await
    })
}

fn open_migrate_staging() -> Result<StagingArea> {
    let ctx = crate::git::worktree::WorktreeContext::resolve()?;
    let staging_root = ctx.shared_staging_dir();
    crate::cmd::lfs::block_on_runtime(async {
        StagingArea::open_blocking_default(staging_root)
            .await
            .map_err(CrabError::from)
    })
}

fn crab_pointer_for_content(
    staging: &StagingArea,
    path: &str,
    content: Vec<u8>,
) -> Result<Vec<u8>> {
    let chunk_result = crate::cmd::lfs::block_on_runtime(async {
        crate::engine::chunk_file::chunk_file(Cursor::new(content)).await
    })?;
    let file_hash = chunk_result.file_hash;
    let total_bytes = chunk_result.total_bytes;
    let chunks = chunk_result
        .chunks
        .iter()
        .map(|chunk| {
            u64::try_from(chunk.data.len())
                .map(|size| (chunk.hash, size))
                .map_err(|_| {
                    CrabError::StagingCorrupt(
                        "migration chunk size cannot be represented as u64".to_owned(),
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let recipe = FileRecipe::from_staged_chunks(
        ChunkingPolicyId::XetGearV1_64KiB,
        file_hash,
        total_bytes,
        &chunks,
    )?;

    staging.pre_register_file_with_path(&file_hash, total_bytes, path)?;
    if let Some(published) = staging.published_recipe_for_file(&file_hash)? {
        if published != recipe {
            return Err(CrabError::StagingCorrupt(format!(
                "published recipe for {} differs from deterministic migration output",
                file_hash.hex()
            )));
        }
        staging.publish_verified_history_recipe(&recipe)?;
        let raw_file_hash: [u8; 32] = file_hash.into();
        return Ok(Pointer {
            file_hash: raw_file_hash,
            size: total_bytes,
            shard_hint: None,
        }
        .serialize());
    }
    let existing = staging.chunks_for_file_with_sizes(&file_hash)?;
    let existing_bytes = existing.iter().try_fold(0u64, |acc, (_, size)| {
        acc.checked_add(*size).ok_or_else(|| {
            mig_err(format!(
                "staged chunks for {} exceed u64 byte count",
                file_hash.hex()
            ))
        })
    })?;

    if !existing.is_empty() {
        return Err(mig_err(format!(
            "existing staging rows for {} cover {existing_bytes} byte(s), expected {total_bytes}; \
             run `crab staging verify` before migrating",
            file_hash.hex()
        )));
    }

    let refs: Vec<(&MerkleHash, &[u8])> = chunk_result
        .chunks
        .iter()
        .map(|chunk| (&chunk.hash, chunk.data.as_ref()))
        .collect();
    if !refs.is_empty() {
        crate::cmd::lfs::block_on_runtime(async {
            staging
                .stage_chunks_batch(&refs, &file_hash, 0)
                .await
                .map_err(CrabError::from)
        })?;
        crate::cmd::lfs::block_on_runtime(async {
            staging.flush_pending().await.map_err(CrabError::from)
        })?;
    }
    staging.publish_verified_history_recipe(&recipe)?;

    let raw_file_hash: [u8; 32] = file_hash.into();
    Ok(Pointer {
        file_hash: raw_file_hash,
        size: total_bytes,
        shard_hint: None,
    }
    .serialize())
}

// ---------------------------------------------------------------------------
// Two-pass rewriting engine
// ---------------------------------------------------------------------------

/// Collected blob from the fast-export stream.
struct ExportedBlob {
    mark: u64,
    data: Vec<u8>,
}

/// Collected commit from the fast-export stream.
struct ExportedCommit {
    /// Raw header lines (commit ref, mark, author, committer, encoding,
    /// from, merge, data <N>, <message>).
    header_lines: Vec<String>,
    mark: Option<u64>,
    original_oid: Option<String>,
    file_ops: Vec<FileOp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerboseMigrationEntry {
    commit: String,
    path: String,
}

/// Any non-blob, non-commit section (reset, tag, progress, etc.).
struct PassthroughSection {
    lines: Vec<String>,
}

/// Parsed fast-export stream.
struct ExportStream {
    /// All sections in order. Each is either a blob index, commit index,
    /// or passthrough index into the respective vecs.
    order: Vec<SectionRef>,
    blobs: Vec<ExportedBlob>,
    commits: Vec<ExportedCommit>,
    passthroughs: Vec<PassthroughSection>,
}

#[derive(Clone, Copy)]
enum SectionRef {
    Blob(usize),
    Commit(usize),
    Passthrough(usize),
}

/// Parse the entire fast-export output into structured sections.
fn parse_export_stream(raw: &[u8]) -> Result<ExportStream> {
    let mut reader = BufReader::new(std::io::Cursor::new(raw));
    let mut stream = ExportStream {
        order: Vec::new(),
        blobs: Vec::new(),
        commits: Vec::new(),
        passthroughs: Vec::new(),
    };

    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf).map_err(io_err)?;
        if n == 0 {
            break;
        }
        let trimmed = line_buf.trim_end_matches('\n').to_owned();

        if trimmed == "blob" {
            let blob = parse_blob(&mut reader)?;
            let idx = stream.blobs.len();
            stream.blobs.push(blob);
            stream.order.push(SectionRef::Blob(idx));
        } else if trimmed.starts_with("commit ") {
            let commit = parse_commit(&trimmed, &mut reader)?;
            let idx = stream.commits.len();
            stream.commits.push(commit);
            stream.order.push(SectionRef::Commit(idx));
        } else if trimmed.starts_with("reset ")
            || trimmed.starts_with("tag ")
            || trimmed.starts_with("progress ")
            || trimmed.starts_with("checkpoint")
            || trimmed.starts_with("done")
            || trimmed.starts_with("feature ")
            || trimmed.starts_with("option ")
        {
            let section = parse_passthrough(&trimmed, &mut reader)?;
            let idx = stream.passthroughs.len();
            stream.passthroughs.push(section);
            stream.order.push(SectionRef::Passthrough(idx));
        } else if !trimmed.is_empty() {
            // Unknown line — pass through.
            stream.passthroughs.push(PassthroughSection {
                lines: vec![trimmed],
            });
            stream
                .order
                .push(SectionRef::Passthrough(stream.passthroughs.len() - 1));
        }
    }

    Ok(stream)
}

fn parse_blob(reader: &mut dyn BufRead) -> Result<ExportedBlob> {
    let mut line = String::new();

    // Read mark line.
    line.clear();
    reader.read_line(&mut line).map_err(io_err)?;
    let mark = parse_mark_line(line.trim_end_matches('\n'))
        .ok_or_else(|| mig_err("expected mark line after blob"))?;

    // Read data line, allowing `git fast-export --show-original-ids`
    // to insert an original-oid line that fast-import must not see.
    line.clear();
    reader.read_line(&mut line).map_err(io_err)?;
    let mut data_line = line.trim_end_matches('\n').to_owned();
    if data_line.starts_with("original-oid ") {
        line.clear();
        reader.read_line(&mut line).map_err(io_err)?;
        data_line = line.trim_end_matches('\n').to_owned();
    }
    let size =
        parse_data_line(&data_line).ok_or_else(|| mig_err("expected data line after blob mark"))?;

    let data = read_exact_bytes(reader, size)?;
    consume_optional_lf(reader)?;

    Ok(ExportedBlob { mark, data })
}

fn parse_commit(first_line: &str, reader: &mut dyn BufRead) -> Result<ExportedCommit> {
    let mut header_lines = vec![first_line.to_owned()];
    let mut mark = None;
    let mut original_oid = None;
    let mut line = String::new();

    // Read header lines until file operations or blank line.
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(io_err)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');

        if trimmed.is_empty() {
            break;
        }

        if let Some(m) = parse_mark_line(trimmed) {
            mark = Some(m);
            header_lines.push(trimmed.to_owned());
        } else if let Some(oid) = trimmed.strip_prefix("original-oid ") {
            original_oid = Some(oid.to_owned());
        } else if trimmed.starts_with("data ") {
            let size =
                parse_data_line(trimmed).ok_or_else(|| mig_err("invalid data line in commit"))?;
            let msg_bytes = read_exact_bytes(reader, size)?;
            consume_optional_lf(reader)?;
            let msg = String::from_utf8_lossy(&msg_bytes).to_string();
            header_lines.push(format!("data {size}"));
            header_lines.push(msg);
        } else if trimmed.starts_with("M ")
            || trimmed.starts_with("D ")
            || trimmed.starts_with("C ")
            || trimmed.starts_with("R ")
            || trimmed.starts_with("N ")
            || trimmed == "deleteall"
        {
            let file_ops = parse_file_ops(trimmed, reader)?;
            return Ok(ExportedCommit {
                header_lines,
                mark,
                original_oid,
                file_ops,
            });
        } else {
            header_lines.push(trimmed.to_owned());
        }
    }

    Ok(ExportedCommit {
        header_lines,
        mark,
        original_oid,
        file_ops: Vec::new(),
    })
}

fn parse_file_ops(first_line: &str, reader: &mut dyn BufRead) -> Result<Vec<FileOp>> {
    let mut ops = Vec::new();
    let mut current = first_line.to_owned();

    loop {
        ops.push(parse_single_file_op(&current, reader)?);

        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(io_err)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            break;
        }
        current = trimmed.to_owned();
    }

    Ok(ops)
}

fn parse_single_file_op(line: &str, reader: &mut dyn BufRead) -> Result<FileOp> {
    if let Some(rest) = line.strip_prefix("M ") {
        let parts: Vec<&str> = rest.splitn(3, ' ').collect();
        if parts.len() < 3 {
            return Ok(FileOp {
                kind: FileOpKind::Passthrough {
                    raw: line.to_owned(),
                },
            });
        }
        let mode = parts[0].to_owned();
        let dataref = parts[1];
        let path = unquote_path(parts[2]);

        if dataref == "inline" {
            let mut data_line = String::new();
            reader.read_line(&mut data_line).map_err(io_err)?;
            let size = parse_data_line(data_line.trim_end_matches('\n'))
                .ok_or_else(|| mig_err("expected data line after inline modify"))?;
            let data = read_exact_bytes(reader, size)?;
            consume_optional_lf(reader)?;

            return Ok(FileOp {
                kind: FileOpKind::ModifyInline { mode, path, data },
            });
        }

        Ok(FileOp {
            kind: FileOpKind::Modify {
                mode,
                dataref: dataref.to_owned(),
                path,
            },
        })
    } else if let Some(rest) = line.strip_prefix("D ") {
        Ok(FileOp {
            kind: FileOpKind::Delete {
                path: unquote_path(rest),
            },
        })
    } else {
        Ok(FileOp {
            kind: FileOpKind::Passthrough {
                raw: line.to_owned(),
            },
        })
    }
}

fn parse_passthrough(first_line: &str, reader: &mut dyn BufRead) -> Result<PassthroughSection> {
    let mut lines = vec![first_line.to_owned()];

    if first_line.starts_with("tag ") {
        // Tags have a body: from, tagger, data.
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(io_err)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches('\n');
            if trimmed.is_empty() {
                break;
            }
            if trimmed.starts_with("data ") {
                let size =
                    parse_data_line(trimmed).ok_or_else(|| mig_err("invalid data line in tag"))?;
                let msg_bytes = read_exact_bytes(reader, size)?;
                consume_optional_lf(reader)?;
                let msg = String::from_utf8_lossy(&msg_bytes).to_string();
                lines.push(format!("data {size}"));
                lines.push(msg);
            } else {
                lines.push(trimmed.to_owned());
            }
        }
    }

    Ok(PassthroughSection { lines })
}

// ---------------------------------------------------------------------------
// Stream writer
// ---------------------------------------------------------------------------

/// Write the transformed stream to fast-import.
fn write_stream(writer: &mut dyn Write, stream: &ExportStream) -> Result<()> {
    for section_ref in &stream.order {
        match section_ref {
            SectionRef::Blob(idx) => {
                let blob = &stream.blobs[*idx];
                write!(
                    writer,
                    "blob\nmark :{}\ndata {}\n",
                    blob.mark,
                    blob.data.len()
                )
                .map_err(io_err)?;
                writer.write_all(&blob.data).map_err(io_err)?;
                writer.write_all(b"\n").map_err(io_err)?;
            }
            SectionRef::Commit(idx) => {
                let commit = &stream.commits[*idx];
                for line in &commit.header_lines {
                    writeln!(writer, "{line}").map_err(io_err)?;
                }
                for op in &commit.file_ops {
                    write_file_op(writer, op)?;
                }
                writeln!(writer).map_err(io_err)?;
            }
            SectionRef::Passthrough(idx) => {
                let section = &stream.passthroughs[*idx];
                for line in &section.lines {
                    writeln!(writer, "{line}").map_err(io_err)?;
                }
                writeln!(writer).map_err(io_err)?;
            }
        }
    }

    writeln!(writer, "done").map_err(io_err)?;
    writer.flush().map_err(io_err)?;
    Ok(())
}

fn write_file_op(writer: &mut dyn Write, op: &FileOp) -> Result<()> {
    match &op.kind {
        FileOpKind::Modify {
            mode,
            dataref,
            path,
        } => {
            writeln!(writer, "M {mode} {dataref} {}", quote_path(path)).map_err(io_err)?;
        }
        FileOpKind::ModifyInline { mode, path, data } => {
            writeln!(writer, "M {mode} inline {}", quote_path(path)).map_err(io_err)?;
            writeln!(writer, "data {}", data.len()).map_err(io_err)?;
            writer.write_all(data).map_err(io_err)?;
            writer.write_all(b"\n").map_err(io_err)?;
        }
        FileOpKind::Delete { path } => {
            writeln!(writer, "D {}", quote_path(path)).map_err(io_err)?;
        }
        FileOpKind::Passthrough { raw } => {
            writeln!(writer, "{raw}").map_err(io_err)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fast-export / fast-import pipeline helpers
// ---------------------------------------------------------------------------

fn run_fast_export(refs_arg: &[String]) -> Result<Vec<u8>> {
    let mut args = vec!["fast-export".to_owned(), "--show-original-ids".to_owned()];
    args.extend(refs_arg.iter().cloned());

    let output = Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| mig_err(format!("failed to run git fast-export: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git fast-export failed: {stderr}")));
    }

    Ok(output.stdout)
}

fn run_fast_import(stream_data: &[u8], export_marks: bool) -> Result<Option<HashMap<u64, String>>> {
    let marks_file = if export_marks {
        Some(tempfile::NamedTempFile::new().map_err(|e| {
            mig_err(format!(
                "failed to create temporary fast-import marks file: {e}"
            ))
        })?)
    } else {
        None
    };

    let mut command = Command::new("git");
    command.args(["fast-import", "--force", "--quiet", "--done"]);
    if let Some(file) = &marks_file {
        command.arg(format!("--export-marks={}", file.path().display()));
    }

    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| mig_err(format!("failed to start git fast-import: {e}")))?;

    {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| mig_err("failed to capture fast-import stdin"))?;
        let mut writer = std::io::BufWriter::new(stdin);
        writer.write_all(stream_data).map_err(io_err)?;
        writer.flush().map_err(io_err)?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| mig_err(format!("fast-import failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git fast-import failed: {stderr}")));
    }

    match marks_file {
        Some(file) => Ok(Some(read_fast_import_marks(file.path())?)),
        None => Ok(None),
    }
}

fn read_fast_import_marks(path: &Path) -> Result<HashMap<u64, String>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| mig_err(format!("failed to read fast-import marks file: {e}")))?;
    let mut marks = HashMap::new();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(mark) = parts.next() else {
            continue;
        };
        let Some(oid) = parts.next() else {
            continue;
        };
        let Some(mark) = mark.strip_prefix(':') else {
            continue;
        };
        let Ok(mark) = mark.parse::<u64>() else {
            continue;
        };
        marks.insert(mark, oid.to_owned());
    }
    Ok(marks)
}

fn write_object_map(
    path: &str,
    stream: &ExportStream,
    marks: Option<&HashMap<u64, String>>,
) -> Result<()> {
    let marks = marks.ok_or_else(|| mig_err("fast-import did not produce marks for object map"))?;
    let mut rows = Vec::new();

    for commit in &stream.commits {
        let (Some(original_oid), Some(mark)) = (&commit.original_oid, commit.mark) else {
            continue;
        };
        let new_oid = marks.get(&mark).ok_or_else(|| {
            mig_err(format!(
                "fast-import did not report rewritten commit for mark :{mark}"
            ))
        })?;
        rows.push(format!("{original_oid},{new_oid}"));
    }

    let mut content = rows.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    std::fs::write(path, content)
        .map_err(|e| mig_err(format!("failed to write object map {path}: {e}")))?;
    Ok(())
}

fn commit_verbose_label(commit: &ExportedCommit) -> String {
    commit
        .original_oid
        .clone()
        .or_else(|| commit.mark.map(|mark| format!(":{mark}")))
        .unwrap_or_else(|| "<unknown>".to_owned())
}

fn print_verbose_migrations(entries: &[VerboseMigrationEntry]) {
    for entry in entries {
        println!("{} {}", entry.commit, entry.path);
    }
}

fn find_max_mark(stream: &ExportStream) -> u64 {
    let mut max = 0u64;
    for blob in &stream.blobs {
        max = max.max(blob.mark);
    }
    for commit in &stream.commits {
        if let Some(m) = commit.mark {
            max = max.max(m);
        }
    }
    max
}

// ---------------------------------------------------------------------------
// migrate import (full history rewrite)
// ---------------------------------------------------------------------------

/// Convert matching files across all history to LFS pointers.
///
/// Rewrites every commit using the `git fast-export` → transform →
/// `git fast-import` pipeline. For each blob whose path matches `include`:
/// computes SHA-256, creates an LFS pointer, uploads original content to
/// the remote store, and replaces the blob data. Updates `.gitattributes`
/// in every commit to include the LFS tracking pattern.
pub fn migrate_import(include: &str, everything: bool, from_crab: bool) -> Result<()> {
    migrate_import_with_store(include, everything, from_crab, None)
}

/// Convert matching files across history to LFS pointers with full options.
pub fn migrate_import_with_options(options: MigrateImportOptions<'_>) -> Result<()> {
    migrate_import_with_store_options(options, None)
}

/// Inner implementation that accepts an optional store for testing.
pub fn migrate_import_with_store(
    include: &str,
    everything: bool,
    _from_crab: bool,
    store: Option<Arc<LfsObjectStore>>,
) -> Result<()> {
    migrate_import_with_store_options(
        MigrateImportOptions {
            include: Some(include),
            exclude: None,
            above: None,
            fixup: false,
            no_rewrite: false,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection {
                everything,
                ..MigrateRefSelection::default()
            },
            yes: false,
            verbose: false,
            from_crab: _from_crab,
        },
        store,
    )
}

fn migrate_import_with_store_options(
    options: MigrateImportOptions<'_>,
    store: Option<Arc<LfsObjectStore>>,
) -> Result<()> {
    if options.no_rewrite {
        return migrate_import_no_rewrite(options, store);
    }

    require_clean_working_tree(options.yes)?;
    validate_import_filters(&options)?;

    let repo_root = std::env::current_dir()
        .map_err(|e| mig_err(format!("failed to get current directory: {e}")))?;

    let remote_store = match store {
        Some(s) => Some(s),
        None => crate::cmd::lfs::store_setup::resolve_lfs_remote_sync()
            .ok()
            .map(|ctx| ctx.store),
    };

    let matcher = PathMatcher::new(options.include, options.exclude);
    let threshold = parse_size_threshold(options.above)?;
    let fixup_matcher = if options.fixup {
        Some(FixupMatcher::open(&repo_root)?)
    } else {
        None
    };

    let refs = resolve_ref_selection(&options.refs)?;
    let original_refs = save_ref_state(&refs.state_scope)?;

    // Phase 1: Export.
    let raw_stream = run_fast_export(&refs.revision_args)?;
    let mut stream = parse_export_stream(&raw_stream)?;

    // Phase 2: Identify blobs to convert.
    // Build mark → blob index map.
    let mark_to_blob: HashMap<u64, usize> = stream
        .blobs
        .iter()
        .enumerate()
        .map(|(i, b)| (b.mark, i))
        .collect();

    // Scan commits to find which marks are referenced by matching paths.
    let mut marks_to_convert: HashSet<u64> = HashSet::new();
    let mut tracking_patterns: HashSet<String> = HashSet::new();
    let mut verbose_entries = Vec::new();
    for commit in &stream.commits {
        let commit_label = commit_verbose_label(commit);
        for op in &commit.file_ops {
            if let FileOpKind::Modify { dataref, path, .. } = &op.kind
                && let Some(mark_num) = dataref.strip_prefix(':')
                && let Ok(m) = mark_num.parse::<u64>()
                && let Some(&blob_idx) = mark_to_blob.get(&m)
                && let Some(source_size) =
                    import_source_size(&stream.blobs[blob_idx].data, options.from_crab)
                && should_import_path(
                    path,
                    source_size,
                    &matcher,
                    threshold,
                    fixup_matcher.as_ref(),
                )
            {
                marks_to_convert.insert(m);
                if options.verbose {
                    verbose_entries.push(VerboseMigrationEntry {
                        commit: commit_label.clone(),
                        path: path.clone(),
                    });
                }
                if !options.fixup {
                    add_tracking_pattern(&mut tracking_patterns, options.include, path);
                }
            }
        }
    }

    // Check for inline matches too.
    let mut has_inline = false;
    for commit in &stream.commits {
        let commit_label = commit_verbose_label(commit);
        for op in &commit.file_ops {
            if let FileOpKind::ModifyInline { path, data, .. } = &op.kind
                && import_source_size(data, options.from_crab).is_some_and(|source_size| {
                    should_import_path(
                        path,
                        source_size,
                        &matcher,
                        threshold,
                        fixup_matcher.as_ref(),
                    )
                })
            {
                has_inline = true;
                if options.verbose {
                    verbose_entries.push(VerboseMigrationEntry {
                        commit: commit_label.clone(),
                        path: path.clone(),
                    });
                }
            }
        }
    }

    if marks_to_convert.is_empty() && !has_inline {
        eprintln!(
            "migrate import: no files matched {}",
            import_filter_label(&options)
        );
        return Ok(());
    }

    let crab_hydrator = if options.from_crab {
        Some(resolve_crab_hydrator("migrate-import-from-crab")?)
    } else {
        None
    };

    // Phase 3: Replace blob content with LFS pointers, upload originals.
    let mut converted_count = 0u64;

    for blob in &mut stream.blobs {
        if !marks_to_convert.contains(&blob.mark) {
            continue;
        }

        let content = if let Some(hydrator) = crab_hydrator.as_ref() {
            resolve_crab_content(&blob.data, hydrator)?
        } else {
            blob.data.clone()
        };
        blob.data = lfs_pointer_for_content(
            &content,
            remote_store.as_ref(),
            &format!("blob (mark :{})", blob.mark),
        )?;
        converted_count += 1;
    }

    // Handle inline data in commits.
    for commit in &mut stream.commits {
        for op in &mut commit.file_ops {
            if let FileOpKind::ModifyInline { path, data, .. } = &mut op.kind
                && let Some(source_size) = import_source_size(data, options.from_crab)
                && should_import_path(
                    path,
                    source_size,
                    &matcher,
                    threshold,
                    fixup_matcher.as_ref(),
                )
            {
                let content = if let Some(hydrator) = crab_hydrator.as_ref() {
                    resolve_crab_content(data, hydrator)?
                } else {
                    data.clone()
                };
                *data = lfs_pointer_for_content(
                    &content,
                    remote_store.as_ref(),
                    &format!("inline blob for {path}"),
                )?;
                if !options.fixup {
                    add_tracking_pattern(&mut tracking_patterns, options.include, path);
                }
                converted_count += 1;
            }
        }
    }

    // Phase 4: Inject .gitattributes into each commit.
    let mut next_mark = find_max_mark(&stream) + 1;
    let tracking_lines = tracking_lines_for_patterns(&tracking_patterns);
    inject_gitattributes_for_import(&mut stream, &tracking_lines, &mark_to_blob, &mut next_mark);

    // Phase 5: Serialize and run fast-import.
    let mut output_buf = Vec::new();
    write_stream(&mut output_buf, &stream)?;

    let import_marks = match run_fast_import(&output_buf, options.object_map.is_some()) {
        Ok(marks) => marks,
        Err(e) => {
            restore_ref_state(&original_refs);
            return Err(e);
        }
    };

    if let Some(path) = options.object_map {
        write_object_map(path, &stream, import_marks.as_ref())?;
    }

    // Phase 6: Update .gitattributes in the working tree.
    let mut sorted_patterns: Vec<&String> = tracking_patterns.iter().collect();
    sorted_patterns.sort();
    for pattern in sorted_patterns {
        crate::lfs::track::track(pattern, &repo_root)?;
    }

    // Phase 7: Reset working tree to match the rewritten HEAD.
    let _ = Command::new("git")
        .args(["checkout", "--force", "HEAD"])
        .output();

    if options.verbose {
        print_verbose_migrations(&verbose_entries);
    }

    eprintln!("migrate import: history rewritten successfully");
    eprintln!(
        "  converted {converted_count} blob(s) matching {}",
        import_filter_label(&options)
    );
    if let Some(exclude) = options.exclude {
        eprintln!("  excluded paths matching \"{exclude}\"");
    }
    eprintln!("  scope: {}", refs.scope_label);

    Ok(())
}

fn migrate_import_no_rewrite(
    options: MigrateImportOptions<'_>,
    store: Option<Arc<LfsObjectStore>>,
) -> Result<()> {
    validate_import_no_rewrite_options(&options)?;
    require_clean_working_tree(options.yes)?;

    let repo_root = std::env::current_dir()
        .map_err(|e| mig_err(format!("failed to get current directory: {e}")))?;
    let remote_store = match store {
        Some(s) => Some(s),
        None => crate::cmd::lfs::store_setup::resolve_lfs_remote_sync()
            .ok()
            .map(|ctx| ctx.store),
    };
    let fixup = FixupMatcher::open(&repo_root)?;

    let mut converted = Vec::new();
    for input in &options.no_rewrite_files {
        let rel_path = normalize_no_rewrite_path(input)?;
        if !fixup.matches(&rel_path) {
            return Err(mig_err(format!(
                "file \"{rel_path}\" is not tracked by .gitattributes filter=lfs"
            )));
        }

        let abs_path = repo_root.join(&rel_path);
        let data = std::fs::read(&abs_path)
            .map_err(|e| mig_err(format!("failed to read {rel_path}: {e}")))?;
        if is_lfs_pointer(&data) {
            continue;
        }

        let oid: [u8; 32] = Sha256::digest(&data).into();
        let pointer = LfsPointer {
            oid,
            size: data.len() as u64,
            extensions: Vec::new(),
        };

        if let Some(ref store) = remote_store {
            let content_bytes = bytes::Bytes::from(data.clone());
            let store_clone = Arc::clone(store);
            crate::cmd::lfs::block_on_runtime(async move {
                store_clone
                    .put(&oid, content_bytes)
                    .await
                    .map_err(CrabError::from)
            })
            .map_err(|e| mig_err(format!("failed to upload {rel_path}: {e}")))?;
        }

        cache_lfs_object(&oid, data.len() as u64, &data)?;
        std::fs::write(&abs_path, pointer.serialize())
            .map_err(|e| mig_err(format!("failed to write pointer for {rel_path}: {e}")))?;
        converted.push(rel_path);
    }

    if converted.is_empty() {
        eprintln!("migrate import --no-rewrite: no files required migration");
        return Ok(());
    }

    stage_paths(&converted)?;
    commit_no_rewrite(&converted, options.message)?;

    if options.verbose {
        let commit = current_head_label()?;
        let entries: Vec<VerboseMigrationEntry> = converted
            .iter()
            .map(|path| VerboseMigrationEntry {
                commit: commit.clone(),
                path: path.clone(),
            })
            .collect();
        print_verbose_migrations(&entries);
    }

    eprintln!(
        "migrate import --no-rewrite: committed {} file(s)",
        converted.len()
    );
    Ok(())
}

fn current_head_label() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| mig_err(format!("failed to resolve HEAD: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git rev-parse HEAD failed: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn validate_import_filters(options: &MigrateImportOptions<'_>) -> Result<()> {
    if options.fixup
        && (options.include.is_some() || options.exclude.is_some() || options.above.is_some())
    {
        return Err(mig_err(
            "migrate import --fixup cannot be combined with --include, --exclude, or --above",
        ));
    }

    if options.above.is_none() {
        return Ok(());
    }

    if options.include.is_some() || options.exclude.is_some() {
        return Err(mig_err(
            "migrate import --above cannot be combined with --include or --exclude",
        ));
    }

    Ok(())
}

fn validate_import_no_rewrite_options(options: &MigrateImportOptions<'_>) -> Result<()> {
    if !options.no_rewrite {
        return Ok(());
    }

    if options.fixup {
        return Err(mig_err(
            "migrate import --no-rewrite cannot be combined with --fixup",
        ));
    }

    if options.no_rewrite_files.is_empty() {
        return Err(mig_err(
            "migrate import --no-rewrite requires at least one file",
        ));
    }

    Ok(())
}

fn import_filter_label(options: &MigrateImportOptions<'_>) -> String {
    if options.fixup {
        return "files tracked by .gitattributes filter=lfs".to_owned();
    }

    let mut parts = Vec::new();
    if let Some(include) = options.include {
        parts.push(format!("pattern \"{include}\""));
    } else {
        parts.push("all paths".to_owned());
    }
    if let Some(exclude) = options.exclude {
        parts.push(format!("excluding \"{exclude}\""));
    }
    if let Some(above) = options.above {
        parts.push(format!("at least {above}"));
    }
    parts.join(", ")
}

fn normalize_no_rewrite_path(input: &str) -> Result<String> {
    let path = Path::new(input);
    if path.is_absolute() {
        return Err(mig_err(format!(
            "migrate import --no-rewrite expects repo-relative paths, got \"{input}\""
        )));
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(mig_err(format!(
                    "migrate import --no-rewrite path escapes the repository: \"{input}\""
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(mig_err(format!(
                    "migrate import --no-rewrite expects repo-relative paths, got \"{input}\""
                )));
            }
        }
    }

    if parts.is_empty() {
        return Err(mig_err(
            "migrate import --no-rewrite cannot migrate the repository root",
        ));
    }

    Ok(parts.join("/"))
}

fn stage_paths(paths: &[String]) -> Result<()> {
    let output = Command::new("git")
        .arg("add")
        .arg("--")
        .args(paths)
        .output()
        .map_err(|e| mig_err(format!("failed to stage migrated files: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git add failed: {stderr}")));
    }

    Ok(())
}

fn commit_no_rewrite(paths: &[String], message: Option<&str>) -> Result<()> {
    let message = message
        .map(str::to_owned)
        .unwrap_or_else(|| default_no_rewrite_message(paths));
    let output = Command::new("git")
        .args(["commit", "-m", &message])
        .output()
        .map_err(|e| mig_err(format!("failed to commit migrated files: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(mig_err(format!("git commit failed: {stderr}")));
    }

    Ok(())
}

fn default_no_rewrite_message(paths: &[String]) -> String {
    if paths.len() == 1 {
        format!("Migrate {} to Git LFS", paths[0])
    } else {
        format!("Migrate {} files to Git LFS", paths.len())
    }
}

fn should_import_path(
    path: &str,
    size: u64,
    matcher: &PathMatcher,
    threshold: u64,
    fixup_matcher: Option<&FixupMatcher>,
) -> bool {
    if let Some(fixup) = fixup_matcher {
        return fixup.matches(path);
    }

    matcher.matches(path) && size >= threshold
}

fn is_gitattributes_path(path: &str) -> bool {
    path == ".gitattributes" || path.ends_with("/.gitattributes")
}

struct FixupMatcher {
    matchers: Vec<Box<dyn Fn(&str) -> bool>>,
}

impl FixupMatcher {
    fn open(repo_root: &std::path::Path) -> Result<Self> {
        let patterns = crate::lfs::track::list(repo_root)?;
        Ok(Self::from_patterns(patterns))
    }

    fn from_patterns(patterns: Vec<String>) -> Self {
        Self {
            matchers: patterns
                .into_iter()
                .map(|pattern| glob_matches_factory(&pattern))
                .collect(),
        }
    }

    fn matches(&self, path: &str) -> bool {
        !is_gitattributes_path(path) && self.matchers.iter().any(|matcher| matcher(path))
    }
}

fn add_tracking_pattern(patterns: &mut HashSet<String>, include: Option<&str>, path: &str) {
    if let Some(include) = include {
        patterns.insert(include.to_owned());
    } else {
        patterns.insert(tracking_pattern_for_path(path));
    }
}

fn tracking_pattern_for_path(path: &str) -> String {
    match file_extension(path).as_str() {
        "(no extension)" => path.to_owned(),
        pattern => pattern.to_owned(),
    }
}

fn tracking_lines_for_patterns(patterns: &HashSet<String>) -> Vec<String> {
    let mut patterns: Vec<&String> = patterns.iter().collect();
    patterns.sort();
    patterns
        .into_iter()
        .map(|pattern| format!("{pattern} filter=lfs diff=lfs merge=lfs -text"))
        .collect()
}

fn crab_tracking_lines_for_patterns(patterns: &HashSet<String>) -> Vec<String> {
    let mut patterns: Vec<&String> = patterns.iter().collect();
    patterns.sort();
    patterns
        .into_iter()
        .map(|pattern| format!("{pattern} filter=crab diff=crab merge=crab -text"))
        .collect()
}

fn untrack_lines_for_patterns(patterns: &HashSet<String>) -> Vec<String> {
    let mut patterns: Vec<&String> = patterns.iter().collect();
    patterns.sort();
    patterns
        .into_iter()
        .map(|pattern| format!("{pattern} !text !filter !merge !diff"))
        .collect()
}

fn update_gitattributes_for_export(
    stream: &mut ExportStream,
    include: &str,
    lfs_attrs_line: &str,
    mark_to_blob: &HashMap<u64, usize>,
    to_crab: bool,
) {
    let mut patterns = HashSet::new();
    patterns.insert(include.to_owned());
    let mut next_mark = find_max_mark(stream) + 1;

    if to_crab {
        remove_gitattributes_for_export(stream, lfs_attrs_line, mark_to_blob);
        let tracking_lines = crab_tracking_lines_for_patterns(&patterns);
        inject_gitattributes_for_import(stream, &tracking_lines, mark_to_blob, &mut next_mark);
    } else {
        let untrack_lines = untrack_lines_for_patterns(&patterns);
        inject_gitattributes_for_import(stream, &untrack_lines, mark_to_blob, &mut next_mark);
    }
}

/// Inject `.gitattributes` modifications into each commit.
///
/// For each commit, ensures `.gitattributes` contains the requested lines.
/// Creates new blob sections and adds `M 100644 :<mark> .gitattributes`
/// file ops to commits that need updating.
fn inject_gitattributes_for_import(
    stream: &mut ExportStream,
    lfs_attrs_lines: &[String],
    mark_to_blob: &HashMap<u64, usize>,
    next_mark: &mut u64,
) {
    if lfs_attrs_lines.is_empty() {
        return;
    }

    // Track the current .gitattributes content as we walk commits.
    let mut current_attrs: Option<Vec<u8>> = None;

    // We need to insert new blobs. Collect them, then insert into the
    // stream at the right positions.
    // Strategy: for each commit that needs a .gitattributes update,
    // add an inline M-line with the new content. This avoids needing
    // to insert blob sections at specific positions.

    for commit in &mut stream.commits {
        // Check if this commit has a deleteall (resets the tree).
        let has_deleteall = commit
            .file_ops
            .iter()
            .any(|op| matches!(&op.kind, FileOpKind::Passthrough { raw } if raw == "deleteall"));
        if has_deleteall {
            current_attrs = None;
        }

        // Check if this commit modifies .gitattributes.
        let mut found_attrs = false;
        for op in &commit.file_ops {
            match &op.kind {
                FileOpKind::Modify { dataref, path, .. } if path == ".gitattributes" => {
                    if let Some(mark_str) = dataref.strip_prefix(':')
                        && let Ok(m) = mark_str.parse::<u64>()
                        && let Some(&blob_idx) = mark_to_blob.get(&m)
                    {
                        current_attrs = Some(stream.blobs[blob_idx].data.clone());
                    }
                    found_attrs = true;
                }
                FileOpKind::ModifyInline { path, data, .. } if path == ".gitattributes" => {
                    current_attrs = Some(data.clone());
                    found_attrs = true;
                }
                FileOpKind::Delete { path } if path == ".gitattributes" => {
                    current_attrs = None;
                    found_attrs = true;
                }
                _ => {}
            }
        }

        // Build the desired .gitattributes content.
        let attrs_text = match &current_attrs {
            Some(bytes) => String::from_utf8_lossy(bytes).to_string(),
            None => String::new(),
        };

        let missing_lines: Vec<&String> = lfs_attrs_lines
            .iter()
            .filter(|line| !attrs_text.lines().any(|existing| existing == line.as_str()))
            .collect();
        if missing_lines.is_empty() {
            // Already has the line — nothing to do.
            continue;
        }

        let mut new_attrs = attrs_text;
        if !new_attrs.is_empty() && !new_attrs.ends_with('\n') {
            new_attrs.push('\n');
        }
        for line in missing_lines {
            new_attrs.push_str(line);
            new_attrs.push('\n');
        }

        let new_data = new_attrs.into_bytes();
        current_attrs = Some(new_data.clone());

        // If the commit already has a .gitattributes M-line, replace its
        // content. Otherwise, add a new inline M-line.
        if found_attrs {
            // Replace the existing .gitattributes op with inline data.
            for op in &mut commit.file_ops {
                let is_attrs = match &op.kind {
                    FileOpKind::Modify { path, .. } | FileOpKind::ModifyInline { path, .. } => {
                        path == ".gitattributes"
                    }
                    _ => false,
                };
                if is_attrs {
                    op.kind = FileOpKind::ModifyInline {
                        mode: "100644".to_owned(),
                        path: ".gitattributes".to_owned(),
                        data: new_data.clone(),
                    };
                    break;
                }
            }
        } else {
            // Add a new inline M-line for .gitattributes.
            commit.file_ops.push(FileOp {
                kind: FileOpKind::ModifyInline {
                    mode: "100644".to_owned(),
                    path: ".gitattributes".to_owned(),
                    data: new_data,
                },
            });
        }
    }

    // next_mark is unused now since we use inline data, but keep the
    // parameter for API compatibility.
    let _ = next_mark;
}

// ---------------------------------------------------------------------------
// migrate export (full history rewrite)
// ---------------------------------------------------------------------------

/// Convert LFS pointers matching `include` back to regular files across
/// all history.
///
/// Rewrites every commit using the fast-export/fast-import pipeline.
/// For each blob that is an LFS pointer whose path matches the pattern,
/// downloads the original content and replaces the pointer. Removes the
/// LFS tracking line from `.gitattributes` in each commit.
pub fn migrate_export(include: &str, _to_crab: bool) -> Result<()> {
    migrate_export_with_options(MigrateExportOptions {
        include,
        exclude: None,
        object_map: None,
        remote: None,
        refs: MigrateRefSelection {
            everything: true,
            ..MigrateRefSelection::default()
        },
        yes: false,
        verbose: false,
        to_crab: _to_crab,
    })
}

/// Convert LFS pointers back to regular files with full options.
pub fn migrate_export_with_options(options: MigrateExportOptions<'_>) -> Result<()> {
    require_clean_working_tree(options.yes)?;

    let repo_root = std::env::current_dir()
        .map_err(|e| mig_err(format!("failed to get current directory: {e}")))?;

    let matcher = PathMatcher::new(Some(options.include), options.exclude);
    let lfs_attrs_line = format!(
        "{include} filter=lfs diff=lfs merge=lfs -text",
        include = options.include
    );

    let remote_ctx = if options.remote.is_some() {
        Some(
            crate::cmd::lfs::store_setup::resolve_lfs_remote_for_operation_with_remote_sync(
                "migrate-export",
                options.remote,
            )?,
        )
    } else {
        crate::cmd::lfs::store_setup::resolve_lfs_remote_for_operation_sync("migrate-export").ok()
    };

    let refs = resolve_ref_selection(&options.refs)?;
    let original_refs = save_ref_state(&refs.state_scope)?;

    // Phase 1: Export.
    let raw_stream = run_fast_export(&refs.revision_args)?;
    let mut stream = parse_export_stream(&raw_stream)?;

    // Phase 2: Identify blobs to convert (LFS pointers matching the pattern).
    let mark_to_blob: HashMap<u64, usize> = stream
        .blobs
        .iter()
        .enumerate()
        .map(|(i, b)| (b.mark, i))
        .collect();

    let mut marks_to_convert: HashSet<u64> = HashSet::new();
    let mut mark_to_path: HashMap<u64, String> = HashMap::new();
    let mut verbose_entries = Vec::new();
    for commit in &stream.commits {
        let commit_label = commit_verbose_label(commit);
        for op in &commit.file_ops {
            if let FileOpKind::Modify { dataref, path, .. } = &op.kind
                && matcher.matches(path)
                && let Some(mark_str) = dataref.strip_prefix(':')
                && let Ok(m) = mark_str.parse::<u64>()
                && let Some(&blob_idx) = mark_to_blob.get(&m)
                && is_lfs_pointer(&stream.blobs[blob_idx].data)
            {
                marks_to_convert.insert(m);
                mark_to_path.entry(m).or_insert_with(|| path.clone());
                if options.verbose {
                    verbose_entries.push(VerboseMigrationEntry {
                        commit: commit_label.clone(),
                        path: path.clone(),
                    });
                }
            }
        }
    }

    let mut has_inline = false;
    for commit in &stream.commits {
        let commit_label = commit_verbose_label(commit);
        for op in &commit.file_ops {
            if let FileOpKind::ModifyInline { path, data, .. } = &op.kind
                && matcher.matches(path)
                && is_lfs_pointer(data)
            {
                has_inline = true;
                if options.verbose {
                    verbose_entries.push(VerboseMigrationEntry {
                        commit: commit_label.clone(),
                        path: path.clone(),
                    });
                }
            }
        }
    }

    if marks_to_convert.is_empty() && !has_inline {
        eprintln!(
            "migrate export: no LFS pointers matched pattern \"{}\"",
            options.include
        );
        return Ok(());
    }

    let crab_staging = if options.to_crab {
        Some(open_migrate_staging()?)
    } else {
        None
    };

    // Phase 3: Replace LFS pointers with original content.
    let mut converted_count = 0u64;

    for blob in &mut stream.blobs {
        if !marks_to_convert.contains(&blob.mark) {
            continue;
        }

        let content = resolve_lfs_content(&blob.data, remote_ctx.as_ref())?;
        blob.data = if let Some(staging) = crab_staging.as_ref() {
            let path = mark_to_path
                .get(&blob.mark)
                .map(String::as_str)
                .unwrap_or("migrate-history-blob");
            crab_pointer_for_content(staging, path, content)?
        } else {
            content
        };
        converted_count += 1;
    }

    // Handle inline LFS pointers.
    for commit in &mut stream.commits {
        for op in &mut commit.file_ops {
            if let FileOpKind::ModifyInline { path, data, .. } = &mut op.kind
                && matcher.matches(path)
                && is_lfs_pointer(data)
            {
                let content = resolve_lfs_content(data, remote_ctx.as_ref())?;
                *data = if let Some(staging) = crab_staging.as_ref() {
                    crab_pointer_for_content(staging, path, content)?
                } else {
                    content
                };
                converted_count += 1;
            }
        }
    }

    // Phase 4: Update .gitattributes in each commit.
    update_gitattributes_for_export(
        &mut stream,
        options.include,
        &lfs_attrs_line,
        &mark_to_blob,
        options.to_crab,
    );

    // Phase 5: Serialize and run fast-import.
    let mut output_buf = Vec::new();
    write_stream(&mut output_buf, &stream)?;

    let import_marks = match run_fast_import(&output_buf, options.object_map.is_some()) {
        Ok(marks) => marks,
        Err(e) => {
            restore_ref_state(&original_refs);
            return Err(e);
        }
    };

    if let Some(path) = options.object_map {
        write_object_map(path, &stream, import_marks.as_ref())?;
    }

    // Phase 6: Update working tree .gitattributes.
    if options.to_crab {
        crate::lfs::track::untrack(options.include, &repo_root)?;
        crate::cmd::track::run_track_in(options.include, &repo_root)?;
    } else {
        crate::lfs::track::append_untrack_override(options.include, &repo_root)?;
    }

    // Phase 7: Reset working tree.
    let _ = Command::new("git")
        .args(["checkout", "--force", "HEAD"])
        .output();

    if options.verbose {
        print_verbose_migrations(&verbose_entries);
    }

    eprintln!("migrate export: history rewritten successfully");
    eprintln!(
        "  converted {converted_count} LFS pointer(s) matching \"{}\"{}",
        options.include,
        if options.to_crab {
            " to Crab pointers"
        } else {
            ""
        }
    );
    if let Some(exclude) = options.exclude {
        eprintln!("  excluded paths matching \"{exclude}\"");
    }
    if let Some(remote) = options.remote {
        eprintln!("  remote: {remote}");
    }
    eprintln!("  scope: {}", refs.scope_label);

    Ok(())
}

/// Resolve the original content for an LFS pointer blob.
fn resolve_lfs_content(
    pointer_bytes: &[u8],
    remote_ctx: Option<&crate::cmd::lfs::store_setup::LfsRemoteContext>,
) -> Result<Vec<u8>> {
    let crab_git::PointerKind::Lfs(pointer) = crab_git::classify(pointer_bytes) else {
        return Err(mig_err(
            "blob classified as LFS pointer but failed to parse",
        ));
    };

    let oid_hex = hex_encode(&pointer.oid);

    // Try local cache first.
    if let Some(local) = try_local_lfs_cache(&pointer)? {
        return Ok(local);
    }

    // Download from remote.
    if let Some(ctx) = remote_ctx {
        let store = Arc::clone(&ctx.store);
        let oid = pointer.oid;
        let content = crate::cmd::lfs::block_on_runtime(async move {
            store.verify(&oid).await.map_err(CrabError::from)
        })
        .map(|b| b.to_vec())
        .map_err(|e| mig_err(format!("failed to download LFS object {oid_hex}: {e}")))?;
        cache_lfs_object(&pointer.oid, pointer.size, &content)?;
        return Ok(content);
    }

    Err(mig_err(format!(
        "cannot resolve LFS object {oid_hex}: no local cache and no remote configured"
    )))
}

/// Remove the LFS tracking line from `.gitattributes` in each commit.
fn remove_gitattributes_for_export(
    stream: &mut ExportStream,
    lfs_attrs_line: &str,
    mark_to_blob: &HashMap<u64, usize>,
) {
    let mut current_attrs: Option<Vec<u8>> = None;

    for commit in &mut stream.commits {
        let has_deleteall = commit
            .file_ops
            .iter()
            .any(|op| matches!(&op.kind, FileOpKind::Passthrough { raw } if raw == "deleteall"));
        if has_deleteall {
            current_attrs = None;
        }

        // Check if this commit modifies .gitattributes.
        let mut found_attrs = false;
        for op in &commit.file_ops {
            match &op.kind {
                FileOpKind::Modify { dataref, path, .. } if path == ".gitattributes" => {
                    if let Some(mark_str) = dataref.strip_prefix(':')
                        && let Ok(m) = mark_str.parse::<u64>()
                        && let Some(&blob_idx) = mark_to_blob.get(&m)
                    {
                        current_attrs = Some(stream.blobs[blob_idx].data.clone());
                    }
                    found_attrs = true;
                }
                FileOpKind::ModifyInline { path, data, .. } if path == ".gitattributes" => {
                    current_attrs = Some(data.clone());
                    found_attrs = true;
                }
                FileOpKind::Delete { path } if path == ".gitattributes" => {
                    current_attrs = None;
                }
                _ => {}
            }
        }

        // If current attrs contain the LFS line, remove it.
        let attrs_text = match &current_attrs {
            Some(bytes) => String::from_utf8_lossy(bytes).to_string(),
            None => continue,
        };

        if !attrs_text.contains(lfs_attrs_line) {
            continue;
        }

        // Remove the LFS tracking line.
        let mut new_attrs = String::new();
        for line in attrs_text
            .lines()
            .filter(|line| !line.contains(lfs_attrs_line))
        {
            let _ = writeln!(new_attrs, "{line}");
        }

        let new_data = new_attrs.into_bytes();
        current_attrs = Some(new_data.clone());

        if found_attrs {
            for op in &mut commit.file_ops {
                let is_attrs = match &op.kind {
                    FileOpKind::Modify { path, .. } | FileOpKind::ModifyInline { path, .. } => {
                        path == ".gitattributes"
                    }
                    _ => false,
                };
                if is_attrs {
                    op.kind = FileOpKind::ModifyInline {
                        mode: "100644".to_owned(),
                        path: ".gitattributes".to_owned(),
                        data: new_data.clone(),
                    };
                    break;
                }
            }
        } else {
            commit.file_ops.push(FileOp {
                kind: FileOpKind::ModifyInline {
                    mode: "100644".to_owned(),
                    path: ".gitattributes".to_owned(),
                    data: new_data,
                },
            });
        }
    }
}

// ---------------------------------------------------------------------------
// migrate info
// ---------------------------------------------------------------------------

/// Analyze the repository and display a summary of large files or LFS pointers.
pub fn migrate_info(above: Option<&str>, include: Option<&str>, pointers: bool) -> Result<()> {
    let pointer_mode = if pointers {
        MigrateInfoPointerMode::PointersOnly
    } else {
        MigrateInfoPointerMode::Follow
    };
    let refs = MigrateRefSelection::default();
    migrate_info_with_options(MigrateInfoOptions {
        above,
        include,
        top: None,
        unit: None,
        exclude: None,
        pointer_mode,
        fixup: false,
        refs: &refs,
    })
}

/// Analyze the repository with Git LFS-compatible output options.
pub fn migrate_info_with_options(options: MigrateInfoOptions<'_>) -> Result<()> {
    let unit = parse_size_unit(options.unit)?;
    validate_info_options(&options)?;

    if options.pointer_mode == MigrateInfoPointerMode::PointersOnly {
        return migrate_info_pointers(options.include, options.exclude, unit);
    }

    let threshold = parse_size_threshold(options.above)?;
    let mut entries = collect_large_files(
        threshold,
        options.include,
        options.exclude,
        options.pointer_mode,
        options.fixup,
        options.refs,
    )?;
    let lfs_objects = limit_info_entries(&mut entries, options.top);

    if entries.is_empty() && lfs_objects.is_none() {
        println!("migrate info: no files found matching criteria");
        return Ok(());
    }

    println!("{:<40} {:>10} {:>10}", "File type", "Count", "Total size");
    println!("{}", "-".repeat(62));

    for entry in &entries {
        println!(
            "{:<40} {:>10} {:>10}",
            entry.pattern,
            entry.file_count,
            format_size_with_unit(entry.total_size, unit),
        );
    }
    if let Some(entry) = &lfs_objects {
        println!(
            "{:<40} {:>10} {:>10}",
            entry.pattern,
            entry.file_count,
            format_size_with_unit(entry.total_size, unit),
        );
    }

    Ok(())
}

fn validate_info_options(options: &MigrateInfoOptions<'_>) -> Result<()> {
    if options.fixup && (options.include.is_some() || options.exclude.is_some()) {
        return Err(mig_err(
            "migrate info --fixup cannot be combined with --include or --exclude",
        ));
    }
    if options.fixup && options.pointer_mode != MigrateInfoPointerMode::Ignore {
        return Err(mig_err(
            "migrate info --fixup is only compatible with --pointers=ignore",
        ));
    }
    Ok(())
}

fn migrate_info_pointers(
    include: Option<&str>,
    exclude: Option<&str>,
    unit: Option<SizeUnit>,
) -> Result<()> {
    let entries = collect_pointer_info(include, exclude)?;

    if entries.is_empty() {
        println!("migrate info --pointers: no LFS pointers found in HEAD");
        return Ok(());
    }

    println!("{path:<50} {size:>12} OID", path = "Path", size = "Size");
    println!("{}", "-".repeat(80));

    for entry in &entries {
        let oid_short = if entry.oid.len() >= 10 {
            &entry.oid[..10]
        } else {
            &entry.oid
        };
        println!(
            "{:<50} {:>12} {}",
            entry.path,
            format_size_with_unit(entry.size, unit),
            oid_short,
        );
    }

    println!("\n{} LFS pointer(s) found", entries.len());
    Ok(())
}

fn collect_large_files(
    threshold: u64,
    include: Option<&str>,
    exclude: Option<&str>,
    pointer_mode: MigrateInfoPointerMode,
    fixup: bool,
    ref_selection: &MigrateRefSelection,
) -> Result<Vec<MigrateInfoEntry>> {
    let refs = resolve_ref_selection(ref_selection)?;
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    args.extend(refs.revision_args);

    let output =
        Command::new("git")
            .args(args)
            .output()
            .map_err(|e| CrabError::LfsMigrationFailed {
                reason: format!("failed to run git rev-list: {e}"),
            })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::LfsMigrationFailed {
            reason: format!("git rev-list failed: {stderr}"),
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);

    let mut oid_path_pairs: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((hash, path)) = line.split_once(' ') {
            oid_path_pairs.push((hash.to_owned(), path.to_owned()));
        }
    }

    if oid_path_pairs.is_empty() {
        return Ok(Vec::new());
    }

    let matcher = PathMatcher::new(include, exclude);
    let fixup_matcher = if fixup {
        Some(FixupMatcher::open(&std::env::current_dir()?)?)
    } else {
        None
    };
    oid_path_pairs.retain(|(_, path)| {
        fixup_matcher
            .as_ref()
            .map_or_else(|| matcher.matches(path), |fixup| fixup.matches(path))
    });

    if oid_path_pairs.is_empty() {
        return Ok(Vec::new());
    }

    let oids_input: String = oid_path_pairs
        .iter()
        .map(|(oid, _)| oid.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut child = Command::new("git")
        .args(["cat-file", "--batch-check"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::LfsMigrationFailed {
            reason: format!("failed to spawn git cat-file: {e}"),
        })?;

    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(oids_input.as_bytes());
    }
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::LfsMigrationFailed {
            reason: format!("git cat-file failed: {e}"),
        })?;

    let batch_text = String::from_utf8_lossy(&output.stdout);
    let batch_lines: Vec<&str> = batch_text.lines().collect();

    let mut by_ext: HashMap<String, (u64, u64)> = HashMap::new();
    let mut pointer_blobs: HashMap<String, Option<LfsPointer>> = HashMap::new();

    for (i, (oid, path)) in oid_path_pairs.iter().enumerate() {
        let Some(size) = batch_lines
            .get(i)
            .and_then(|line| parse_batch_check_size(line))
        else {
            continue;
        };

        let pointer = if pointer_mode != MigrateInfoPointerMode::NoFollow && size <= 1024 {
            if let Some(cached) = pointer_blobs.get(oid) {
                cached.clone()
            } else {
                let pointer = cat_file_blob(oid)
                    .ok()
                    .and_then(|blob| LfsPointer::parse(&blob).ok())
                    .filter(|pointer| pointer.size > 0);
                pointer_blobs.insert(oid.clone(), pointer.clone());
                pointer
            }
        } else {
            None
        };

        add_info_entry_for_blob(&mut by_ext, path, size, pointer, threshold, pointer_mode);
    }

    let mut entries: Vec<MigrateInfoEntry> = by_ext
        .into_iter()
        .map(|(pattern, (file_count, total_size))| MigrateInfoEntry {
            pattern,
            file_count,
            total_size,
        })
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.total_size));

    Ok(entries)
}

fn add_info_entry_for_blob(
    by_ext: &mut HashMap<String, (u64, u64)>,
    path: &str,
    blob_size: u64,
    pointer: Option<LfsPointer>,
    threshold: u64,
    pointer_mode: MigrateInfoPointerMode,
) {
    if let Some(pointer) = pointer
        && pointer_mode != MigrateInfoPointerMode::NoFollow
    {
        if pointer_mode == MigrateInfoPointerMode::Follow && pointer.size >= threshold {
            let entry = by_ext
                .entry(LFS_OBJECTS_PATTERN.to_owned())
                .or_insert((0, 0));
            entry.0 += 1;
            entry.1 += pointer.size;
        }
        return;
    }

    if blob_size < threshold {
        return;
    }

    let ext = file_extension(path);
    let entry = by_ext.entry(ext).or_insert((0, 0));
    entry.0 += 1;
    entry.1 += blob_size;
}

fn collect_pointer_info(
    include: Option<&str>,
    exclude: Option<&str>,
) -> Result<Vec<PointerInfoEntry>> {
    let tree_entries = ls_tree_head()?;
    let matcher = PathMatcher::new(include, exclude);

    let mut results = Vec::new();

    for (blob_hash, filename) in &tree_entries {
        if !matcher.matches(filename) {
            continue;
        }

        let blob = cat_file_blob(blob_hash)?;

        if blob.len() > 1024 {
            continue;
        }

        if let Ok(pointer) = LfsPointer::parse(&blob)
            && pointer.size > 0
        {
            results.push(PointerInfoEntry {
                path: filename.clone(),
                oid: hex_encode(&pointer.oid),
                size: pointer.size,
            });
        }
    }

    results.sort_by_key(|e| std::cmp::Reverse(e.size));

    Ok(results)
}

// ---------------------------------------------------------------------------
// Local cache helpers
// ---------------------------------------------------------------------------

fn cache_lfs_object(oid: &[u8; 32], size: u64, content: &[u8]) -> Result<()> {
    let repo_root = std::env::current_dir().map_err(CrabError::Io)?;
    let lfs_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(&repo_root)?;
    crate::lfs::cache::install_bytes(&lfs_dir, oid, size, content)?;
    Ok(())
}

fn try_local_lfs_cache(pointer: &LfsPointer) -> Result<Option<Vec<u8>>> {
    let repo_root = std::env::current_dir().map_err(CrabError::Io)?;
    let lfs_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(&repo_root)?;
    match crate::lfs::cache::read_pointer(&lfs_dir, pointer) {
        Err(CrabError::LfsObjectCorrupt { .. }) => Ok(None),
        result => result,
    }
}

// ---------------------------------------------------------------------------
// Git plumbing helpers
// ---------------------------------------------------------------------------

fn ls_tree_head() -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "HEAD"])
        .output()
        .map_err(|e| CrabError::LfsMigrationFailed {
            reason: format!("failed to run git ls-tree: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") {
            return Ok(Vec::new());
        }
        return Err(CrabError::LfsMigrationFailed {
            reason: format!("git ls-tree failed: {stderr}"),
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((meta, filename)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            continue;
        }
        results.push((parts[2].to_owned(), filename.to_owned()));
    }

    Ok(results)
}

fn cat_file_blob(blob_hash: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["cat-file", "-p", blob_hash])
        .output()
        .map_err(|e| CrabError::LfsMigrationFailed {
            reason: format!("failed to run git cat-file for {blob_hash}: {e}"),
        })?;

    if !output.status.success() {
        return Err(CrabError::LfsMigrationFailed {
            reason: format!("git cat-file failed for {blob_hash}"),
        });
    }

    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

fn parse_size_threshold(above: Option<&str>) -> Result<u64> {
    let s = match above {
        Some(s) => s.trim(),
        None => return Ok(0),
    };

    if s.is_empty() {
        return Ok(0);
    }

    let s_lower = s.to_lowercase();

    let (num_str, multiplier) = if let Some(n) = s_lower.strip_suffix("gb") {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s_lower.strip_suffix('g') {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s_lower.strip_suffix("mb") {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s_lower.strip_suffix('m') {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s_lower.strip_suffix("kb") {
        (n, 1024u64)
    } else if let Some(n) = s_lower.strip_suffix('k') {
        (n, 1024u64)
    } else if let Some(n) = s_lower.strip_suffix('b') {
        (n, 1u64)
    } else {
        (s_lower.as_str(), 1u64)
    };

    let num: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| CrabError::LfsMigrationFailed {
            reason: format!("invalid size threshold: \"{s}\""),
        })?;

    Ok(num * multiplier)
}

fn parse_batch_check_size(line: &str) -> Option<u64> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }
    if parts[1] != "blob" {
        return None;
    }
    parts[2].parse::<u64>().ok()
}

fn file_extension(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && !ext.contains('/') => format!("*.{ext}"),
        _ => "(no extension)".to_owned(),
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

fn parse_size_unit(unit: Option<&str>) -> Result<Option<SizeUnit>> {
    let Some(unit) = unit else {
        return Ok(None);
    };
    match unit.trim().to_lowercase().as_str() {
        "b" => Ok(Some(SizeUnit::B)),
        "kb" => Ok(Some(SizeUnit::Kb)),
        "mb" => Ok(Some(SizeUnit::Mb)),
        "gb" => Ok(Some(SizeUnit::Gb)),
        "tb" => Ok(Some(SizeUnit::Tb)),
        "pb" => Ok(Some(SizeUnit::Pb)),
        "kib" => Ok(Some(SizeUnit::KiB)),
        "mib" => Ok(Some(SizeUnit::MiB)),
        "gib" => Ok(Some(SizeUnit::GiB)),
        "tib" => Ok(Some(SizeUnit::TiB)),
        "pib" => Ok(Some(SizeUnit::PiB)),
        other => Err(CrabError::LfsMigrationFailed {
            reason: format!("invalid size unit: \"{other}\""),
        }),
    }
}

fn format_size_with_unit(bytes: u64, unit: Option<SizeUnit>) -> String {
    let Some(unit) = unit else {
        return format_size(bytes);
    };

    let (label, divisor) = match unit {
        SizeUnit::B => ("B", 1u64),
        SizeUnit::Kb => ("KB", 1000),
        SizeUnit::Mb => ("MB", 1000u64.pow(2)),
        SizeUnit::Gb => ("GB", 1000u64.pow(3)),
        SizeUnit::Tb => ("TB", 1000u64.pow(4)),
        SizeUnit::Pb => ("PB", 1000u64.pow(5)),
        SizeUnit::KiB => ("KiB", 1024),
        SizeUnit::MiB => ("MiB", 1024u64.pow(2)),
        SizeUnit::GiB => ("GiB", 1024u64.pow(3)),
        SizeUnit::TiB => ("TiB", 1024u64.pow(4)),
        SizeUnit::PiB => ("PiB", 1024u64.pow(5)),
    };

    if divisor == 1 {
        format!("{bytes} {label}")
    } else {
        format!("{:.1} {label}", bytes as f64 / divisor as f64)
    }
}

fn limit_info_entries(
    entries: &mut Vec<MigrateInfoEntry>,
    top: Option<usize>,
) -> Option<MigrateInfoEntry> {
    let lfs_objects = take_lfs_objects_entry(entries);
    entries.truncate(top.unwrap_or(DEFAULT_MIGRATE_INFO_TOP));
    lfs_objects
}

fn take_lfs_objects_entry(entries: &mut Vec<MigrateInfoEntry>) -> Option<MigrateInfoEntry> {
    let index = entries
        .iter()
        .position(|entry| entry.pattern == LFS_OBJECTS_PATTERN)?;
    Some(entries.remove(index))
}

struct PathMatcher {
    include: Option<Box<dyn Fn(&str) -> bool>>,
    exclude: Option<Box<dyn Fn(&str) -> bool>>,
}

impl PathMatcher {
    fn new(include: Option<&str>, exclude: Option<&str>) -> Self {
        Self {
            include: include.map(glob_matches_factory),
            exclude: exclude.map(glob_matches_factory),
        }
    }

    fn matches(&self, path: &str) -> bool {
        let included = self.include.as_ref().is_none_or(|matcher| matcher(path));
        let excluded = self.exclude.as_ref().is_some_and(|matcher| matcher(path));
        included && !excluded
    }
}

/// Build a path-matcher for `glob_matches_factory` semantics.
///
/// Under `gix-pathmatch`, this delegates to the consolidated
/// `gix_pathspec::Search` via [`core::pathmatch::build_filter`] so the
/// migrate command picks up the same glob semantics as `crab add` /
/// `crab hydrate`. Falls back to a hand-rolled suffix matcher when
/// the feature is disabled.
fn glob_matches_factory(pattern: &str) -> Box<dyn Fn(&str) -> bool> {
    #[cfg(feature = "gix-pathmatch")]
    {
        let pattern_owned = pattern.to_owned();
        // Build the filter eagerly. On parse failure fall through to the
        // legacy matcher so callers never observe a panic / error from
        // an exotic user-supplied glob. The warn! is observability-only.
        match crate::core::pathmatch::build_filter(&[pattern_owned.clone()], &[]) {
            Ok(filter) => Box::new(move |path: &str| filter.matches(path)),
            Err(err) => {
                tracing::warn!(
                    pattern = %pattern_owned,
                    error = %err,
                    "gix-pathmatch parse failed, using legacy matcher"
                );
                legacy_glob_factory(&pattern_owned)
            }
        }
    }
    #[cfg(not(feature = "gix-pathmatch"))]
    {
        legacy_glob_factory(pattern)
    }
}

/// Legacy fallback — simple suffix / exact-match logic. Retained so the
/// `gix-pathmatch` feature flag can be rolled back if a regression surfaces.
fn legacy_glob_factory(pattern: &str) -> Box<dyn Fn(&str) -> bool> {
    let pattern = pattern.to_owned();
    Box::new(move |path: &str| {
        if let Some(suffix) = pattern.strip_prefix('*') {
            path.ends_with(suffix)
        } else {
            path == pattern || path.ends_with(&format!("/{pattern}"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_threshold_none_returns_zero() {
        assert_eq!(parse_size_threshold(None).unwrap(), 0);
    }

    #[test]
    fn parse_size_threshold_bytes() {
        assert_eq!(parse_size_threshold(Some("100b")).unwrap(), 100);
        assert_eq!(parse_size_threshold(Some("100")).unwrap(), 100);
    }

    #[test]
    fn parse_size_threshold_kilobytes() {
        assert_eq!(parse_size_threshold(Some("1kb")).unwrap(), 1024);
        assert_eq!(parse_size_threshold(Some("1k")).unwrap(), 1024);
        assert_eq!(parse_size_threshold(Some("500KB")).unwrap(), 500 * 1024);
    }

    #[test]
    fn parse_size_threshold_megabytes() {
        assert_eq!(parse_size_threshold(Some("1mb")).unwrap(), 1024 * 1024);
        assert_eq!(parse_size_threshold(Some("1m")).unwrap(), 1024 * 1024);
        assert_eq!(
            parse_size_threshold(Some("10MB")).unwrap(),
            10 * 1024 * 1024
        );
    }

    #[test]
    fn parse_size_threshold_gigabytes() {
        assert_eq!(
            parse_size_threshold(Some("1gb")).unwrap(),
            1024 * 1024 * 1024
        );
        assert_eq!(
            parse_size_threshold(Some("2G")).unwrap(),
            2 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn parse_size_threshold_invalid() {
        assert!(parse_size_threshold(Some("abc")).is_err());
        assert!(parse_size_threshold(Some("mb")).is_err());
    }

    #[test]
    fn file_extension_extracts_correctly() {
        assert_eq!(file_extension("models/large.bin"), "*.bin");
        assert_eq!(file_extension("data.tar.gz"), "*.gz");
        assert_eq!(file_extension("README"), "(no extension)");
        assert_eq!(file_extension("path/to/file.psd"), "*.psd");
    }

    #[test]
    fn format_size_display() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn parse_size_unit_accepts_si_and_iec_units() {
        assert_eq!(parse_size_unit(Some("mb")).unwrap(), Some(SizeUnit::Mb));
        assert_eq!(parse_size_unit(Some("MiB")).unwrap(), Some(SizeUnit::MiB));
        assert_eq!(parse_size_unit(None).unwrap(), None);
    }

    #[test]
    fn parse_size_unit_rejects_unknown_units() {
        assert!(parse_size_unit(Some("pages")).is_err());
    }

    #[test]
    fn format_size_with_unit_uses_requested_unit() {
        assert_eq!(
            format_size_with_unit(1_500_000, Some(SizeUnit::Mb)),
            "1.5 MB"
        );
        assert_eq!(
            format_size_with_unit(1_572_864, Some(SizeUnit::MiB)),
            "1.5 MiB"
        );
        assert_eq!(format_size_with_unit(42, Some(SizeUnit::B)), "42 B");
    }

    #[test]
    fn apply_top_truncates_entries() {
        let mut entries = vec![
            MigrateInfoEntry {
                pattern: "*.bin".to_owned(),
                file_count: 3,
                total_size: 30,
            },
            MigrateInfoEntry {
                pattern: "*.psd".to_owned(),
                file_count: 2,
                total_size: 20,
            },
        ];

        let lfs_objects = limit_info_entries(&mut entries, Some(1));

        assert_eq!(entries.len(), 1);
        assert!(lfs_objects.is_none());
    }

    #[test]
    fn migrate_info_top_defaults_to_five_regular_entries() {
        let mut entries = (0..6)
            .map(|index| MigrateInfoEntry {
                pattern: format!("*.{index}"),
                file_count: 1,
                total_size: index,
            })
            .collect::<Vec<_>>();

        let lfs_objects = limit_info_entries(&mut entries, None);

        assert_eq!(entries.len(), DEFAULT_MIGRATE_INFO_TOP);
        assert!(lfs_objects.is_none());
    }

    #[test]
    fn migrate_info_top_keeps_lfs_objects_separate() {
        let mut entries = vec![
            MigrateInfoEntry {
                pattern: "*.bin".to_owned(),
                file_count: 3,
                total_size: 30,
            },
            MigrateInfoEntry {
                pattern: LFS_OBJECTS_PATTERN.to_owned(),
                file_count: 2,
                total_size: 200,
            },
            MigrateInfoEntry {
                pattern: "*.psd".to_owned(),
                file_count: 2,
                total_size: 20,
            },
        ];

        let lfs_objects = limit_info_entries(&mut entries, Some(1));

        assert_eq!(entries.len(), 1);
        assert_eq!(
            lfs_objects.map(|entry| entry.pattern),
            Some(LFS_OBJECTS_PATTERN.to_owned())
        );
    }

    #[test]
    fn parse_batch_check_size_blob() {
        assert_eq!(parse_batch_check_size("abc123 blob 1048576"), Some(1048576));
    }

    #[test]
    fn parse_batch_check_size_non_blob() {
        assert_eq!(parse_batch_check_size("abc123 tree 40"), None);
        assert_eq!(parse_batch_check_size("abc123 commit 200"), None);
    }

    #[test]
    fn parse_batch_check_size_missing() {
        assert_eq!(parse_batch_check_size("abc123 missing"), None);
    }

    #[test]
    fn glob_matches_wildcard_prefix() {
        let matcher = glob_matches_factory("*.bin");
        assert!(matcher("models/large.bin"));
        assert!(matcher("large.bin"));
        assert!(!matcher("large.txt"));
    }

    #[test]
    fn glob_matches_exact_root_path() {
        let matcher = glob_matches_factory("README.md");
        assert!(matcher("README.md"));
        assert!(!matcher("docs/README.md"));
        assert!(!matcher("README.txt"));
    }

    #[test]
    fn path_matcher_applies_include_and_exclude() {
        let matcher = PathMatcher::new(Some("*.bin"), Some("data/skip.bin"));

        assert!(matcher.matches("data/keep.bin"));
        assert!(!matcher.matches("data/skip.bin"));
        assert!(!matcher.matches("data/readme.md"));
    }

    #[test]
    fn path_matcher_without_include_matches_everything_except_excludes() {
        let matcher = PathMatcher::new(None, Some("*.tmp"));

        assert!(matcher.matches("data/file.bin"));
        assert!(!matcher.matches("data/file.tmp"));
    }

    #[test]
    fn import_source_size_requires_crab_pointer_when_from_crab() {
        assert_eq!(import_source_size(b"not a pointer", true), None);

        let pointer = Pointer {
            file_hash: [7u8; 32],
            size: 1234,
            shard_hint: None,
        };
        assert_eq!(import_source_size(&pointer.serialize(), true), Some(1234));
    }

    #[test]
    fn import_source_size_skips_lfs_pointer_without_from_crab() {
        let pointer = LfsPointer {
            oid: [9u8; 32],
            size: 55,
            extensions: Vec::new(),
        };

        assert_eq!(import_source_size(&pointer.serialize(), false), None);
        assert_eq!(import_source_size(b"payload", false), Some(7));
    }

    #[test]
    fn migrate_info_follow_counts_lfs_pointer_targets() {
        let pointer = LfsPointer {
            oid: [9u8; 32],
            size: 55,
            extensions: Vec::new(),
        };
        let mut by_ext = HashMap::new();

        add_info_entry_for_blob(
            &mut by_ext,
            "model.bin",
            pointer.serialize().len() as u64,
            Some(pointer),
            0,
            MigrateInfoPointerMode::Follow,
        );

        assert_eq!(by_ext.get("LFS Objects"), Some(&(1, 55)));
    }

    #[test]
    fn migrate_info_no_follow_counts_pointer_blob() {
        let pointer = LfsPointer {
            oid: [9u8; 32],
            size: 55,
            extensions: Vec::new(),
        };
        let pointer_size = pointer.serialize().len() as u64;
        let mut by_ext = HashMap::new();

        add_info_entry_for_blob(
            &mut by_ext,
            "model.bin",
            pointer_size,
            Some(pointer),
            0,
            MigrateInfoPointerMode::NoFollow,
        );

        assert_eq!(by_ext.get("*.bin"), Some(&(1, pointer_size)));
    }

    #[test]
    fn crab_tracking_lines_use_crab_filter_suffix() {
        let mut patterns = HashSet::new();
        patterns.insert("*.bin".to_owned());
        patterns.insert("README".to_owned());

        assert_eq!(
            crab_tracking_lines_for_patterns(&patterns),
            vec![
                "*.bin filter=crab diff=crab merge=crab -text",
                "README filter=crab diff=crab merge=crab -text",
            ]
        );
    }

    #[test]
    fn export_adds_git_lfs_untrack_override() {
        let input = b"blob\nmark :1\ndata 42\n*.bin filter=lfs diff=lfs merge=lfs -text\n\
            commit refs/heads/main\nmark :2\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 .gitattributes\n\n";
        let mut stream = parse_export_stream(input).unwrap();
        let mark_to_blob: HashMap<u64, usize> = stream
            .blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (b.mark, i))
            .collect();

        update_gitattributes_for_export(
            &mut stream,
            "*.bin",
            "*.bin filter=lfs diff=lfs merge=lfs -text",
            &mark_to_blob,
            false,
        );

        let attrs = stream.commits[0]
            .file_ops
            .iter()
            .find_map(|op| match &op.kind {
                FileOpKind::ModifyInline { path, data, .. } if path == ".gitattributes" => {
                    Some(String::from_utf8_lossy(data))
                }
                _ => None,
            })
            .unwrap();

        assert!(attrs.contains("*.bin filter=lfs diff=lfs merge=lfs -text"));
        assert!(attrs.contains("*.bin !text !filter !merge !diff"));
    }

    #[test]
    fn export_to_crab_replaces_lfs_tracking_with_crab_tracking() {
        let input = b"blob\nmark :1\ndata 42\n*.bin filter=lfs diff=lfs merge=lfs -text\n\
            commit refs/heads/main\nmark :2\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 .gitattributes\n\n";
        let mut stream = parse_export_stream(input).unwrap();
        let mark_to_blob: HashMap<u64, usize> = stream
            .blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (b.mark, i))
            .collect();

        update_gitattributes_for_export(
            &mut stream,
            "*.bin",
            "*.bin filter=lfs diff=lfs merge=lfs -text",
            &mark_to_blob,
            true,
        );

        let attrs = stream.commits[0]
            .file_ops
            .iter()
            .find_map(|op| match &op.kind {
                FileOpKind::ModifyInline { path, data, .. } if path == ".gitattributes" => {
                    Some(String::from_utf8_lossy(data))
                }
                _ => None,
            })
            .unwrap();

        assert!(!attrs.contains("*.bin filter=lfs diff=lfs merge=lfs -text"));
        assert!(attrs.contains("*.bin filter=crab diff=crab merge=crab -text"));
    }

    #[test]
    fn crab_pointer_for_content_stages_bytes_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let staging = crate::cmd::lfs::block_on_runtime(async {
            StagingArea::open(dir.path().to_path_buf())
                .await
                .map_err(CrabError::from)
        })
        .unwrap();
        let content = b"history LFS content".to_vec();

        let pointer_bytes =
            crab_pointer_for_content(&staging, "models/model.bin", content.clone()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, *blake3::hash(&content).as_bytes());
        assert_eq!(pointer.size, content.len() as u64);
        assert!(
            !staging
                .chunks_for_file(&MerkleHash::from(pointer.file_hash))
                .unwrap()
                .is_empty()
        );

        let second =
            crab_pointer_for_content(&staging, "models/model.bin", content.clone()).unwrap();
        assert_eq!(second, pointer_bytes);
    }

    #[test]
    fn import_above_rejects_include_filter() {
        let result = validate_import_filters(&MigrateImportOptions {
            include: Some("*.bin"),
            exclude: None,
            above: Some("1mb"),
            fixup: false,
            no_rewrite: false,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: false,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_err());
    }

    #[test]
    fn import_above_rejects_exclude_filter() {
        let result = validate_import_filters(&MigrateImportOptions {
            include: None,
            exclude: Some("skip.bin"),
            above: Some("1mb"),
            fixup: false,
            no_rewrite: false,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: false,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_err());
    }

    #[test]
    fn import_defaults_to_all_paths_without_selector() {
        let result = validate_import_filters(&MigrateImportOptions {
            include: None,
            exclude: None,
            above: None,
            fixup: false,
            no_rewrite: false,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: false,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_ok());
    }

    #[test]
    fn import_fixup_rejects_explicit_filters() {
        let result = validate_import_filters(&MigrateImportOptions {
            include: Some("*.bin"),
            exclude: None,
            above: None,
            fixup: true,
            no_rewrite: false,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: false,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_err());
    }

    #[test]
    fn import_no_rewrite_requires_files() {
        let result = validate_import_no_rewrite_options(&MigrateImportOptions {
            include: None,
            exclude: None,
            above: None,
            fixup: false,
            no_rewrite: true,
            no_rewrite_files: Vec::new(),
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: true,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_err());
    }

    #[test]
    fn import_no_rewrite_ignores_rewrite_filters_and_ref_selectors() {
        let result = validate_import_no_rewrite_options(&MigrateImportOptions {
            include: Some("*.bin"),
            exclude: Some("*.tmp"),
            above: Some("1b"),
            fixup: false,
            no_rewrite: true,
            no_rewrite_files: vec!["data/model.bin".to_owned()],
            message: None,
            object_map: Some("object-map.csv"),
            refs: MigrateRefSelection {
                include_refs: vec!["HEAD".to_owned()],
                skip_fetch: true,
                ..MigrateRefSelection::default()
            },
            yes: true,
            verbose: false,
            from_crab: true,
        });

        assert!(result.is_ok());
    }

    #[test]
    fn import_no_rewrite_rejects_fixup() {
        let result = validate_import_no_rewrite_options(&MigrateImportOptions {
            include: None,
            exclude: None,
            above: None,
            fixup: true,
            no_rewrite: true,
            no_rewrite_files: vec!["data/model.bin".to_owned()],
            message: None,
            object_map: None,
            refs: MigrateRefSelection::default(),
            yes: true,
            verbose: false,
            from_crab: false,
        });

        assert!(result.is_err());
    }

    #[test]
    fn no_rewrite_path_normalization_rejects_escaping_paths() {
        assert_eq!(
            normalize_no_rewrite_path("./data/model.bin").unwrap(),
            "data/model.bin"
        );
        assert!(normalize_no_rewrite_path("../model.bin").is_err());
        assert!(normalize_no_rewrite_path("/tmp/model.bin").is_err());
    }

    #[test]
    fn import_fixup_matches_lfs_attrs() {
        let fixup = FixupMatcher::from_patterns(vec!["*.bin".to_owned()]);
        let matcher = PathMatcher::new(None, None);

        assert!(should_import_path(
            "data/model.bin",
            1,
            &matcher,
            0,
            Some(&fixup),
        ));
        assert!(!should_import_path(
            "data/readme.txt",
            1,
            &matcher,
            0,
            Some(&fixup),
        ));
        assert!(!should_import_path(
            ".gitattributes",
            1,
            &matcher,
            0,
            Some(&fixup),
        ));
    }

    #[test]
    fn import_tracking_patterns_derive_from_paths() {
        let mut patterns = HashSet::new();

        add_tracking_pattern(&mut patterns, None, "data/model.bin");
        add_tracking_pattern(&mut patterns, None, "README");
        let lines = tracking_lines_for_patterns(&patterns);

        assert_eq!(
            lines,
            vec![
                "*.bin filter=lfs diff=lfs merge=lfs -text",
                "README filter=lfs diff=lfs merge=lfs -text",
            ]
        );
    }

    #[test]
    fn revision_selection_defaults_to_current_branch() {
        let selection = build_revision_selection(
            &MigrateRefSelection::default(),
            Some("refs/heads/main".to_owned()),
        )
        .unwrap();

        assert_eq!(selection.args, vec!["refs/heads/main"]);
        assert!(!selection.all_refs);
    }

    #[test]
    fn revision_selection_default_scope_excludes_remote_refs() {
        let selection = build_revision_selection_with_remote_refs(
            &MigrateRefSelection::default(),
            Some("refs/heads/main".to_owned()),
            &[
                "refs/remotes/origin/main".to_owned(),
                "refs/remotes/upstream/main".to_owned(),
            ],
        )
        .unwrap();

        assert_eq!(
            selection.args,
            vec![
                "refs/heads/main",
                "^refs/remotes/origin/main",
                "^refs/remotes/upstream/main",
            ]
        );
    }

    #[test]
    fn revision_selection_branch_scope_excludes_remote_refs() {
        let selection = build_revision_selection_with_remote_refs(
            &MigrateRefSelection {
                branches: vec!["feature".to_owned()],
                ..MigrateRefSelection::default()
            },
            Some("refs/heads/main".to_owned()),
            &["refs/remotes/origin/main".to_owned()],
        )
        .unwrap();

        assert_eq!(selection.args, vec!["feature", "^refs/remotes/origin/main"]);
    }

    #[test]
    fn revision_selection_explicit_ref_filters_do_not_add_remote_ref_exclusions() {
        let selection = build_revision_selection_with_remote_refs(
            &MigrateRefSelection {
                include_refs: vec!["refs/heads/main".to_owned()],
                ..MigrateRefSelection::default()
            },
            Some("refs/heads/current".to_owned()),
            &["refs/remotes/origin/main".to_owned()],
        )
        .unwrap();

        assert_eq!(selection.args, vec!["refs/heads/main"]);
    }

    #[test]
    fn revision_selection_combines_branch_and_ref_filters() {
        let selection = build_revision_selection(
            &MigrateRefSelection {
                include_refs: vec!["refs/heads/release".to_owned()],
                exclude_refs: vec!["refs/remotes/origin/main".to_owned()],
                branches: vec!["main".to_owned(), "^old".to_owned()],
                ..MigrateRefSelection::default()
            },
            Some("refs/heads/current".to_owned()),
        )
        .unwrap();

        assert_eq!(
            selection.args,
            vec![
                "main",
                "refs/heads/release",
                "^old",
                "^refs/remotes/origin/main"
            ]
        );
        assert_eq!(selection.positive_refs, vec!["main", "refs/heads/release"]);
    }

    #[test]
    fn revision_selection_all_refs_accepts_exclusions() {
        let selection = build_revision_selection(
            &MigrateRefSelection {
                everything: true,
                exclude_refs: vec!["refs/remotes/origin/main".to_owned()],
                ..MigrateRefSelection::default()
            },
            None,
        )
        .unwrap();

        assert_eq!(selection.args, vec!["--all", "^refs/remotes/origin/main"]);
        assert!(selection.all_refs);
    }

    #[test]
    fn revision_selection_rejects_all_refs_with_positive_refs() {
        let result = build_revision_selection(
            &MigrateRefSelection {
                everything: true,
                branches: vec!["main".to_owned()],
                ..MigrateRefSelection::default()
            },
            None,
        );

        assert!(result.is_err());
    }

    #[test]
    fn migrate_ref_selection_refreshes_remote_refs_by_default() {
        assert!(should_refresh_remote_refs(&MigrateRefSelection::default()));
    }

    #[test]
    fn migrate_ref_selection_excludes_remote_refs_by_default() {
        assert!(should_exclude_remote_refs(&MigrateRefSelection::default()));
    }

    #[test]
    fn migrate_ref_selection_skip_fetch_disables_remote_refresh() {
        let selection = MigrateRefSelection {
            skip_fetch: true,
            ..MigrateRefSelection::default()
        };

        assert!(!should_refresh_remote_refs(&selection));
        assert!(should_exclude_remote_refs(&selection));
    }

    #[test]
    fn migrate_ref_selection_explicit_ref_filters_disable_remote_refresh() {
        let include = MigrateRefSelection {
            include_refs: vec!["refs/heads/main".to_owned()],
            ..MigrateRefSelection::default()
        };
        let exclude = MigrateRefSelection {
            exclude_refs: vec!["refs/remotes/origin/main".to_owned()],
            ..MigrateRefSelection::default()
        };

        assert!(!should_refresh_remote_refs(&include));
        assert!(!should_refresh_remote_refs(&exclude));
        assert!(!should_exclude_remote_refs(&include));
        assert!(!should_exclude_remote_refs(&exclude));
    }

    #[test]
    fn migrate_ref_selection_everything_disables_remote_ref_exclusions() {
        let selection = MigrateRefSelection {
            everything: true,
            ..MigrateRefSelection::default()
        };

        assert!(!should_exclude_remote_refs(&selection));
    }

    #[test]
    fn parse_git_remote_names_skips_blank_lines() {
        assert_eq!(
            parse_git_remote_names("origin\n\nupstream \n"),
            vec!["origin", "upstream"]
        );
    }

    // --- fast-export stream parsing tests ---

    #[test]
    fn parse_blob_section_roundtrip() {
        let input = b"blob\nmark :1\ndata 5\nhello\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.blobs.len(), 1);
        assert_eq!(stream.blobs[0].mark, 1);
        assert_eq!(stream.blobs[0].data, b"hello");
    }

    #[test]
    fn parse_blob_skips_original_oid() {
        let input = b"blob\nmark :1\noriginal-oid abc123\ndata 5\nhello\n";
        let stream = parse_export_stream(input).unwrap();

        assert_eq!(stream.blobs[0].data, b"hello");
    }

    #[test]
    fn parse_commit_with_file_ops() {
        let input = b"blob\nmark :1\ndata 5\nhello\n\
            commit refs/heads/main\nmark :2\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 file.txt\n\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.blobs.len(), 1);
        assert_eq!(stream.commits.len(), 1);
        assert_eq!(stream.commits[0].mark, Some(2));
        assert_eq!(stream.commits[0].original_oid, None);
        assert_eq!(stream.commits[0].file_ops.len(), 1);
        match &stream.commits[0].file_ops[0].kind {
            FileOpKind::Modify {
                mode,
                dataref,
                path,
            } => {
                assert_eq!(mode, "100644");
                assert_eq!(dataref, ":1");
                assert_eq!(path, "file.txt");
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn parse_commit_captures_original_oid_without_writing_it() {
        let input = b"blob\nmark :1\ndata 5\nhello\n\
            commit refs/heads/main\nmark :2\n\
            original-oid 1111111111111111111111111111111111111111\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 file.txt\n\n";
        let stream = parse_export_stream(input).unwrap();

        assert_eq!(
            stream.commits[0].original_oid.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );

        let mut output = Vec::new();
        write_stream(&mut output, &stream).unwrap();
        let text = String::from_utf8_lossy(&output);
        assert!(!text.contains("original-oid"));
    }

    #[test]
    fn parse_inline_data_in_commit() {
        let input = b"commit refs/heads/main\nmark :1\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 inline file.txt\ndata 5\nhello\n\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.commits.len(), 1);
        assert_eq!(stream.commits[0].file_ops.len(), 1);
        match &stream.commits[0].file_ops[0].kind {
            FileOpKind::ModifyInline { mode, path, data } => {
                assert_eq!(mode, "100644");
                assert_eq!(path, "file.txt");
                assert_eq!(data, b"hello");
            }
            other => panic!("expected ModifyInline, got {other:?}"),
        }
    }

    #[test]
    fn parse_delete_op() {
        let input = b"commit refs/heads/main\nmark :1\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            D old_file.txt\n\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.commits[0].file_ops.len(), 1);
        match &stream.commits[0].file_ops[0].kind {
            FileOpKind::Delete { path } => {
                assert_eq!(path, "old_file.txt");
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn write_stream_roundtrip() {
        let input = b"blob\nmark :1\ndata 5\nhello\n\
            commit refs/heads/main\nmark :2\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 file.txt\n\n";
        let stream = parse_export_stream(input).unwrap();

        let mut output = Vec::new();
        write_stream(&mut output, &stream).unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("blob\nmark :1\ndata 5\n"));
        assert!(output_str.contains("commit refs/heads/main"));
        assert!(output_str.contains("M 100644 :1 file.txt"));
        assert!(output_str.contains("done"));
    }

    #[test]
    fn unquote_path_plain() {
        assert_eq!(unquote_path("file.txt"), "file.txt");
    }

    #[test]
    fn unquote_path_quoted() {
        assert_eq!(unquote_path("\"file name.txt\""), "file name.txt");
    }

    #[test]
    fn unquote_path_escaped() {
        assert_eq!(unquote_path("\"file\\\\name.txt\""), "file\\name.txt");
        assert_eq!(unquote_path("\"file\\nname.txt\""), "file\nname.txt");
    }

    #[test]
    fn quote_path_plain() {
        assert_eq!(quote_path("file.txt"), "file.txt");
    }

    #[test]
    fn quote_path_with_space() {
        assert_eq!(quote_path("file name.txt"), "\"file name.txt\"");
    }

    #[test]
    fn find_max_mark_works() {
        let stream = ExportStream {
            order: vec![SectionRef::Blob(0), SectionRef::Commit(0)],
            blobs: vec![ExportedBlob {
                mark: 5,
                data: vec![],
            }],
            commits: vec![ExportedCommit {
                header_lines: vec![],
                mark: Some(10),
                original_oid: None,
                file_ops: vec![],
            }],
            passthroughs: vec![],
        };
        assert_eq!(find_max_mark(&stream), 10);
    }

    #[test]
    fn write_object_map_writes_old_to_new_commit_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("object-map.csv");
        let stream = ExportStream {
            order: vec![SectionRef::Commit(0)],
            blobs: vec![],
            commits: vec![ExportedCommit {
                header_lines: vec![],
                mark: Some(2),
                original_oid: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                file_ops: vec![],
            }],
            passthroughs: vec![],
        };
        let mut marks = HashMap::new();
        marks.insert(2, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned());

        write_object_map(path.to_str().unwrap(), &stream, Some(&marks)).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            content,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
        );
    }

    #[test]
    fn verbose_commit_label_prefers_original_oid_then_mark() {
        let with_original = ExportedCommit {
            header_lines: Vec::new(),
            mark: Some(7),
            original_oid: Some("abcdef".to_owned()),
            file_ops: Vec::new(),
        };
        let with_mark = ExportedCommit {
            header_lines: Vec::new(),
            mark: Some(8),
            original_oid: None,
            file_ops: Vec::new(),
        };

        assert_eq!(commit_verbose_label(&with_original), "abcdef");
        assert_eq!(commit_verbose_label(&with_mark), ":8");
    }

    #[test]
    fn is_lfs_pointer_detects_pointer() {
        let pointer_bytes = format!(
            "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
             size 1024\n"
        );
        assert!(is_lfs_pointer(pointer_bytes.as_bytes()));
    }

    #[test]
    fn is_lfs_pointer_rejects_non_pointer() {
        assert!(!is_lfs_pointer(b"hello world"));
    }

    #[test]
    fn inject_gitattributes_adds_tracking_line() {
        let input = b"blob\nmark :1\ndata 5\nhello\n\
            commit refs/heads/main\nmark :2\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 4\ntest\n\
            M 100644 :1 file.bin\n\n";
        let mut stream = parse_export_stream(input).unwrap();

        let mark_to_blob: HashMap<u64, usize> = stream
            .blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (b.mark, i))
            .collect();

        let mut next_mark = find_max_mark(&stream) + 1;
        let tracking_lines = vec![
            "*.bin filter=lfs diff=lfs merge=lfs -text".to_owned(),
            "README filter=lfs diff=lfs merge=lfs -text".to_owned(),
        ];
        inject_gitattributes_for_import(
            &mut stream,
            &tracking_lines,
            &mark_to_blob,
            &mut next_mark,
        );

        // The commit should now have a .gitattributes file op.
        let commit = &stream.commits[0];
        let attrs_op = commit.file_ops.iter().find(|op| {
            matches!(&op.kind, FileOpKind::ModifyInline { path, .. } if path == ".gitattributes")
        });
        assert!(attrs_op.is_some(), "expected .gitattributes file op");

        if let FileOpKind::ModifyInline { data, .. } = &attrs_op.unwrap().kind {
            let text = String::from_utf8_lossy(data);
            assert!(text.contains("*.bin filter=lfs diff=lfs merge=lfs -text"));
            assert!(text.contains("README filter=lfs diff=lfs merge=lfs -text"));
        }
    }

    #[test]
    fn remove_gitattributes_strips_tracking_line() {
        let attrs_content = "*.txt text\n*.bin filter=lfs diff=lfs merge=lfs -text\n";

        // Build stream manually for this test.
        let mut stream = ExportStream {
            order: vec![SectionRef::Blob(0), SectionRef::Commit(0)],
            blobs: vec![ExportedBlob {
                mark: 1,
                data: attrs_content.as_bytes().to_vec(),
            }],
            commits: vec![ExportedCommit {
                header_lines: vec![
                    "commit refs/heads/main".to_owned(),
                    "mark :2".to_owned(),
                    "author Test <test@test.com> 1000000000 +0000".to_owned(),
                    "committer Test <test@test.com> 1000000000 +0000".to_owned(),
                    "data 4".to_owned(),
                    "test".to_owned(),
                ],
                mark: Some(2),
                original_oid: None,
                file_ops: vec![FileOp {
                    kind: FileOpKind::Modify {
                        mode: "100644".to_owned(),
                        dataref: ":1".to_owned(),
                        path: ".gitattributes".to_owned(),
                    },
                }],
            }],
            passthroughs: vec![],
        };

        let mark_to_blob: HashMap<u64, usize> = stream
            .blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (b.mark, i))
            .collect();

        remove_gitattributes_for_export(
            &mut stream,
            "*.bin filter=lfs diff=lfs merge=lfs -text",
            &mark_to_blob,
        );

        // The .gitattributes op should now be inline with the line removed.
        let commit = &stream.commits[0];
        let attrs_op = commit.file_ops.iter().find(|op| {
            matches!(&op.kind, FileOpKind::ModifyInline { path, .. } if path == ".gitattributes")
        });
        assert!(attrs_op.is_some());

        if let FileOpKind::ModifyInline { data, .. } = &attrs_op.unwrap().kind {
            let text = String::from_utf8_lossy(data);
            assert!(text.contains("*.txt text"));
            assert!(!text.contains("filter=lfs"));
        }
    }

    #[test]
    fn parse_reset_section() {
        let input = b"reset refs/heads/main\n\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.passthroughs.len(), 1);
        assert_eq!(stream.passthroughs[0].lines[0], "reset refs/heads/main");
    }

    #[test]
    fn parse_multiple_blobs_and_commits() {
        let input = b"blob\nmark :1\ndata 5\nhello\n\
            blob\nmark :2\ndata 5\nworld\n\
            commit refs/heads/main\nmark :3\n\
            author Test <test@test.com> 1000000000 +0000\n\
            committer Test <test@test.com> 1000000000 +0000\n\
            data 7\ncommit1\n\
            M 100644 :1 a.txt\n\
            M 100644 :2 b.txt\n\n";
        let stream = parse_export_stream(input).unwrap();
        assert_eq!(stream.blobs.len(), 2);
        assert_eq!(stream.commits.len(), 1);
        assert_eq!(stream.commits[0].file_ops.len(), 2);
    }
}
