//! CLI-facing prefetch profile adapter.
//!
//! Shared prefetch profiles live in the committed `crab.toml`. This module
//! compiles their path patterns for hydrate callers.

use std::collections::BTreeMap;
use std::path::Path;

use globset::Glob;

use crate::core::Result;

/// Named prefetch profiles with validated glob patterns.
#[derive(Debug)]
pub struct PrefetchConfig {
    pub profiles: BTreeMap<String, Vec<Glob>>,
}

impl PrefetchConfig {
    /// Return the patterns for a named profile.
    pub fn profile(&self, name: &str) -> Result<&[Glob]> {
        self.profiles.get(name).map(Vec::as_slice).ok_or_else(|| {
            crate::core::error::CrabError::PrefetchProfileNotFound {
                name: name.to_owned(),
            }
        })
    }
}

/// Load shared prefetch profiles from `crab.toml` at the given repo root.
///
/// Returns an empty [`PrefetchConfig`] if the file does not exist. Returns an
/// error for invalid TOML, unsupported schema versions, or invalid glob
/// patterns.
pub fn load_prefetch(repo_root: &Path) -> Result<PrefetchConfig> {
    let Some(project) = crate::core::project_config::ProjectConfig::load_for_repo(repo_root)?
    else {
        return Ok(PrefetchConfig {
            profiles: BTreeMap::new(),
        });
    };
    let Some(prefetch) = project.prefetch else {
        return Ok(PrefetchConfig {
            profiles: BTreeMap::new(),
        });
    };

    let mut profiles = BTreeMap::new();
    for (name, profile) in prefetch.profiles {
        let globs = profile
            .paths
            .into_iter()
            .map(|pattern| {
                Glob::new(&pattern).map_err(|error| crab_cache::CacheError::PrefetchParse {
                    reason: format!(
                        "invalid glob in prefetch profile '{name}': pattern '{pattern}': {error}"
                    ),
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        profiles.insert(name, globs);
    }
    Ok(PrefetchConfig { profiles })
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
        let project_path = dir.path().join("crab.toml");
        std::fs::write(
            project_path,
            "[remote]\nurl = \"crab://bucket/repo\"\n\n[prefetch.profiles.always]\npaths = [\"*.md\"]\n",
        )
        .unwrap();

        let config = load_prefetch(dir.path()).unwrap();
        assert_eq!(config.profiles.len(), 1);
        assert!(config.profiles.contains_key("always"));
    }
}
