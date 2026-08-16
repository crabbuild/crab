//! Worktree materialization helper built on `gix_worktree_state::checkout`.
//!
//! This module is the Req 6 adoption surface for
//! [`gix_worktree_state::checkout`]. It wraps the gitoxide function with
//! an ergonomic signature that takes a mutable index, a [`CrabOdb`]
//! (wired to resolve xorb-backed blobs transparently), and a worktree
//! root, and produces a typed [`Outcome`].
//!
//! The helper itself owns no worktree policy beyond what gitoxide
//! already ships — CRLF normalization, exec-bit handling, symlink
//! target resolution, Windows developer-mode fallback — are all
//! gitoxide's responsibilities. Crab's callers (the VFS engine and
//! [`crate::cmd::hydrate`]) supply the index and the ODB adapter,
//! and receive the typed outcome.
//!
//! # Pointer-backed blob streaming
//!
//! Large pointer-backed blobs (above the configurable threshold,
//! default 256 MiB — see `docs/architecture/gitoxide.md`
//! §Streaming strategy for pointer-backed blobs) are handled in two
//! passes. The ODB adapter returns `None` for oversized blobs; the
//! checkout helper then asks gitoxide to perform a **mode-only pass**
//! that creates the file with the correct mode and zero content. The
//! caller follows up with
//! [`crate::vfs::engine::VfsEngine::promote_pointer_streaming`] (or
//! the hydrate smudge pipeline) to stream the real content into place.
//!
//! This module is intentionally thin — every non-trivial line is in
//! gitoxide. That's the whole point: delete the bespoke worktree code
//! in favor of upstream semantics.

#![cfg(feature = "gix-worktree")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gix_features::progress::Discard;
use gix_worktree_state::checkout::{Options, Outcome};

use crate::core::error::{CrabError, Result};
use crate::git::odb_adapter::CrabOdb;

/// Run `gix_worktree_state::checkout` with the supplied index into the
/// worktree rooted at `worktree_root`, resolving object bytes through
/// `odb` (which itself composes the native git ODB with xorb-backed
/// blob reconstruction).
///
/// Returns the typed [`Outcome`] describing how many files / bytes
/// were written, plus any per-path errors and collisions encountered.
/// The index is consumed by the underlying gitoxide call (the path
/// backing is taken, used, and returned) so the caller keeps ownership
/// of the modified [`gix_index::State`] on return.
///
/// # Errors
///
/// - [`CrabError::GixWorktree`] when gitoxide's checkout fails at a
///   level it surfaces as [`gix_worktree_state::checkout::Error`].
/// - [`CrabError::Io`] when creating the parent worktree directory
///   fails.
///
/// # Cancellation
///
/// The `should_interrupt` flag is honored by gitoxide's internal
/// parallel iterator — flip it to `true` from another thread to stop
/// gracefully. On interruption the function still returns `Ok(outcome)`
/// per gitoxide's contract; callers that care should compare
/// `outcome.files_updated` against the expected index size.
pub fn checkout_from_index(
    index: &mut gix_index::State,
    worktree_root: &Path,
    odb: Arc<CrabOdb>,
    should_interrupt: &AtomicBool,
    options: Options,
) -> Result<Outcome> {
    // gix_worktree_state::checkout creates the worktree entries relative
    // to `dir` but expects `dir` itself to exist. crab's callers are
    // either the VFS mount (worktree always exists) or the hydrate
    // command (operates on the current cwd), so the directory usually
    // exists. Create it defensively — the cost is a single stat on the
    // happy path.
    std::fs::create_dir_all(worktree_root).map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to ensure worktree root exists at {}: {e}",
                worktree_root.display()
            ),
        ))
    })?;

    let mut files_progress = Discard;
    let mut bytes_progress = Discard;

    // `CrabOdb: Clone` is required by gix_worktree_state::checkout
    // because the parallel iterator hands each worker its own clone.
    // Our adapter wraps every heavy state in `Arc`, so cloning is cheap.
    let odb_for_gix = (*odb).clone();

    let outcome = gix_worktree_state::checkout(
        index,
        worktree_root.to_path_buf(),
        odb_for_gix,
        &mut files_progress,
        &mut bytes_progress,
        should_interrupt,
        options,
    )?;

    Ok(outcome)
}

/// Default streaming threshold for pointer-backed blob bypass.
///
/// The ODB adapter returns `None` for blobs declared larger than this,
/// so gitoxide's checkout falls through to a mode-only pass and the
/// streamer (e.g. `VfsEngine::promote_pointer_streaming`) writes the
/// actual content. See `docs/architecture/gitoxide.md` for the
/// rationale.
///
/// 256 MiB keeps typical binaries on the fast path (a 200 MiB xorb
/// bundle fits in memory on CI runners) while forcing the streamer to
/// own multi-gigabyte files. Override via `crab.worktree.streamingThreshold`.
pub const DEFAULT_STREAMING_THRESHOLD_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::git::odb_adapter::NoopXorbResolver;

    fn init_bare_repo(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(dir)
            .status()
            .expect("git init --bare");
        assert!(status.success(), "git init --bare failed");
    }

    #[test]
    fn checkout_empty_index_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repo.git");
        init_bare_repo(&bare);

        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("create worktree");

        let odb = Arc::new(
            CrabOdb::new(&bare.join("objects"), Arc::new(NoopXorbResolver)).expect("open odb"),
        );

        let mut index = gix_index::State::new(gix_hash::Kind::Sha1);
        let interrupt = AtomicBool::new(false);

        let outcome =
            checkout_from_index(&mut index, &worktree, odb, &interrupt, Options::default())
                .expect("checkout empty index");

        // Empty index → nothing to write. This is a smoke test that
        // the plumbing compiles and executes without a panic, not a
        // behavior assertion — that lives in worktree_golden.rs.
        assert_eq!(outcome.files_updated, 0);
        assert_eq!(outcome.bytes_written, 0);
    }

    #[test]
    fn checkout_creates_missing_worktree_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repo.git");
        init_bare_repo(&bare);

        // Worktree dir does NOT exist — checkout_from_index should
        // create it defensively before handing off to gitoxide.
        let worktree = tmp.path().join("does-not-exist-yet");

        let odb = Arc::new(
            CrabOdb::new(&bare.join("objects"), Arc::new(NoopXorbResolver)).expect("open odb"),
        );

        let mut index = gix_index::State::new(gix_hash::Kind::Sha1);
        let interrupt = AtomicBool::new(false);

        checkout_from_index(&mut index, &worktree, odb, &interrupt, Options::default())
            .expect("checkout creates worktree root");

        assert!(worktree.is_dir(), "worktree root should have been created");
    }
}
