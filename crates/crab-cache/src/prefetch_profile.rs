//! Prefetch profile parser for `.crab/prefetch.toml`.
//!
//! A prefetch config declares named sets of glob patterns that control cache
//! warming and eager hydration decisions.

use std::collections::BTreeMap;
use std::path::Path;

use globset::Glob;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{CacheError, Result};

/// File name of the prefetch profile config inside a repo's shared `.crab/`.
pub const PREFETCH_TOML_FILE: &str = "prefetch.toml";

/// Parsed prefetch configuration with profiles indexed by name.
///
/// Profiles are stored in a [`BTreeMap`] for deterministic iteration order.
#[derive(Debug)]
pub struct PrefetchConfig {
    pub profiles: BTreeMap<String, Vec<Glob>>,
}

impl PrefetchConfig {
    /// Returns the glob patterns for `name`.
    pub fn profile(&self, name: &str) -> Result<&[Glob]> {
        self.profiles.get(name).map(Vec::as_slice).ok_or_else(|| {
            CacheError::PrefetchProfileNotFound {
                name: name.to_owned(),
            }
        })
    }
}

/// Loads `prefetch.toml` from the given shared `.crab/` directory.
pub fn load_prefetch_from_crab_dir(crab_dir: &Path) -> Result<PrefetchConfig> {
    load_prefetch_path(&crab_dir.join(PREFETCH_TOML_FILE))
}

/// Loads and parses a prefetch config file.
///
/// A missing file yields an empty config because prefetch profiles are
/// optional. Malformed TOML, unsupported versions, and invalid glob patterns
/// are hard errors.
pub fn load_prefetch_path(path: &Path) -> Result<PrefetchConfig> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "prefetch.toml not found, using empty config");
            return Ok(PrefetchConfig {
                profiles: BTreeMap::new(),
            });
        }
        Err(error) => return Err(CacheError::Io(error)),
    };

    parse_prefetch(&contents)
}

/// Parses a prefetch TOML string.
pub fn parse_prefetch(contents: &str) -> Result<PrefetchConfig> {
    const SUPPORTED_VERSION: u32 = 1;

    let raw: RawPrefetchFile =
        toml::from_str(contents).map_err(|error| CacheError::PrefetchParse {
            reason: error.to_string(),
        })?;

    if raw.version != SUPPORTED_VERSION {
        return Err(CacheError::PrefetchParse {
            reason: format!(
                "unsupported version {}, expected {SUPPORTED_VERSION}",
                raw.version
            ),
        });
    }

    let mut profiles = BTreeMap::new();

    for entry in &raw.profile {
        let mut globs = Vec::with_capacity(entry.paths.len());
        for pattern in &entry.paths {
            let glob = Glob::new(pattern).map_err(|error| CacheError::PrefetchParse {
                reason: format!(
                    "invalid glob in profile '{}': pattern '{}': {}",
                    entry.name, pattern, error
                ),
            })?;
            globs.push(glob);
        }

        if profiles.contains_key(&entry.name) {
            warn!(
                profile = %entry.name,
                "duplicate profile name in prefetch.toml, last definition wins"
            );
        }

        debug!(
            profile = %entry.name,
            patterns = globs.len(),
            "parsed prefetch profile"
        );
        profiles.insert(entry.name.clone(), globs);
    }

    debug!(profiles = profiles.len(), "loaded prefetch config");
    Ok(PrefetchConfig { profiles })
}

#[derive(Deserialize)]
struct RawPrefetchFile {
    version: u32,
    #[serde(default)]
    profile: Vec<RawProfile>,
}

#[derive(Deserialize)]
struct RawProfile {
    name: String,
    paths: Vec<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn valid_config_parses() {
        let toml = r#"
version = 1

[[profile]]
name = "always"
paths = [
    "README.md",
    "docs/**/*.md",
    "*.toml",
    "src/**/*.rs",
]

[[profile]]
name = "ci"
paths = ["tests/fixtures/small/**"]
"#;
        let config = parse_prefetch(toml).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert!(config.profiles.contains_key("always"));
        assert!(config.profiles.contains_key("ci"));
        assert_eq!(config.profiles["always"].len(), 4);
        assert_eq!(config.profiles["ci"].len(), 1);
    }

    #[test]
    fn empty_profiles_list() {
        let config = parse_prefetch("version = 1\n").unwrap();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn missing_file_returns_empty_config() {
        let config = load_prefetch_path(Path::new("/nonexistent/repo/root")).unwrap();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = parse_prefetch("this is not valid toml {{{}}}");
        assert!(matches!(result, Err(CacheError::PrefetchParse { .. })));
    }

    #[test]
    fn unknown_version_returns_error() {
        let result = parse_prefetch("version = 99\n");
        assert!(matches!(result, Err(CacheError::PrefetchParse { .. })));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported version 99")
        );
    }

    #[test]
    fn invalid_glob_returns_error() {
        let toml = r#"
version = 1

[[profile]]
name = "bad"
paths = ["[invalid"]
"#;
        let result = parse_prefetch(toml);
        assert!(matches!(result, Err(CacheError::PrefetchParse { .. })));
        let message = result.unwrap_err().to_string();
        assert!(message.contains("bad"), "error should mention profile name");
        assert!(
            message.contains("[invalid"),
            "error should mention the pattern"
        );
    }

    #[test]
    fn profiles_are_sorted_by_name() {
        let toml = r#"
version = 1

[[profile]]
name = "zebra"
paths = ["z/**"]

[[profile]]
name = "alpha"
paths = ["a/**"]
"#;
        let config = parse_prefetch(toml).unwrap();
        let names: Vec<&String> = config.profiles.keys().collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn duplicate_profile_name_last_wins() {
        let toml = r#"
version = 1

[[profile]]
name = "dup"
paths = ["first/**"]

[[profile]]
name = "dup"
paths = ["second/**"]
"#;
        let config = parse_prefetch(toml).unwrap();
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles["dup"].len(), 1);
        assert_eq!(config.profiles["dup"][0].glob(), "second/**");
    }

    #[test]
    fn load_from_crab_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PREFETCH_TOML_FILE),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.md\"]\n",
        )
        .unwrap();

        let config = load_prefetch_from_crab_dir(dir.path()).unwrap();
        assert_eq!(config.profiles.len(), 1);
        assert!(config.profiles.contains_key("always"));
    }

    #[test]
    fn profile_with_empty_paths() {
        let toml = r#"
version = 1

[[profile]]
name = "empty"
paths = []
"#;
        let config = parse_prefetch(toml).unwrap();
        assert_eq!(config.profile("empty").unwrap().len(), 0);
    }

    #[test]
    fn profile_lookup_reports_missing_name() {
        let config = parse_prefetch("version = 1\n").unwrap();
        assert!(matches!(
            config.profile("missing"),
            Err(CacheError::PrefetchProfileNotFound { name }) if name == "missing"
        ));
    }

    #[test]
    fn version_zero_rejected() {
        let result = parse_prefetch("version = 0\n");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported version 0")
        );
    }

    #[test]
    fn missing_version_field_is_error() {
        let toml = r#"
[[profile]]
name = "no-version"
paths = ["*.rs"]
"#;
        assert!(matches!(
            parse_prefetch(toml),
            Err(CacheError::PrefetchParse { .. })
        ));
    }
}
