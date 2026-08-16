//! `crab diff-driver` — git external diff driver for chunk-level diffs.
//!
//! Conforms to git's external diff driver protocol: receives path, old-file,
//! old-hex, old-mode, new-file, new-hex, new-mode as arguments. Reads the
//! file contents, parses as crab pointers, resolves chunk sequences, compares,
//! and formats output to stdout.
//!
//! Git applies the smudge filter before passing temp files to the diff driver,
//! so hydrated content may arrive instead of pointer blobs. When that happens,
//! we fall back to reading the raw blob from git's object store using the hex
//! SHA provided in the arguments.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::cache::LocalCache;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::diff::formatter::{format_diff, format_size};
use crab_diff::chunk_sequence::{ChunkSequence, compare_sequences};
use crab_diff::types::{ChunkSequenceSourceKind, DiffSummary, FileDiffEntry, OutputMode};
use crab_staging::StagingAreaReadOnly;
use crab_types::pointer::{Pointer, is_pointer};
use crab_xet::hash::MerkleHash;

/// Arguments matching git's external diff driver protocol.
#[derive(Debug, Clone)]
pub struct DiffDriverArgs {
    pub path: String,
    pub old_file: PathBuf,
    pub old_hex: String,
    pub old_mode: String,
    pub new_file: PathBuf,
    pub new_hex: String,
    pub new_mode: String,
}

/// Try to parse content as a pointer. If it's not a pointer and a valid git
/// object hex is available, read the raw blob from git (bypassing the smudge
/// filter) and try again.
fn resolve_pointer(content: &[u8], hex: &str) -> Option<Pointer> {
    if is_pointer(content) {
        return Pointer::parse(content).ok();
    }
    // The hex is "." for a null side (new file / deleted file) or a full SHA.
    if hex == "." || hex.len() < 4 {
        return None;
    }
    // Read the raw blob from git's object store — this is the pointer before
    // the smudge filter expanded it.
    let output = std::process::Command::new("git")
        .args(["cat-file", "blob", hex])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    if is_pointer(&output.stdout) {
        Pointer::parse(&output.stdout).ok()
    } else {
        None
    }
}

