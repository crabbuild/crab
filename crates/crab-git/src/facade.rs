//! Thin `gix::Repository` facade.
//!
//! CLI entry points, workflow callers, and diagnostic commands open
//! the repo once via [`open`] / [`open_at`] and thread
//! `&gix::Repository` to helpers that previously shelled out to
//! `git rev-parse`, `git symbolic-ref`, `git log`, etc.
//!
//! The facade is intentionally tiny — it is a boundary, not a
//! wrapper type. Callers get the raw `gix::Repository` back and use
//! gitoxide's native API (`rev_parse_single`, `head_ref`,
//! `find_commit`, `find_remote`, …) from there. Wrapping the
//! entire gitoxide surface in a crab-local type would duplicate
//! hundreds of LoC for no simplification win.
//!
//! Errors retain the operation and repository path so callers can map them to
//! their own presentation layer without depending on CLI error types.

use std::path::{Path, PathBuf};

use gix::Repository;
use thiserror::Error;

macro_rules! gix_boundary {
    ($operation:literal) => {
        tracing::debug_span!(
            concat!("gix.facade.", $operation),
            gix_crate = "facade",
            gix_fn = $operation
        )
    };
}

/// Errors returned by the repository facade.
#[derive(Debug, Error)]
pub enum FacadeError {
    #[error("failed to open repo at {}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: Box<gix::open::Error>,
    },
    #[error("{operation}")]
    Operation {
        operation: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Result type for repository facade operations.
pub type Result<T> = std::result::Result<T, FacadeError>;

/// Open a `gix::Repository` by walking upward from the current
/// working directory.
///
/// Honours `GIT_DIR` via gitoxide's own discovery rules.
///
/// # Errors
///
/// Returns [`FacadeError`] when no repository is discovered from `cwd` or
/// configuration cannot be loaded.
pub fn open() -> Result<Repository> {
    let _span = gix_boundary!("open").entered();
    gix::open(".").map_err(|source| FacadeError::Open {
        path: PathBuf::from("."),
        source: Box::new(source),
    })
}

/// Open a `gix::Repository` rooted at `path`.
///
/// `path` may be a working tree root or a `.git` directory;
/// gitoxide resolves either shape via its own open logic.
///
/// # Errors
///
/// Returns [`FacadeError`] on open failure.
pub fn open_at(path: &Path) -> Result<Repository> {
    let _span = gix_boundary!("open_at").entered();
    gix::open(path).map_err(|source| FacadeError::Open {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Resolve a single revspec to its hex SHA.
///
/// Preserves the shape the `git rev-parse` shellouts had — input
/// string → output hex string → `None` on missing ref. Errors
/// other than "not found" propagate.
///
/// Use the typed [`Repository::rev_parse_single`] directly when the
/// caller wants an `Id` instead of a hex string.
pub fn rev_parse_hex(repo: &Repository, spec: &str) -> Result<Option<String>> {
    let _span = gix_boundary!("rev_parse_hex").entered();
    match repo.rev_parse_single(spec) {
        Ok(id) => Ok(Some(id.to_hex().to_string())),
        Err(err) => {
            // `rev_parse_single` surfaces "not found" as a variant
            // of its error enum; the idiomatic translation is
            // `Ok(None)` so call sites can pattern-match the
            // presence/absence of the ref rather than sniff the
            // error message. The shellout's `!status.success()`
            // branch had the same intent.
            let msg = err.to_string();
            if (msg.contains("object") && msg.contains("not") && msg.contains("found"))
                || msg.contains("unknown revision")
                || msg.contains("not a valid ref")
            {
                Ok(None)
            } else {
                Err(FacadeError::Operation {
                    operation: format!("rev_parse_single('{spec}') failed"),
                    source: Box::new(err),
                })
            }
        }
    }
}

/// Return the short branch name HEAD points to, or `None` when
/// detached.
///
/// Replaces the `git symbolic-ref HEAD` shellout pattern that
/// strips `refs/heads/` before use.
pub fn current_branch_name(repo: &Repository) -> Result<Option<String>> {
    let _span = gix_boundary!("current_branch_name").entered();
    let head_ref = repo.head_ref().map_err(|source| FacadeError::Operation {
        operation: "failed to read HEAD".to_owned(),
        source: Box::new(source),
    })?;
    Ok(head_ref.and_then(|r| {
        let name = r.name().as_bstr().to_string();
        name.strip_prefix("refs/heads/").map(|s| s.to_owned())
    }))
}

/// Return the commit timestamp of HEAD in Unix-epoch seconds.
///
/// Replaces the `git log -1 --format=%ct` shellout used by the VFS
/// daemon to stamp base files with HEAD's commit time.
pub fn head_commit_time(repo: &Repository) -> Result<Option<i64>> {
    let _span = gix_boundary!("head_commit_time").entered();
    let head_id = match repo.head_id() {
        Ok(id) => id,
        // Unborn HEAD (fresh bare clone that hasn't advertised a
        // branch yet) surfaces as an error; treat it as "no
        // timestamp available" rather than propagating.
        Err(_) => return Ok(None),
    };
    let commit = repo
        .find_commit(head_id)
        .map_err(|source| FacadeError::Operation {
            operation: "failed to find HEAD commit".to_owned(),
            source: Box::new(source),
        })?;
    let sig = commit
        .committer()
        .map_err(|source| FacadeError::Operation {
            operation: "failed to read HEAD committer".to_owned(),
            source: Box::new(source),
        })?;
    Ok(Some(sig.time().map(|t| t.seconds).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn open_error_retains_repository_path_and_source() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");

        let error = open_at(&missing).unwrap_err();

        assert!(matches!(
            &error,
            FacadeError::Open { path, .. } if path == &missing
        ));
        assert!(error.source().is_some());
    }
}
