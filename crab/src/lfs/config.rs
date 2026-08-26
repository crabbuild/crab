//! LFS configuration resolution.
//!
//! Reads LFS settings from environment variables, `.lfsconfig`, and Git's
//! canonical config resolver with a defined precedence order. Provides defaults for
//! transfer concurrency, fetch behavior, retry policy, and storage paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Resolved LFS configuration.
///
/// Built by layering defaults ← `.lfsconfig` ← Git config ← env vars.
/// Higher-priority sources override lower ones on a per-key basis. Storage
/// redirection is ignored in tracked `.lfsconfig` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsConfig {
    /// Maximum concurrent upload/download transfers (1–100).
    pub concurrent_transfers: u32,
    /// Days of recent refs to include when fetching.
    pub fetch_recent_refs_days: u32,
    /// Days of recent commits within refs to include when fetching.
    pub fetch_recent_commits_days: u32,
    /// Days to retain objects beyond the latest ref before pruning.
    pub prune_offset_days: u32,
    /// Path-based include filter for fetch operations.
    pub fetch_include: Option<String>,
    /// Path-based exclude filter for fetch operations.
    pub fetch_exclude: Option<String>,
    /// Maximum retry attempts for failed transfers.
    pub transfer_max_retries: u32,
    /// Maximum delay in seconds between transfer retries.
    pub transfer_max_retry_delay: u32,
    /// When true, continue on download errors instead of aborting.
    pub skip_download_errors: bool,
    /// Override for the default `.git/lfs` storage directory.
    pub lfs_dir: Option<PathBuf>,
    /// Maximum bandwidth in bytes/second for transfers (0 = unlimited).
    pub transfer_max_bandwidth: u64,
}

impl Default for LfsConfig {
    fn default() -> Self {
        Self {
            concurrent_transfers: 8,
            fetch_recent_refs_days: 7,
            fetch_recent_commits_days: 0,
            prune_offset_days: 3,
            fetch_include: None,
            fetch_exclude: None,
            transfer_max_retries: 8,
            transfer_max_retry_delay: 10,
            skip_download_errors: false,
            lfs_dir: None,
            transfer_max_bandwidth: 0,
        }
    }
}

impl LfsConfig {
    /// Resolve the local LFS storage root against the common Git directory.
    #[must_use]
    pub fn storage_dir(&self, common_git_dir: &Path) -> PathBuf {
        match self.lfs_dir.as_ref() {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => common_git_dir.join(path),
            None => common_git_dir.join("lfs"),
        }
    }

