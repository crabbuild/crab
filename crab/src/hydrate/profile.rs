//! CLI-facing prefetch profile adapter.
//!
//! The `.crab/prefetch.toml` format and parser live in `crab-cache`; this
//! module preserves the existing `CrabError`-shaped hydrate Interface and the
//! linked-worktree path resolution used by CLI callers.

use std::path::{Path, PathBuf};

pub use crab_cache::prefetch_profile::PrefetchConfig;

use crate::core::Result;

const PREFETCH_TOML_PATH: &str = ".crab/prefetch.toml";

/// Load `.crab/prefetch.toml` from the given repo root.
///
/// Returns an empty [`PrefetchConfig`] if the file does not exist. Returns an
/// error for invalid TOML, unsupported schema versions, or invalid glob
/// patterns.
pub fn load_prefetch(repo_root: &Path) -> Result<PrefetchConfig> {
    let path = prefetch_config_path(repo_root);
    crab_cache::load_prefetch_path(&path).map_err(Into::into)
}

fn prefetch_config_path(repo_root: &Path) -> PathBuf {
    crate::git::worktree::WorktreeContext::resolve_from_path(repo_root).map_or_else(
        |_| repo_root.join(PREFETCH_TOML_PATH),
        |ctx| ctx.shared_crab_dir.join(crab_cache::PREFETCH_TOML_FILE),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty_config() {
        let config = load_prefetch(Path::new("/nonexistent/repo/root")).unwrap();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let crab_dir = dir.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        std::fs::write(
            crab_dir.join("prefetch.toml"),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.md\"]\n",
        )
        .unwrap();

        let config = load_prefetch(dir.path()).unwrap();
        assert_eq!(config.profiles.len(), 1);
        assert!(config.profiles.contains_key("always"));
    }
}
