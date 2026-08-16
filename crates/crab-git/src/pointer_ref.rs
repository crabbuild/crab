//! Git ref to Crab pointer-map resolution.
//!
//! Resolves a git refspec (branch, tag, SHA, `HEAD~N`) to a commit, walks
//! its tree using `gix-traverse`, and extracts crab pointer blobs. The
//! result is a sorted map of file paths to parsed [`Pointer`] values.
//!
//! Non-pointer blobs (small files stored directly in git) are excluded
//! from the map — the caller reports them separately as "git-native".
//!
//! Follows the same `gix-odb` + `gix-traverse::tree::Recorder` pattern
//! used by `vfs/snapshot.rs::build_snapshot`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use gix_hash::ObjectId;
use gix_object::{Find, FindExt};
use tracing::{debug, warn};

use crab_types::pointer::{Pointer, is_pointer};

/// Result alias for pointer ref resolution.
pub type Result<T> = std::result::Result<T, PointerRefError>;

/// Errors returned while resolving a Git ref into Crab pointer blobs.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum PointerRefError {
    /// Spawning `git rev-parse` failed.
    #[error("failed to spawn git rev-parse for '{refspec}': {source}")]
    RevParseSpawn {
        refspec: String,
        #[source]
        source: std::io::Error,
    },

    /// The requested refspec was not present.
    #[error("git ref not found: '{refspec}'")]
    NotFound { refspec: String },

    /// `git rev-parse` returned something other than a full SHA-1.
    #[error("git rev-parse returned unexpected output for '{refspec}': {output}")]
    UnexpectedRevParseOutput { refspec: String, output: String },

    /// The Git object database directory was missing.
    #[error("git objects directory not found: {path}")]
    ObjectsDirNotFound { path: PathBuf },

    /// Opening the Git object database failed.
    #[error("failed to open git ODB at {path}: {source}")]
    OpenOdb {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The resolved commit object id was malformed.
    #[error("invalid commit OID {commit_hex}: {source}")]
    InvalidCommitOid {
        commit_hex: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Reading the resolved commit failed.
    #[error("failed to read commit {commit_hex}: {source}")]
    ReadCommit {
        commit_hex: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Parsing the tree id from the resolved commit failed.
    #[error("failed to parse tree from commit {commit_hex}: {source}")]
    ParseCommitTree {
        commit_hex: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Reading the resolved commit tree failed.
    #[error("failed to read tree {tree_id}: {source}")]
    ReadTree {
        tree_id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Walking the resolved commit tree failed.
    #[error("tree walk error: {source}")]
    TreeWalk {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Resolve a git refspec to a map of `(file_path → Pointer)` for all
/// crab-tracked files in that commit's tree.
///
/// Uses `git rev-parse` to resolve the refspec, then walks the commit's
/// tree with `gix-traverse` to find pointer blobs. Non-pointer blobs are
/// silently excluded.
///
/// When `path_filter` is `Some`, only paths matching at least one filter
/// entry (prefix match) are included.
///
/// # Errors
///
/// Returns [`PointerRefError`] if the refspec cannot be resolved or the ODB/tree
/// walk fails.
pub fn resolve_pointer_ref(
    git_dir: &Path,
    refspec: &str,
    path_filter: Option<&[String]>,
) -> Result<BTreeMap<String, Pointer>> {
    let commit_hex = rev_parse(git_dir, refspec)?;

    resolve_commit(git_dir, &commit_hex, path_filter)
}

/// Walk a resolved commit and collect Crab pointer blobs.
///
/// Use this when the caller already resolved the revision and only has an
/// object database, such as the SDK remote pack cache for URL-opened repos.
pub fn resolve_pointer_commit(
    git_dir: &Path,
    commit_hex: &str,
    path_filter: Option<&[String]>,
) -> Result<BTreeMap<String, Pointer>> {
    resolve_commit(git_dir, commit_hex, path_filter)
}

/// Resolve a refspec to a commit SHA hex string using `git rev-parse`.
fn rev_parse(git_dir: &Path, refspec: &str) -> Result<String> {
    let work_dir = git_dir.parent().unwrap_or(Path::new("."));

    let output = Command::new("git")
        .args(["rev-parse", "--verify", refspec])
        .current_dir(work_dir)
        .env("GIT_DIR", git_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|source| PointerRefError::RevParseSpawn {
            refspec: refspec.to_owned(),
            source,
        })?;

    if !output.status.success() {
        return Err(PointerRefError::NotFound {
            refspec: refspec.to_owned(),
        });
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.len() != 40 {
        return Err(PointerRefError::UnexpectedRevParseOutput {
            refspec: refspec.to_owned(),
            output: sha,
        });
    }

    Ok(sha)
}

/// Walk a commit's tree and collect pointer blobs into a `BTreeMap`.
fn resolve_commit(
    git_dir: &Path,
    commit_hex: &str,
    path_filter: Option<&[String]>,
) -> Result<BTreeMap<String, Pointer>> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(PointerRefError::ObjectsDirNotFound { path: objects_dir });
    }

    let odb = gix_odb::at(&objects_dir).map_err(|source| PointerRefError::OpenOdb {
        path: objects_dir.clone(),
        source: boxed_source(source),
    })?;

    let commit_oid = ObjectId::from_hex(commit_hex.as_bytes()).map_err(|source| {
        PointerRefError::InvalidCommitOid {
            commit_hex: commit_hex.to_owned(),
            source: boxed_source(source),
        }
    })?;

    // Read the commit to get its tree.
    let tree_id = {
        let mut buf = Vec::new();
        let mut commit_iter = odb
            .find_commit_iter(&commit_oid, &mut buf)
            .map_err(|source| PointerRefError::ReadCommit {
                commit_hex: commit_hex.to_owned(),
                source: boxed_source(source),
            })?;
        commit_iter
            .tree_id()
            .map_err(|source| PointerRefError::ParseCommitTree {
                commit_hex: commit_hex.to_owned(),
                source: boxed_source(source),
            })?
    };

    // Walk the tree using gix-traverse's Recorder to get paths.
    let mut buf = Vec::new();
    let tree_iter =
        odb.find_tree_iter(&tree_id, &mut buf)
            .map_err(|source| PointerRefError::ReadTree {
                tree_id: tree_id.to_string(),
                source: boxed_source(source),
            })?;

    let mut recorder = gix_traverse::tree::Recorder::default();
    let mut state = gix_traverse::tree::breadthfirst::State::default();

    gix_traverse::tree::breadthfirst(tree_iter, &mut state, &odb, &mut recorder).map_err(
        |source| PointerRefError::TreeWalk {
            source: boxed_source(source),
        },
    )?;

    let mut pointers = BTreeMap::new();

    for entry in &recorder.records {
        use gix_object::bstr::ByteSlice;

        let kind = entry.mode.kind();

        // Skip non-file entries (trees, submodules, symlinks).
        if !matches!(
            kind,
            gix_object::tree::EntryKind::Blob | gix_object::tree::EntryKind::BlobExecutable
        ) {
            continue;
        }

        let path = entry.filepath.to_str_lossy().into_owned();

        // Apply path filter if specified.
        if let Some(filters) = path_filter
            && !matches_path_filter(&path, filters)
        {
            continue;
        }

        // Read the blob to check for pointer content.
        let mut blob_buf = Vec::new();
        match odb.try_find(&entry.oid, &mut blob_buf) {
            Ok(Some(data)) if data.kind == gix_object::Kind::Blob => {
                if is_pointer(data.data) {
                    match Pointer::parse(data.data) {
                        Ok(ptr) => {
                            pointers.insert(path, ptr);
                        }
                        Err(e) => {
                            warn!(
                                path = %path,
                                error = %e,
                                "blob looks like pointer but failed to parse"
                            );
                        }
                    }
                }
                // Non-pointer blobs are silently excluded — caller
                // handles them as git-native.
            }
            Ok(Some(_)) => {
                warn!(path = %path, "non-blob entry in tree walk");
            }
            Ok(None) => {
                warn!(path = %path, "blob not found in ODB");
            }
            Err(e) => {
                warn!(path = %path, error = %e, "failed to read blob");
            }
        }
    }

    debug!(
        refspec = %commit_hex,
        pointer_count = pointers.len(),
        "ref resolution complete"
    );

    Ok(pointers)
}

fn boxed_source<E>(source: E) -> Box<dyn std::error::Error + Send + Sync>
where
    E: std::error::Error + Send + Sync + 'static,
{
    Box::new(source)
}

/// Check whether a path matches any of the filter patterns.
///
/// Uses simple prefix matching: a filter "models/" matches "models/v1/weights.bin",
/// and an exact filter "model.bin" matches "model.bin".
fn matches_path_filter(path: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    for filter in filters {
        if path == filter || path.starts_with(filter) {
            return true;
        }
        // Support trailing slash for directory prefix matching.
        let prefix = filter.strip_suffix('/').unwrap_or(filter);
        if path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/') {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn filter_exact_match() {
        assert!(matches_path_filter("model.bin", &["model.bin".to_owned()]));
    }

    #[test]
    fn filter_prefix_match() {
        assert!(matches_path_filter(
            "models/v1/weights.bin",
            &["models/".to_owned()]
        ));
    }

    #[test]
    fn filter_directory_prefix_without_trailing_slash() {
        assert!(matches_path_filter(
            "models/v1/weights.bin",
            &["models".to_owned()]
        ));
    }

    #[test]
    fn filter_no_match() {
        assert!(!matches_path_filter(
            "data/file.bin",
            &["models/".to_owned()]
        ));
    }

    #[test]
    fn filter_empty_allows_all() {
        assert!(matches_path_filter("anything.bin", &[]));
    }

    #[test]
    fn filter_multiple_patterns() {
        let filters = vec!["models/".to_owned(), "data/".to_owned()];
        assert!(matches_path_filter("models/v1/w.bin", &filters));
        assert!(matches_path_filter("data/train.csv", &filters));
        assert!(!matches_path_filter("src/main.rs", &filters));
    }

    // --- resolve_ref integration test (requires git) ---

    #[test]
    fn resolve_ref_on_real_repo_with_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo_dir)
            .status();

        // Write a valid crab pointer.
        let ptr = Pointer {
            file_hash: [0xab; 32],
            size: 42,
            shard_hint: None,
        };
        std::fs::write(repo_dir.join("data.bin"), ptr.serialize()).unwrap();

        // Write a non-pointer file.
        std::fs::write(repo_dir.join("readme.txt"), b"hello world\n").unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let git_dir = repo_dir.join(".git");

        let result = resolve_pointer_ref(&git_dir, "HEAD", None).unwrap();

        // Only the pointer file should be in the map.
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("data.bin"));
        assert_eq!(result["data.bin"].size, 42);
    }

    #[test]
    fn resolve_ref_with_path_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo_dir)
            .status();

        // Create directory structure with pointers.
        std::fs::create_dir_all(repo_dir.join("models")).unwrap();
        std::fs::create_dir_all(repo_dir.join("data")).unwrap();

        let ptr1 = Pointer {
            file_hash: [0x01; 32],
            size: 100,
            shard_hint: None,
        };
        let ptr2 = Pointer {
            file_hash: [0x02; 32],
            size: 200,
            shard_hint: None,
        };

        std::fs::write(repo_dir.join("models/weights.bin"), ptr1.serialize()).unwrap();
        std::fs::write(repo_dir.join("data/train.bin"), ptr2.serialize()).unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let git_dir = repo_dir.join(".git");
        let filters = vec!["models".to_owned()];

        let result = resolve_pointer_ref(&git_dir, "HEAD", Some(&filters)).unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains_key("models/weights.bin"));
    }

    #[test]
    fn resolve_ref_invalid_ref_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let git_dir = repo_dir.join(".git");

        let err = resolve_pointer_ref(&git_dir, "nonexistent-ref", None).unwrap_err();

        match err {
            PointerRefError::NotFound { refspec } => {
                assert_eq!(refspec, "nonexistent-ref");
            }
            other => panic!("expected NotFound, got: {other}"),
        }
    }
}