/// Entry point for `crab diff-driver`.
pub async fn run_diff_driver(
    args: DiffDriverArgs,
    config: Config,
    cancel: CancellationToken,
) -> Result<()> {
    let _span = tracing::info_span!("diff_driver", path = %args.path).entered();

    // Read both file contents.
    let old_content = std::fs::read(&args.old_file).map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to read old file {}: {e}", args.old_file.display()),
        ))
    })?;
    let new_content = std::fs::read(&args.new_file).map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to read new file {}: {e}", args.new_file.display()),
        ))
    })?;

    // Try to parse both as crab pointers. If the temp file was smudge-
    // filtered (hydrated), fall back to reading the raw blob via git cat-file.
    let old_ptr = resolve_pointer(&old_content, &args.old_hex);
    let new_ptr = resolve_pointer(&new_content, &args.new_hex);

    let old_is_null = is_null_side(&args.old_file, &old_content, &args.old_hex);
    let new_is_null = is_null_side(&args.new_file, &new_content, &args.new_hex);

    // A mixed pointer/content pair is usually `git diff` against a hydrated
    // working-tree file. The content has not gone through the clean filter
    // yet, so there is no pointer or reconstruction metadata for that side.
    // Treat it as a size-only modification instead of a deletion/addition.
    match (&old_ptr, &new_ptr) {
        (Some(old), None) if !new_is_null => {
            print_mixed_pointer_content(&args.path, old.size, new_content.len() as u64);
            return Ok(());
        }
        (None, Some(new)) if !old_is_null => {
            print_mixed_pointer_content(&args.path, old_content.len() as u64, new.size);
            return Ok(());
        }
        _ => {}
    }

    // If neither is a pointer, fall back to file size difference.
    if old_ptr.is_none() && new_ptr.is_none() {
        let old_size = old_content.len() as u64;
        let new_size = new_content.len() as u64;
        println!(
            "{}: {} → {} (not crab-tracked)",
            args.path,
            format_size(old_size),
            format_size(new_size),
        );
        return Ok(());
    }

    // Collect hashes for committed chunk-sequence resolution.
    let mut hashes_to_resolve: Vec<(MerkleHash, Option<MerkleHash>, u64)> = Vec::new();
    if let Some(ref ptr) = old_ptr {
        hashes_to_resolve.push(hash_with_hint_and_size(ptr));
    }
    if let Some(ref ptr) = new_ptr {
        hashes_to_resolve.push(hash_with_hint_and_size(ptr));
    }

    // Resolve committed sequences from remote metadata.
    let mut sequences = if hashes_to_resolve.is_empty() {
        std::collections::HashMap::new()
    } else {
        match crate::cmd::diff::create_store_and_prefix(&config, &cancel).await {
            Ok((store, prefix)) => {
                let cache =
                    std::sync::Arc::new(LocalCache::new(crate::cache::default_cache_root()));
                let router = crate::storage::StoreLayout::new(
                    crate::storage::Store::from_storage(store.origin().clone()),
                    prefix,
                );
                let resolver = crate::diff::term_resolver::TermResolver::new(
                    store,
                    router,
                    cache,
                    config.download_concurrency,
                );
                resolver
                    .resolve_sequences_batch(
                        &hashes_to_resolve,
                        ChunkSequenceSourceKind::Committed,
                        &cancel,
                    )
                    .await?
            }
            Err(err) => {
                debug!(path = %args.path, error = %err, "committed chunk sequence lookup unavailable");
                std::collections::HashMap::new()
            }
        }
    };

    // Fill unpushed pointer sides from local staging when possible.
    if let Some(ref ptr) = old_ptr {
        insert_staged_sequence_if_missing(&mut sequences, ptr).await?;
    }
    if let Some(ref ptr) = new_ptr {
        insert_staged_sequence_if_missing(&mut sequences, ptr).await?;
    }

    // Build the diff report.
    let old_hash = old_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
    let new_hash = new_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
    let old_sequence = old_hash.and_then(|h| sequences.get(&h));
    let new_sequence = new_hash.and_then(|h| sequences.get(&h));

    let old_size = old_ptr
        .as_ref()
        .map_or(old_content.len() as u64, |p| p.size);
    let new_size = new_ptr
        .as_ref()
        .map_or(new_content.len() as u64, |p| p.size);

    let report = match (old_sequence, new_sequence) {
        (Some(old_seq), Some(new_seq)) => compare_sequences(&args.path, old_seq, new_seq),
        (None, Some(new_seq)) if old_ptr.is_none() => {
            let empty_old = empty_sequence(ChunkSequenceSourceKind::Committed);
            compare_sequences(&args.path, &empty_old, new_seq)
        }
        (None, Some(_new_seq)) => {
            debug!(path = %args.path, "old version chunk sequence unavailable");
            println!(
                "{}: {} → {} (chunk diff unavailable)",
                args.path,
                format_size(old_size),
                format_size(new_size),
            );
            return Ok(());
        }
        (Some(_old_seq), None) if new_ptr.is_some() => {
            // New pointer exists but metadata is unavailable. Show a
            // size-based modified report instead of treating it as deletion.
            debug!(path = %args.path, "new version chunk sequence unavailable");
            println!(
                "{}: {} → {} (chunk diff unavailable)",
                args.path,
                format_size(old_size),
                format_size(new_size),
            );
            return Ok(());
        }
        (Some(old_seq), None) => {
            let empty_new = empty_sequence(ChunkSequenceSourceKind::Committed);
            compare_sequences(&args.path, old_seq, &empty_new)
        }
        (None, None) => {
            // Both pointers but sequences unavailable — report size diff.
            debug!(path = %args.path, "chunk sequence resolution failed for both versions");
            println!(
                "{}: {} → {} (chunk diff unavailable)",
                args.path,
                format_size(old_size),
                format_size(new_size),
            );
            return Ok(());
        }
    };

    let entries = vec![FileDiffEntry { report }];
    let summary = DiffSummary {
        files_changed: 1,
        total_segments_changed: entries[0].report.added_segments
            + entries[0].report.removed_segments,
        total_delta_bytes: entries[0].report.delta_bytes,
    };

    let mut stdout = std::io::stdout().lock();
    format_diff(
        &entries,
        &summary,
        OutputMode::Human,
        false,
        false,
        &mut stdout,
    )?;

    Ok(())
}

/// Extract `(MerkleHash, Option<MerkleHash>, size)` from a pointer.
fn hash_with_hint_and_size(ptr: &Pointer) -> (MerkleHash, Option<MerkleHash>, u64) {
    let file_hash = MerkleHash::from(ptr.file_hash);
    let shard_hint = ptr.shard_hint.map(MerkleHash::from);
    (file_hash, shard_hint, ptr.size)
}

async fn insert_staged_sequence_if_missing(
    sequences: &mut std::collections::HashMap<MerkleHash, ChunkSequence>,
    ptr: &Pointer,
) -> Result<()> {
    let file_hash = MerkleHash::from(ptr.file_hash);
    if sequences.contains_key(&file_hash) {
        return Ok(());
    }
    let Some(sequence) = resolve_staged_sequence(ptr).await? else {
        return Ok(());
    };
    sequences.insert(file_hash, sequence);
    Ok(())
}

