//! Git worktree compatibility helpers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::Serialize;
use thiserror::Error;

use crate::discover::resolve_common_dir;

/// Errors returned by Git worktree discovery and porcelain parsing.
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to discover Git worktree from {}", path.display())]
    Discover {
        path: PathBuf,
        #[source]
        source: gix_discover::upwards::Error,
    },
    #[error("{0}")]
    Protocol(String),
}

/// Result type for Git worktree helpers.
pub type Result<T> = std::result::Result<T, WorktreeError>;

pub const REQUIRED_COMPATIBILITY_FLOOR: &str = "2.39.x";
pub const TRACKED_LATEST_MANUAL_VERSION: &str = "2.54.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeCommandSurface {
    pub subcommand: &'static str,
    pub options: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionGatedOption {
    pub subcommand: &'static str,
    pub option: &'static str,
    pub introduced_by_manual: &'static str,
}

pub const GIT_2_39_WORKTREE_SURFACE: &[WorktreeCommandSurface] = &[
    WorktreeCommandSurface {
        subcommand: "add",
        options: &[
            "-f",
            "--force",
            "-B",
            "--detach",
            "-d",
            "--checkout",
            "--no-checkout",
            "--lock",
            "--reason",
            "-b",
            "-q",
            "--quiet",
            "--track",
            "--guess-remote",
        ],
    },
    WorktreeCommandSurface {
        subcommand: "list",
        options: &["-v", "--porcelain", "-z"],
    },
    WorktreeCommandSurface {
        subcommand: "lock",
        options: &["--reason"],
    },
    WorktreeCommandSurface {
        subcommand: "move",
        options: &["-f", "--force"],
    },
    WorktreeCommandSurface {
        subcommand: "prune",
        options: &["-n", "--dry-run", "-v", "--expire"],
    },
    WorktreeCommandSurface {
        subcommand: "remove",
        options: &["-f", "--force"],
    },
    WorktreeCommandSurface {
        subcommand: "repair",
        options: &[],
    },
    WorktreeCommandSurface {
        subcommand: "unlock",
        options: &[],
    },
];

pub const LATEST_TRACKED_VERSION_GATED_OPTIONS: &[VersionGatedOption] = &[
    VersionGatedOption {
        subcommand: "add",
        option: "--orphan",
        introduced_by_manual: TRACKED_LATEST_MANUAL_VERSION,
    },
    VersionGatedOption {
        subcommand: "add",
        option: "--relative-paths",
        introduced_by_manual: TRACKED_LATEST_MANUAL_VERSION,
    },
    VersionGatedOption {
        subcommand: "add",
        option: "--no-relative-paths",
        introduced_by_manual: TRACKED_LATEST_MANUAL_VERSION,
    },
    VersionGatedOption {
        subcommand: "repair",
        option: "--relative-paths",
        introduced_by_manual: TRACKED_LATEST_MANUAL_VERSION,
    },
    VersionGatedOption {
        subcommand: "repair",
        option: "--no-relative-paths",
        introduced_by_manual: TRACKED_LATEST_MANUAL_VERSION,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeContext {
    pub current_worktree_root: PathBuf,
    pub main_worktree_root: PathBuf,
    pub common_git_dir: PathBuf,
    pub per_worktree_git_dir: PathBuf,
    pub identity: String,
}

impl WorktreeContext {
    pub fn resolve() -> Result<Self> {
        Self::resolve_from(Path::new("."))
    }

    pub fn resolve_from(start: &Path) -> Result<Self> {
        if let Some(ctx) = Self::resolve_from_git_env(start)? {
            return Ok(ctx);
        }

        Self::resolve_from_path(start)
    }

    /// Resolves the Git worktree containing `start` without consulting Git environment overrides.
    pub fn resolve_from_path(start: &Path) -> Result<Self> {
        let (repo_path, _trust) =
            gix_discover::upwards(start).map_err(|source| WorktreeError::Discover {
                path: start.to_path_buf(),
                source,
            })?;
        let (git_dir, work_tree) = repo_path.into_repository_and_work_tree_directories();
        let Some(current_worktree_root) = work_tree else {
            return Err(WorktreeError::Protocol(format!(
                "{} is not inside a Git working tree",
                start.display()
            )));
        };

        let per_worktree_git_dir = normalize_existing_path(&git_dir);
        let common_git_dir = normalize_existing_path(&resolve_common_dir(&per_worktree_git_dir));
        let Some(main_worktree_root) = common_git_dir.parent().map(Path::to_path_buf) else {
            return Err(WorktreeError::Protocol(format!(
                "Git common directory has no parent: {}",
                common_git_dir.display()
            )));
        };
        let identity = if same_path(&per_worktree_git_dir, &common_git_dir) {
            "main".to_owned()
        } else {
            per_worktree_git_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    WorktreeError::Protocol(format!(
                        "linked worktree git directory has no identity: {}",
                        per_worktree_git_dir.display()
                    ))
                })?
        };
        Ok(Self {
            current_worktree_root: normalize_existing_path(&current_worktree_root),
            main_worktree_root: normalize_existing_path(&main_worktree_root),
            common_git_dir,
            per_worktree_git_dir,
            identity,
        })
    }

    fn resolve_from_git_env(start: &Path) -> Result<Option<Self>> {
        let Some(git_dir) = non_empty_env_path("GIT_DIR") else {
            return Ok(None);
        };
        let Some(work_tree) = non_empty_env_path("GIT_WORK_TREE") else {
            return Ok(None);
        };

        let current_worktree_root = resolve_env_path(start, &work_tree);
        let per_worktree_git_dir = normalize_existing_path(&resolve_env_path(start, &git_dir));
        let common_git_dir = non_empty_env_path("GIT_COMMON_DIR").map_or_else(
            || normalize_existing_path(&resolve_common_dir(&per_worktree_git_dir)),
            |common_dir| normalize_existing_path(&resolve_env_path(start, &common_dir)),
        );
        let Some(main_worktree_root) = common_git_dir.parent().map(Path::to_path_buf) else {
            return Err(WorktreeError::Protocol(format!(
                "Git common directory has no parent: {}",
                common_git_dir.display()
            )));
        };
        let identity = if same_path(&per_worktree_git_dir, &common_git_dir) {
            "main".to_owned()
        } else {
            per_worktree_git_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    WorktreeError::Protocol(format!(
                        "linked worktree git directory has no identity: {}",
                        per_worktree_git_dir.display()
                    ))
                })?
        };
        Ok(Some(Self {
            current_worktree_root: normalize_existing_path(&current_worktree_root),
            main_worktree_root: normalize_existing_path(&main_worktree_root),
            common_git_dir,
            per_worktree_git_dir,
            identity,
        }))
    }

    pub fn index_path(&self) -> PathBuf {
        self.per_worktree_git_dir.join("index")
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.common_git_dir.join("objects")
    }

    pub fn lfs_objects_dir(&self) -> PathBuf {
        self.common_git_dir.join("lfs").join("objects")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: Option<u16>,
    pub original: String,
}

