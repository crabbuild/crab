//! Mount source detection: determines whether a user-provided input
//! refers to a remote repository (cloud-backed) or a local git repo.
//!
//! The [`MountSource`] enum is the first decision point in the mount
//! pipeline — it tells downstream code whether to perform a blobless
//! clone (remote) or use the local `.git` directory directly (local).

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::core::error::{CrabError, Result};

/// Remote URL schemes that indicate a cloud-backed repository.
const REMOTE_SCHEMES: &[&str] = &["crab://", "s3://", "gs://", "az://"];

/// Parsed mount source: either a remote URL or a local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A remote repository identified by a cloud URL.
    Remote {
        /// The original URL string (e.g. `crab://bucket/repo`).
        url: String,
    },
    /// A local git repository on the filesystem.
    Local {
        /// Absolute path to the repository root (parent of `.git`
        /// for non-bare repos, or the bare repo directory itself).
        path: PathBuf,
    },
}

/// Extracted bucket and prefix from a remote URL, suitable for
/// constructing a [`crate::StoreLayout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteComponents {
    /// Cloud storage bucket or container name.
    pub bucket: String,
    /// Repository prefix within the bucket (may be nested, e.g. `org/repo`).
    pub prefix: String,
}

impl MountSource {
    /// Parse a user-provided input string into a [`MountSource`].
    ///
    /// If the input starts with any of the recognized remote schemes
    /// (`crab://`, `s3://`, `gs://`, `az://`), it is treated as a
    /// remote URL. Otherwise it is treated as a local filesystem path
    /// and resolved to an absolute path.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(CrabError::Configuration {
                key: "mount source cannot be empty".into(),
                origin: String::new(),
            });
        }

        // Check for remote schemes.
        let lower = trimmed.to_ascii_lowercase();
        for scheme in REMOTE_SCHEMES {
            if lower.starts_with(scheme) {
                debug!(url = %trimmed, "detected remote mount source");
                return Ok(MountSource::Remote {
                    url: trimmed.to_owned(),
                });
            }
        }

        // Local path: resolve to absolute.
        let path = Path::new(trimmed);
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(CrabError::Io)?.join(path)
        };

        debug!(path = %abs_path.display(), "detected local mount source");
        Ok(MountSource::Local { path: abs_path })
    }

    /// Validate a local mount source.
    ///
    /// Checks that the path contains a valid git repository:
    /// - For standard repos: `.git/` directory or `.git` file (worktrees) must exist.
    /// - For bare repos: `objects/` directory must exist at the path.
    ///
    /// Returns the path to the git directory (`.git` for standard repos,
    /// or the bare repo path itself).
    pub fn validate_local(path: &Path) -> Result<PathBuf> {
        let git_dir = path.join(".git");

        if git_dir.is_dir() {
            // Standard repository with .git/ directory.
            debug!(git_dir = %git_dir.display(), "found .git directory");
            return Ok(git_dir);
        }

        if git_dir.is_file() {
            // Worktree: .git is a file containing `gitdir: <path>`.
            debug!(git_file = %git_dir.display(), "found .git file (worktree)");
            return resolve_gitdir_file(&git_dir);
        }

        // Check if this is a bare repository (has objects/ directly).
        let objects_dir = path.join("objects");
        if objects_dir.is_dir() {
            debug!(bare_repo = %path.display(), "found bare repository");
            return Ok(path.to_path_buf());
        }

        Err(CrabError::Configuration {
            key: "not a git repository: no .git/ directory, .git file, or objects/ found".into(),
            origin: path.display().to_string(),
        })
    }

    /// Validate a remote URL and extract bucket/prefix components.
    ///
    /// Ensures the URL has a valid format with a non-empty bucket and
    /// prefix, suitable for constructing a [`crate::StoreLayout`].
    pub fn validate_remote(url: &str) -> Result<RemoteComponents> {
        let parsed = crab_git::RepositoryUrl::parse(url)?;
        let bucket = parsed.bucket.to_ascii_lowercase();
        let prefix = parsed.repo_prefix;

        debug!(
            bucket = %bucket,
            prefix = %prefix,
            "validated remote URL components"
        );

        Ok(RemoteComponents { bucket, prefix })
    }
}

