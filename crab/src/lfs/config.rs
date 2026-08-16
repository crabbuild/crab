//! LFS configuration resolution.
//!
//! Reads LFS settings from environment variables, `.lfsconfig`, and
//! `.gitconfig` with a defined precedence order. Provides defaults for
//! transfer concurrency, fetch behavior, retry policy, and storage paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};

/// Resolved LFS configuration.
///
/// Built by layering defaults ← `.gitconfig` ← `.lfsconfig` ← env vars.
/// Higher-priority sources override lower ones on a per-key basis.
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
    /// Resolve LFS configuration from all sources.
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables (`GIT_LFS_*`)
    /// 2. `.lfsconfig` in the repository root
    /// 3. `.gitconfig` (local `.git/config` → global `~/.gitconfig` → system `/etc/gitconfig`)
    /// 4. Compiled defaults
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] when a config file is
    /// malformed or a value is out of range.
    pub fn resolve(repo_root: &Path) -> Result<Self> {
        let mut config = Self::default();

        // Layer 3: .gitconfig (system → global → local, each overriding the previous)
        let gitconfig_paths = gitconfig_paths(repo_root);
        for path in &gitconfig_paths {
            if path.is_file() {
                let values = parse_ini_lfs_section(path)?;
                config.apply_ini_values(&values, &path.display().to_string())?;
            }
        }

        // Layer 2: .lfsconfig (overrides .gitconfig)
        let lfsconfig_path = repo_root.join(".lfsconfig");
        if lfsconfig_path.is_file() {
            let values = parse_ini_lfs_section(&lfsconfig_path)?;
            config.apply_ini_values(&values, &lfsconfig_path.display().to_string())?;
        }

        // Layer 1: environment variables (highest priority)
        config.apply_env();

        // Validate ranges.
        config.validate()?;

        Ok(config)
    }

    /// Apply values parsed from an INI-style config file's `[lfs]` section.
    fn apply_ini_values(&mut self, values: &HashMap<String, String>, origin: &str) -> Result<()> {
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

// ---------------------------------------------------------------------------
// INI parsing helpers
// ---------------------------------------------------------------------------

/// Collect the paths to `.gitconfig` files in system → global → local order.
///
/// Each successive file overrides the previous, so local wins over global
/// wins over system — matching git's own precedence within the gitconfig layer.
fn gitconfig_paths(repo_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(3);

    // System: /etc/gitconfig
    let system = PathBuf::from("/etc/gitconfig");
    paths.push(system);

    // Global: ~/.gitconfig
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".gitconfig"));
    }

    // Local: .git/config
    paths.push(repo_root.join(".git/config"));

    paths
}

/// Parse an INI-style config file and extract keys from the `[lfs]` section.
///
/// Returns a map of lowercased key → value. Supports subsections like
/// `[lfs "transfer"]` which produce keys prefixed with `transfer.`.
///
/// This is intentionally minimal — it handles the subset of git-config
/// syntax needed for LFS settings without pulling in a full INI parser.
fn parse_ini_lfs_section(path: &Path) -> Result<HashMap<String, String>> {
    let content = std::fs::read_to_string(path).map_err(|e| CrabError::Configuration {
        key: format!("failed to read file: {e}"),
        origin: path.display().to_string(),
    })?;

    let mut result = HashMap::new();
    let mut in_lfs_section = false;
    let mut subsection_prefix: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        // Section header.
        if trimmed.starts_with('[') {
            let (is_lfs, prefix) = parse_section_header(trimmed);
            in_lfs_section = is_lfs;
            subsection_prefix = prefix;
            continue;
        }

        if !in_lfs_section {
            continue;
        }

        // Key = value line.
        if let Some((key, value)) = parse_key_value(trimmed) {
            let full_key = match &subsection_prefix {
                Some(prefix) => format!("{prefix}.{key}"),
                None => key,
            };
            result.insert(full_key, value);
        }
    }

    Ok(result)
}

/// Parse a section header line like `[lfs]` or `[lfs "transfer"]`.
///
/// Returns `(is_lfs_section, optional_subsection_prefix)`.
fn parse_section_header(line: &str) -> (bool, Option<String>) {
    // Strip brackets.
    let inner = line.trim_start_matches('[').trim_end_matches(']').trim();

    // Check for subsection: [lfs "transfer"]
    if let Some((section, subsection)) = inner.split_once('"') {
        let section = section.trim().to_lowercase();
        let subsection = subsection.trim_end_matches('"').trim();
        if section == "lfs" {
            return (true, Some(subsection.to_lowercase()));
        }
        return (false, None);
    }

    let section = inner.to_lowercase();
    if section == "lfs" {
        (true, None)
    } else {
        (false, None)
    }
}

/// Parse a `key = value` or `key=value` line.
///
/// Returns `(lowercased_key, trimmed_value)`.
fn parse_key_value(line: &str) -> Option<(String, String)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim().to_lowercase();
    let value = value.trim().to_string();
    Some((key, value))
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
    fn gitconfig_overridden_by_lfsconfig() {
        let dir = tempfile::tempdir().unwrap();

        // Create .git/config with one value.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let gitconfig = dir.path().join(".git/config");
        let mut f = std::fs::File::create(&gitconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 2").unwrap();
        writeln!(f, "    pruneoffsetdays = 10").unwrap();
        drop(f);

        // Create .lfsconfig overriding concurrent_transfers.
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    concurrenttransfers = 16").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        // .lfsconfig wins for concurrent_transfers.
        assert_eq!(config.concurrent_transfers, 16);
        // .gitconfig value preserved for pruneoffsetdays.
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
        let lfsconfig = dir.path().join(".lfsconfig");
        let mut f = std::fs::File::create(&lfsconfig).unwrap();
        writeln!(f, "[lfs]").unwrap();
        writeln!(f, "    lfsdir = /custom/lfs/path").unwrap();
        drop(f);

        let config = LfsConfig::resolve(dir.path()).unwrap();
        assert_eq!(config.lfs_dir, Some(PathBuf::from("/custom/lfs/path")));
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
    fn parse_section_header_variants() {
        assert_eq!(parse_section_header("[lfs]"), (true, None));
        assert_eq!(parse_section_header("[LFS]"), (true, None));
        assert_eq!(
            parse_section_header("[lfs \"transfer\"]"),
            (true, Some("transfer".to_string()))
        );
        assert_eq!(parse_section_header("[core]"), (false, None));
        assert_eq!(parse_section_header("[remote \"origin\"]"), (false, None));
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