async fn resolve_staged_sequence(ptr: &Pointer) -> Result<Option<ChunkSequence>> {
    let Some(crab_dir) = crate::git::discover::resolve_crab_dir() else {
        return Ok(None);
    };
    let staging_root = crab_dir.join("staging");
    // `git diff --cached` can refresh the index and run the clean filter
    // while the external diff driver is resolving the same staged pointer.
    // Wait briefly for that writer so staged diffs do not degrade to
    // size-only output just because the staging lock was momentarily held.
    let staging = match StagingAreaReadOnly::open_blocking_default(staging_root).await {
        Ok(staging) => staging,
        Err(
            crab_staging::StagingError::NotFound { .. }
            | crab_staging::StagingError::StagingLocked { .. },
        ) => {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };

    let file_hash = MerkleHash::from(ptr.file_hash);
    let chunks = staging.chunks_for_file_with_sizes(&file_hash)?;
    if chunks.is_empty() {
        return Ok(None);
    }
    let total_size: u64 = chunks.iter().map(|(_, size)| *size).sum();
    if total_size != ptr.size {
        return Err(CrabError::StagingCorrupt(format!(
            "staged chunks for {} sum to {}, pointer declares {}",
            file_hash.hex(),
            total_size,
            ptr.size
        )));
    }

    Ok(Some(ChunkSequence::from_staged(
        file_hash, ptr.size, &chunks,
    )))
}

fn empty_sequence(source: ChunkSequenceSourceKind) -> ChunkSequence {
    ChunkSequence {
        source,
        file_hash: MerkleHash::default(),
        file_size: 0,
        spans: Vec::new(),
    }
}

fn is_null_side(path: &std::path::Path, content: &[u8], hex: &str) -> bool {
    is_null_hex(hex) && is_null_path(path) && content.is_empty()
}

fn is_null_hex(hex: &str) -> bool {
    hex == "." || (hex.len() >= 4 && hex.bytes().all(|b| b == b'0'))
}

fn is_null_path(path: &std::path::Path) -> bool {
    let path = path.to_string_lossy();
    path == "/dev/null" || path.eq_ignore_ascii_case("NUL")
}

fn print_mixed_pointer_content(path: &str, old_size: u64, new_size: u64) {
    println!(
        "{path}: {} → {} (working tree content is not a crab pointer; run git add for chunk diff)",
        format_size(old_size),
        format_size(new_size),
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test setup")]
mod tests {
    use super::*;

    #[test]
    fn null_side_requires_null_hex_path_and_empty_content() {
        assert!(is_null_side(
            std::path::Path::new("/dev/null"),
            b"",
            "0000000000000000000000000000000000000000"
        ));
        assert!(!is_null_side(
            std::path::Path::new("model.bin"),
            b"",
            "0000000000000000000000000000000000000000"
        ));
        assert!(!is_null_side(
            std::path::Path::new("/dev/null"),
            b"content",
            "0000000000000000000000000000000000000000"
        ));
    }

    #[tokio::test]
    async fn non_pointer_pair_falls_back_without_store_config() {
        let dir = tempfile::tempdir().unwrap();
        let old_file = dir.path().join("old.bin");
        let new_file = dir.path().join("new.bin");
        std::fs::write(&old_file, b"old").unwrap();
        std::fs::write(&new_file, b"new content").unwrap();

        let args = DiffDriverArgs {
            path: "model.bin".to_owned(),
            old_file,
            new_file,
            old_hex: "0".repeat(40),
            old_mode: "100644".to_owned(),
            new_hex: "0".repeat(40),
            new_mode: "100644".to_owned(),
        };

        run_diff_driver(args, Config::default(), CancellationToken::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mixed_pointer_and_worktree_content_does_not_need_remote_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let old_file = dir.path().join("old.bin");
        let new_file = dir.path().join("new.bin");
        let ptr = Pointer {
            file_hash: [0xab; 32],
            size: 5,
            shard_hint: None,
        };
        std::fs::write(&old_file, ptr.serialize()).unwrap();
        std::fs::write(&new_file, b"new content").unwrap();

        let args = DiffDriverArgs {
            path: "model.bin".to_owned(),
            old_file,
            new_file,
            old_hex: "0".repeat(40),
            old_mode: "100644".to_owned(),
            new_hex: "0".repeat(40),
            new_mode: "100644".to_owned(),
        };

        run_diff_driver(args, Config::default(), CancellationToken::new())
            .await
            .unwrap();
    }
}