/// Resolve a `.git` file (worktree pointer) to the actual git directory.
fn resolve_gitdir_file(git_file: &Path) -> Result<PathBuf> {
    let content = std::fs::read_to_string(git_file).map_err(CrabError::Io)?;
    let trimmed = content.trim();

    let gitdir_path = trimmed
        .strip_prefix("gitdir: ")
        .ok_or_else(|| CrabError::Configuration {
            key: "invalid .git file: expected 'gitdir: <path>'".into(),
            origin: git_file.display().to_string(),
        })?;

    let resolved = if Path::new(gitdir_path).is_absolute() {
        PathBuf::from(gitdir_path)
    } else {
        // Relative to the directory containing the .git file.
        git_file
            .parent()
            .unwrap_or(Path::new("."))
            .join(gitdir_path)
    };

    if !resolved.is_dir() {
        return Err(CrabError::Configuration {
            key: format!("gitdir target does not exist: {}", resolved.display()),
            origin: git_file.display().to_string(),
        });
    }

    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    // --- MountSource::parse ---

    #[test]
    fn parse_crab_url_is_remote() {
        let src = MountSource::parse("crab://my-bucket/ml-models").unwrap();
        assert_eq!(
            src,
            MountSource::Remote {
                url: "crab://my-bucket/ml-models".into()
            }
        );
    }

    #[test]
    fn parse_s3_url_is_remote() {
        let src = MountSource::parse("s3://data-bucket/prefix/repo").unwrap();
        assert_eq!(
            src,
            MountSource::Remote {
                url: "s3://data-bucket/prefix/repo".into()
            }
        );
    }

    #[test]
    fn parse_gs_url_is_remote() {
        let src = MountSource::parse("gs://gcs-bucket/repo").unwrap();
        assert_eq!(
            src,
            MountSource::Remote {
                url: "gs://gcs-bucket/repo".into()
            }
        );
    }

    #[test]
    fn parse_az_url_is_remote() {
        let src = MountSource::parse("az://container/path").unwrap();
        assert_eq!(
            src,
            MountSource::Remote {
                url: "az://container/path".into()
            }
        );
    }

    #[test]
    fn parse_case_insensitive_scheme() {
        let src = MountSource::parse("CRAB://Bucket/Repo").unwrap();
        // Preserves original casing in the stored URL.
        assert_eq!(
            src,
            MountSource::Remote {
                url: "CRAB://Bucket/Repo".into()
            }
        );
    }

    #[test]
    fn parse_absolute_path_is_local() {
        let src = MountSource::parse("/home/user/my-repo").unwrap();
        assert_eq!(
            src,
            MountSource::Local {
                path: PathBuf::from("/home/user/my-repo")
            }
        );
    }

    #[test]
    fn parse_relative_path_is_local() {
        let src = MountSource::parse("./my-repo").unwrap();
        match src {
            MountSource::Local { path } => {
                assert!(path.is_absolute(), "expected absolute path, got: {path:?}");
                assert!(
                    path.to_string_lossy().ends_with("my-repo"),
                    "expected path ending with my-repo, got: {path:?}"
                );
            }
            other => panic!("expected Local, got: {other:?}"),
        }
    }

    #[test]
    fn parse_empty_input_errors() {
        let err = MountSource::parse("").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot be empty"), "got: {msg}");
    }

    #[test]
    fn parse_whitespace_only_errors() {
        let err = MountSource::parse("   ").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot be empty"), "got: {msg}");
    }

    // --- validate_local ---

    #[test]
    fn validate_local_standard_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path();
        std::fs::create_dir(repo_path.join(".git")).unwrap();

        let git_dir = MountSource::validate_local(repo_path).unwrap();
        assert_eq!(git_dir, repo_path.join(".git"));
    }

    #[test]
    fn validate_local_bare_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_path = tmp.path();
        std::fs::create_dir(bare_path.join("objects")).unwrap();

        let git_dir = MountSource::validate_local(bare_path).unwrap();
        assert_eq!(git_dir, bare_path);
    }

    #[test]
    fn validate_local_worktree_git_file() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        // Create the actual git dir that the .git file points to.
        let actual_git_dir = tmp.path().join("actual-git-dir");
        std::fs::create_dir_all(&actual_git_dir).unwrap();

        // Write .git file with gitdir pointer.
        let git_file = worktree_path.join(".git");
        std::fs::write(&git_file, format!("gitdir: {}", actual_git_dir.display())).unwrap();

        let result = MountSource::validate_local(&worktree_path).unwrap();
        assert_eq!(result, actual_git_dir);
    }

    #[test]
    fn validate_local_no_git_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = MountSource::validate_local(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not a git repository"), "got: {msg}");
    }

    // --- validate_remote ---

    #[test]
    fn validate_remote_crab_url() {
        let components = MountSource::validate_remote("crab://my-bucket/org/models").unwrap();
        assert_eq!(components.bucket, "my-bucket");
        assert_eq!(components.prefix, "org/models");
    }

    #[test]
    fn validate_remote_s3_url() {
        let components = MountSource::validate_remote("s3://data-bucket/prefix/repo").unwrap();
        assert_eq!(components.bucket, "data-bucket");
        assert_eq!(components.prefix, "prefix/repo");
    }

    #[test]
    fn validate_remote_gs_url() {
        let components = MountSource::validate_remote("gs://gcs-bucket/repo").unwrap();
        assert_eq!(components.bucket, "gcs-bucket");
        assert_eq!(components.prefix, "repo");
    }

    #[test]
    fn validate_remote_az_url() {
        let components = MountSource::validate_remote("az://container/path/to/repo").unwrap();
        assert_eq!(components.bucket, "container");
        assert_eq!(components.prefix, "path/to/repo");
    }

    #[test]
    fn validate_remote_normalizes_bucket_case() {
        let components = MountSource::validate_remote("s3://My-Bucket/Prefix").unwrap();
        assert_eq!(components.bucket, "my-bucket");
        // Prefix case is preserved (object keys are case-sensitive).
        assert_eq!(components.prefix, "Prefix");
    }

    #[test]
    fn validate_remote_strips_trailing_slashes() {
        let components = MountSource::validate_remote("crab://bucket/repo/path/").unwrap();
        assert_eq!(components.prefix, "repo/path");
    }

    #[test]
    fn validate_remote_missing_bucket_errors() {
        let err = MountSource::validate_remote("crab:///repo").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("include a bucket"), "got: {msg}");
    }

    #[test]
    fn validate_remote_missing_prefix_errors() {
        let err = MountSource::validate_remote("s3://bucket/").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("repo prefix"), "got: {msg}");
    }

    #[test]
    fn validate_remote_bucket_only_errors() {
        let err = MountSource::validate_remote("s3://bucket").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing repo path"), "got: {msg}");
    }

    #[test]
    fn validate_remote_unsupported_scheme_errors() {
        let err = MountSource::validate_remote("https://bucket/repo").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported repository URL scheme"),
            "got: {msg}"
        );
    }

    #[test]
    fn validate_remote_missing_separator_errors() {
        let err = MountSource::validate_remote("crab:bucket/repo").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing scheme separator"), "got: {msg}");
    }
}