impl GitVersion {
    pub fn parse(text: &str) -> Option<Self> {
        let original = text.trim().to_owned();
        let version = original.strip_prefix("git version ")?;
        let mut parts = version.split(|c: char| !c.is_ascii_digit() && c != '.');
        let numeric = parts.next()?;
        let mut numeric_parts = numeric.split('.');
        let major = numeric_parts.next()?.parse().ok()?;
        let minor = numeric_parts.next()?.parse().ok()?;
        let patch = numeric_parts.next().and_then(|p| p.parse().ok());
        Some(Self {
            major,
            minor,
            patch,
            original,
        })
    }

    pub fn is_at_least(&self, major: u16, minor: u16) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PorcelainField {
    pub key: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GitWorktreeRecord {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_reason: Option<String>,
    pub prunable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prune_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<PorcelainField>,
}

impl GitWorktreeRecord {
    fn new(path: String) -> Self {
        Self {
            path,
            head: None,
            branch: None,
            detached: false,
            bare: false,
            locked: false,
            lock_reason: None,
            prunable: false,
            prune_reason: None,
            extra: Vec::new(),
        }
    }
}

pub fn installed_git_version() -> Result<Option<GitVersion>> {
    let output = Command::new("git").arg("--version").output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(GitVersion::parse(&stdout))
}

pub fn parse_worktree_list_porcelain(
    input: &[u8],
    nul_terminated: bool,
) -> Result<Vec<GitWorktreeRecord>> {
    let fields = split_porcelain_fields(input, nul_terminated);
    let mut records = Vec::new();
    let mut current: Option<GitWorktreeRecord> = None;

    for field in fields {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }

        let line = String::from_utf8_lossy(field);
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(record) = current.replace(GitWorktreeRecord::new(path.to_owned())) {
                records.push(record);
            }
            continue;
        }

        let Some(record) = current.as_mut() else {
            return Err(WorktreeError::Protocol(
                "git worktree porcelain field appeared before worktree path".to_owned(),
            ));
        };

        apply_porcelain_field(record, &line);
    }

    if let Some(record) = current {
        records.push(record);
    }

    Ok(records)
}

pub fn linked_identity_map_from_current_repo() -> Result<HashMap<String, String>> {
    let Some(common_dir) = git_common_dir(Path::new("."))? else {
        return Ok(HashMap::new());
    };
    linked_identity_map(&common_dir)
}