    /// Resolve the configured local LFS storage directory for a repository.
    ///
    /// Git's LFS storage is shared by linked worktrees, so relative storage
    /// paths are resolved against the common Git directory rather than the
    /// current worktree's private Git directory.
    pub(crate) fn resolve_storage_dir(repo_root: &Path) -> Result<PathBuf> {
        let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)?;
        let config = Self::resolve(&worktree.current_worktree_root)?;
        Ok(config.storage_dir(&worktree.common_git_dir))
    }

    /// Resolve LFS configuration from all sources.
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables (`GIT_LFS_*`)
    /// 2. Git's effective config, including its normal local/global/system
    ///    precedence, includes, and conditional includes
    /// 3. `.lfsconfig` in the repository root, except storage redirection
    /// 4. Compiled defaults
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] when a config file is
    /// malformed or a value is out of range.
    pub fn resolve(repo_root: &Path) -> Result<Self> {
        let mut config = Self::default();

        // `.lfsconfig` is lower precedence than Git config. Do not follow
        // includes from a tracked file into arbitrary local paths.
        let lfsconfig_path = repo_root.join(".lfsconfig");
        if lfsconfig_path.is_file() {
            let mut values = git_config_values(repo_root, Some(&lfsconfig_path), false)?;
            // A tracked file must not redirect local reads or destructive
            // prune operations outside this repository's Git directory.
            values.remove("storage");
            values.remove("lfsdir");
            config.apply_values(&values, &lfsconfig_path.display().to_string())?;
        }

        // Delegate precedence, includes, conditional includes, XDG config,
        // worktree config, and platform-specific locations to Git itself.
        let values = git_config_values(repo_root, None, true)?;
        config.apply_values(&values, "git config")?;

        // Environment variables have the highest priority.
        config.apply_env();

        // Validate ranges.
        config.validate()?;

        Ok(config)
    }

    /// Apply values from Git config's `lfs.*` namespace.
    fn apply_values(&mut self, values: &HashMap<String, String>, origin: &str) -> Result<()> {
        if let Some(v) = values.get("concurrenttransfers") {
            self.concurrent_transfers = parse_u32(v, "lfs.concurrenttransfers", origin)?;
        }
        if let Some(v) = values.get("fetchrecentrefsdays") {
            self.fetch_recent_refs_days = parse_u32(v, "lfs.fetchrecentrefsdays", origin)?;
        }
        if let Some(v) = values.get("fetchrecentcommitsdays") {
            self.fetch_recent_commits_days = parse_u32(v, "lfs.fetchrecentcommitsdays", origin)?;
        }
        if let Some(v) = values.get("pruneoffsetdays") {
            self.prune_offset_days = parse_u32(v, "lfs.pruneoffsetdays", origin)?;
        }
        if let Some(v) = values.get("fetchinclude") {
            self.fetch_include = Some(v.clone());
        }
        if let Some(v) = values.get("fetchexclude") {
            self.fetch_exclude = Some(v.clone());
        }
        if let Some(v) = values.get("transfer.maxretries") {
            self.transfer_max_retries = parse_u32(v, "lfs.transfer.maxretries", origin)?;
        }
        if let Some(v) = values.get("transfer.maxretrydelay") {
            self.transfer_max_retry_delay = parse_u32(v, "lfs.transfer.maxretrydelay", origin)?;
        }
        if let Some(v) = values.get("skipdownloaderrors") {
            self.skip_download_errors = parse_bool(v, "lfs.skipdownloaderrors", origin)?;
        }
        if let Some(v) = values.get("lfsdir") {
            self.lfs_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = values.get("storage") {
            self.lfs_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = values.get("transfer.maxbandwidth") {
            self.transfer_max_bandwidth = parse_u64(v, "lfs.transfer.maxbandwidth", origin)?;
        }
        Ok(())
    }

    /// Apply environment variable overrides (highest priority).
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("GIT_LFS_CONCURRENT_TRANSFERS")
            && let Some(n) = parse_env_u32("GIT_LFS_CONCURRENT_TRANSFERS", &v)
        {
            self.concurrent_transfers = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_FETCH_RECENT_REFS_DAYS")
            && let Some(n) = parse_env_u32("GIT_LFS_FETCH_RECENT_REFS_DAYS", &v)
        {
            self.fetch_recent_refs_days = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_FETCH_RECENT_COMMITS_DAYS")
            && let Some(n) = parse_env_u32("GIT_LFS_FETCH_RECENT_COMMITS_DAYS", &v)
        {
            self.fetch_recent_commits_days = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_PRUNE_OFFSET_DAYS")
            && let Some(n) = parse_env_u32("GIT_LFS_PRUNE_OFFSET_DAYS", &v)
        {
            self.prune_offset_days = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_FETCH_INCLUDE") {
            self.fetch_include = Some(v);
        }
        if let Ok(v) = std::env::var("GIT_LFS_FETCH_EXCLUDE") {
            self.fetch_exclude = Some(v);
        }
        if let Ok(v) = std::env::var("GIT_LFS_TRANSFER_MAX_RETRIES")
            && let Some(n) = parse_env_u32("GIT_LFS_TRANSFER_MAX_RETRIES", &v)
        {
            self.transfer_max_retries = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_TRANSFER_MAX_RETRY_DELAY")
            && let Some(n) = parse_env_u32("GIT_LFS_TRANSFER_MAX_RETRY_DELAY", &v)
        {
            self.transfer_max_retry_delay = n;
        }
        if let Ok(v) = std::env::var("GIT_LFS_SKIP_DOWNLOAD_ERRORS") {
            match v.as_str() {
                "1" | "true" | "yes" => self.skip_download_errors = true,
                "0" | "false" | "no" => self.skip_download_errors = false,
                _ => tracing::warn!(
                    key = "GIT_LFS_SKIP_DOWNLOAD_ERRORS",
                    value = %v,
                    "ignoring unrecognized boolean value, using default",
                ),
            }
        }
        if let Ok(v) = std::env::var("GIT_LFS_DIR") {
            self.lfs_dir = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("GIT_LFS_TRANSFER_MAX_BANDWIDTH")
            && let Some(n) = parse_env_u64("GIT_LFS_TRANSFER_MAX_BANDWIDTH", &v)
        {
            self.transfer_max_bandwidth = n;
        }
    }

    /// Validate that all values are within acceptable ranges.
    fn validate(&self) -> Result<()> {
        if self.concurrent_transfers < 1 || self.concurrent_transfers > 100 {
            return Err(CrabError::Configuration {
                key: format!(
                    "lfs.concurrenttransfers: {} is out of range 1–100",
                    self.concurrent_transfers
                ),
                origin: "resolved LFS config".into(),
            });
        }
        Ok(())
    }
}

fn parse_env_u32(key: &'static str, value: &str) -> Option<u32> {
    if let Ok(n) = value.parse::<u32>() {
        return Some(n);
    }
    tracing::warn!(
        key,
        value = %value,
        "ignoring invalid environment variable value, using default",
    );
    None
}

fn parse_env_u64(key: &'static str, value: &str) -> Option<u64> {
    if let Ok(n) = value.parse::<u64>() {
        return Some(n);
    }
    tracing::warn!(
        key,
        value = %value,
        "ignoring invalid environment variable value, using default",
    );
    None
}

/// Read `lfs.*` values through Git's config parser.
///
/// `file` is used for the tracked `.lfsconfig` source. It is intentionally
/// not loaded with `--includes`; effective repository config uses Git's normal
/// includes and conditional includes so Crab does not implement a second,
/// subtly different config language.
fn git_config_values(
    repo_root: &Path,
    file: Option<&Path>,
    includes: bool,
) -> Result<HashMap<String, String>> {
    let mut command = Command::new("git");
    command.current_dir(repo_root).arg("config").arg("--null");
    if includes {
        command.arg("--includes");
    }
    if let Some(file) = file {
        command.arg("--file").arg(file);
    }
    let output = command
        .arg("--get-regexp")
        .arg(r"^lfs\.")
        .output()
        .map_err(|error| CrabError::Configuration {
            key: "git config".to_owned(),
            origin: format!("failed to execute Git: {error}"),
        })?;

    // `--get-regexp` exits 1 when no matching key exists. That is a normal
    // empty configuration, not a failure to resolve the repository config.
    if !output.status.success() {
        if output.stdout.is_empty() && output.stderr.is_empty() {
            return Ok(HashMap::new());
        }
        return Err(CrabError::Configuration {
            key: "git config".to_owned(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let mut values = HashMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(separator) = record.iter().position(|byte| *byte == b'\n') else {
            return Err(CrabError::Configuration {
                key: "git config".to_owned(),
                origin: "Git returned a malformed NUL-delimited LFS config record".to_owned(),
            });
        };
        let (key, value) = record.split_at(separator);
        let value = &value[1..];
        let key = std::str::from_utf8(key).map_err(|error| CrabError::Configuration {
            key: "git config".to_owned(),
            origin: format!("Git returned a non-UTF-8 LFS config key: {error}"),
        })?;
        let value = std::str::from_utf8(value).map_err(|error| CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("Git returned a non-UTF-8 LFS config value: {error}"),
        })?;
        let Some(key) = key.strip_prefix("lfs.") else {
            continue;
        };
        values.insert(key.to_owned(), value.to_owned());
    }
    Ok(values)
}

/// Parse a string as `u32`, returning a config error on failure.
fn parse_u32(s: &str, key: &str, origin: &str) -> Result<u32> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| CrabError::Configuration {
            key: format!("{key}: invalid integer \"{s}\""),
            origin: origin.to_string(),
        })
}

/// Parse a string as `u64`, returning a config error on failure.
fn parse_u64(s: &str, key: &str, origin: &str) -> Result<u64> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| CrabError::Configuration {
            key: format!("{key}: invalid integer \"{s}\""),
            origin: origin.to_string(),
        })
}

/// Parse a git-config boolean value (`true`/`yes`/`on`/`1` → true).
fn parse_bool(s: &str, key: &str, origin: &str) -> Result<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(CrabError::Configuration {
            key: format!("{key}: invalid boolean \"{s}\""),
            origin: origin.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        assert!(
            Command::new("git")
                .current_dir(dir)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn defaults_are_correct() {
        let config = LfsConfig::default();
        assert_eq!(config.concurrent_transfers, 8);
        assert_eq!(config.fetch_recent_refs_days, 7);
        assert_eq!(config.fetch_recent_commits_days, 0);
        assert_eq!(config.prune_offset_days, 3);
        assert!(config.fetch_include.is_none());
        assert!(config.fetch_exclude.is_none());
        assert_eq!(config.transfer_max_retries, 8);
        assert_eq!(config.transfer_max_retry_delay, 10);
        assert!(!config.skip_download_errors);
        assert!(config.lfs_dir.is_none());
        assert_eq!(config.transfer_max_bandwidth, 0);
    }

    #[test]
    fn resolve_with_no_config_files_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config, LfsConfig::default());
    }

    #[test]
    fn lfsconfig_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 4").unwrap();
        writeln!(f, "    fetchrecentrefsdays = 14").unwrap();
        writeln!(f, "    skipdownloaderrors = true").unwrap();
        writeln!(f, "    fetchinclude = *.bin").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.concurrent_transfers, 4);
        assert_eq!(config.fetch_recent_refs_days, 14);
        assert!(config.skip_download_errors);
        assert_eq!(config.fetch_include.as_deref(), Some("*.bin"));
    }

    #[test]
    fn gitconfig_overrides_lfsconfig() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        // Create .git/config with one value.
        let gitconfig = dir.path().join(".git/config");
        let mut f = std::fs::File::create(&gitconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 2").unwrap();
        writeln!(f, "    pruneoffsetdays = 10").unwrap();
        drop(f);

        // `.lfsconfig` has lower precedence than the effective Git config.
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 16").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.concurrent_transfers, 2);
        assert_eq!(config.prune_offset_days, 10);
    }

    #[test]
    fn transfer_subsection_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs \"transfer\"]").unwrap();
        writeln!(f, "    maxretries = 12").unwrap();
        writeln!(f, "    maxretrydelay = 30").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.transfer_max_retries, 12);
        assert_eq!(config.transfer_max_retry_delay, 30);
    }

    #[test]
    fn concurrent_transfers_out_of_range_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 200").unwrap();
        drop(f);

        let err = LfsConfig::resolve(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("out of range"), "unexpected error: {msg}");
    }

    #[test]
    fn invalid_integer_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = abc").unwrap();
        drop(f);

        let err = LfsConfig::resolve(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid integer"), "unexpected error: {msg}");
    }

    #[test]
    fn invalid_boolean_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    skipdownloaderrors = maybe").unwrap();
        drop(f);

        let err = LfsConfig::resolve(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid boolean"), "unexpected error: {msg}");
    }

    #[test]
    fn lfs_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let lfsconfig = dir.path().join(".git/config");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    lfsdir = /custom/lfs/path").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.lfs_dir, Some(PathBuf::from("/custom/lfs/path")));
    }

    #[test]
    fn standard_lfs_storage_override() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let lfsconfig = dir.path().join(".git/config");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    storage = relative-lfs-cache").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();

        assert_eq!(config.lfs_dir, Some(PathBuf::from("relative-lfs-cache")));
        assert_eq!(
            config.storage_dir(Path::new("/repo/.git")),
            PathBuf::from("/repo/.git/relative-lfs-cache")
        );
    }

    #[test]
    fn resolve_storage_dir_uses_repository_lfs_storage() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let gitconfig = dir.path().join(".git/config");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(gitconfig)
            .unwrap();
        writeln!(file, "[lfs]").unwrap();
        writeln!(file, "    storage = custom-lfs-cache").unwrap();

        assert_eq!(
            LfsConfig::resolve_storage_dir(dir.path()).unwrap(),
            dir.path()
                .canonicalize()
                .unwrap()
                .join(".git/custom-lfs-cache")
        );
    }

    #[test]
    fn tracked_lfsconfig_cannot_redirect_storage() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    storage = /outside/repository").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();

        assert!(config.lfs_dir.is_none());
    }

    #[test]
    fn comments_and_empty_lines_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "# This is a comment").unwrap();
        writeln!(f, "").unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "; another comment").unwrap();
        writeln!(f, "    concurrenttransfers = 5").unwrap();
        writeln!(f, "").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.concurrent_transfers, 5);
    }

    #[test]
    fn non_lfs_sections_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[core]").unwrap();
        writeln!(f, "    autocrlf = true").unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 3").unwrap();
        writeln!(f, "[remote \"origin\"]").unwrap();
        writeln!(f, "    url = https://example.com").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.concurrent_transfers, 3);
    }

    #[test]
    fn parse_bool_variants() {
        assert!(parse_bool("true", "k", "o").unwrap());
        assert!(parse_bool("yes", "k", "o").unwrap());
        assert!(parse_bool("on", "k", "o").unwrap());
        assert!(parse_bool("1", "k", "o").unwrap());
        assert!(!parse_bool("false", "k", "o").unwrap());
        assert!(!parse_bool("no", "k", "o").unwrap());
        assert!(!parse_bool("off", "k", "o").unwrap());
        assert!(!parse_bool("0", "k", "o").unwrap());
        assert!(parse_bool("maybe", "k", "o").is_err());
    }
}