pub fn worktree_identity_for_path(path: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir", "--git-common-dir"])
        .current_dir(path)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let Some(git_dir_text) = lines.next() else {
        return Ok(None);
    };
    let Some(common_dir_text) = lines.next() else {
        return Ok(None);
    };

    let git_dir = resolve_git_path(path, git_dir_text);
    let common_dir = resolve_git_path(path, common_dir_text);
    if same_path(&git_dir, &common_dir) {
        return Ok(Some("main".to_owned()));
    }

    Ok(git_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

fn split_porcelain_fields(input: &[u8], nul_terminated: bool) -> Vec<&[u8]> {
    let delimiter = if nul_terminated { b'\0' } else { b'\n' };
    input
        .split(|byte| *byte == delimiter)
        .map(trim_trailing_cr)
        .collect()
}

fn trim_trailing_cr(field: &[u8]) -> &[u8] {
    field.strip_suffix(b"\r").unwrap_or(field)
}

fn apply_porcelain_field(record: &mut GitWorktreeRecord, line: &str) {
    if let Some(head) = line.strip_prefix("HEAD ") {
        record.head = Some(head.to_owned());
    } else if let Some(branch) = line.strip_prefix("branch ") {
        record.branch = Some(branch.to_owned());
    } else if line == "detached" {
        record.detached = true;
    } else if line == "bare" {
        record.bare = true;
    } else if line == "locked" {
        record.locked = true;
    } else if let Some(reason) = line.strip_prefix("locked ") {
        record.locked = true;
        record.lock_reason = Some(reason.to_owned());
    } else if line == "prunable" {
        record.prunable = true;
    } else if let Some(reason) = line.strip_prefix("prunable ") {
        record.prunable = true;
        record.prune_reason = Some(reason.to_owned());
    } else {
        let (key, value) = line.split_once(' ').map_or_else(
            || (line.to_owned(), None),
            |(key, value)| (key.to_owned(), Some(value.to_owned())),
        );
        record.extra.push(PorcelainField { key, value });
    }
}

fn git_common_dir(path: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().next() else {
        return Ok(None);
    };
    Ok(Some(resolve_git_path(path, line)))
}

fn linked_identity_map(common_dir: &Path) -> Result<HashMap<String, String>> {
    let mut identities = HashMap::new();
    let worktrees_dir = common_dir.join("worktrees");
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(identities),
        Err(e) => return Err(WorktreeError::Io(e)),
    };

    for entry in entries {
        let entry = entry.map_err(WorktreeError::Io)?;
        let admin_dir = entry.path();
        let gitdir_file = admin_dir.join("gitdir");
        let Ok(gitdir_text) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let Some(worktree_path) = worktree_path_from_gitdir_file(&admin_dir, gitdir_text.trim())
        else {
            continue;
        };
        identities.insert(
            normalize_identity_path(&worktree_path),
            entry.file_name().to_string_lossy().into_owned(),
        );
    }

    Ok(identities)
}

fn worktree_path_from_gitdir_file(admin_dir: &Path, value: &str) -> Option<PathBuf> {
    let gitfile_path = resolve_git_path(admin_dir, value);
    if gitfile_path
        .file_name()
        .is_some_and(|name| name == std::ffi::OsStr::new(".git"))
    {
        return gitfile_path.parent().map(Path::to_path_buf);
    }
    None
}

fn resolve_git_path(worktree_path: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        worktree_path.join(path)
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn resolve_env_path(start: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        start.join(value)
    }
}

pub fn normalize_identity_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_version_with_platform_suffix() {
        let parsed = GitVersion::parse("git version 2.39.3 (Apple Git-146)").expect("parse");
        assert_eq!(parsed.major, 2);
        assert_eq!(parsed.minor, 39);
        assert_eq!(parsed.patch, Some(3));
        assert!(parsed.is_at_least(2, 39));
    }

    #[test]
    fn compatibility_matrix_tracks_newer_manual_options() {
        let options: Vec<&str> = LATEST_TRACKED_VERSION_GATED_OPTIONS
            .iter()
            .map(|option| option.option)
            .collect();
        assert!(options.contains(&"--orphan"));
        assert!(options.contains(&"--relative-paths"));
        assert!(options.contains(&"--no-relative-paths"));
    }

    #[test]
    fn parses_porcelain_records() {
        let input = b"worktree /repo\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /linked\nHEAD 2222222222222222222222222222222222222222\ndetached\nlocked testing\n\n";
        let records = parse_worktree_list_porcelain(input, false).expect("parse porcelain");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/repo");
        assert_eq!(records[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(records[1].detached);
        assert!(records[1].locked);
        assert_eq!(records[1].lock_reason.as_deref(), Some("testing"));
    }

    #[test]
    fn parses_nul_porcelain_records() {
        let input = b"worktree /repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /linked\0HEAD 2222222222222222222222222222222222222222\0detached\0\0";
        let records = parse_worktree_list_porcelain(input, true).expect("parse porcelain");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/repo");
        assert_eq!(records[1].path, "/linked");
        assert!(records[1].detached);
    }

    #[test]
    fn linked_identity_map_resolves_relative_gitdir_files_from_admin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let common_dir = tmp.path().join("repo").join(".git");
        let admin_dir = common_dir.join("worktrees").join("linked");
        let linked = tmp.path().join("linked");

        std::fs::create_dir_all(&admin_dir).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            "gitdir: ../repo/.git/worktrees/linked\n",
        )
        .unwrap();
        std::fs::write(admin_dir.join("gitdir"), "../../../../linked/.git\n").unwrap();

        let identities = linked_identity_map(&common_dir).expect("identity map");
        let linked_key = normalize_identity_path(&linked);

        assert_eq!(
            identities.get(&linked_key).map(String::as_str),
            Some("linked")
        );
    }
}
