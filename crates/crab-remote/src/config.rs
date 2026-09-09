//! Narrow repository configuration used by local clone consumers.

use serde::Deserialize;

const PROJECT_CONFIG_VERSION: u32 = 1;

/// Repository checkout mode from the committed `crab.toml` file.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HydrationMode {
    /// Leave tracked content dehydrated after checkout.
    Lazy,
    /// Materialize tracked content during checkout.
    Eager,
}

/// The hydration projection consumed while cloning a repository.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryHydration {
    default: HydrationMode,
    auto_patterns: Option<Vec<String>>,
}

impl RepositoryHydration {
    /// Construct the checkout projection from an already validated project configuration.
    #[must_use]
    pub fn new(default: HydrationMode, auto_patterns: Option<Vec<String>>) -> Self {
        Self {
            default,
            auto_patterns,
        }
    }
}

/// Resolved hydration policy for the first checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutHydration {
    lazy: bool,
    patterns: Vec<String>,
}

impl CheckoutHydration {
    /// Return whether the first checkout must leave unmatched content dehydrated.
    #[must_use]
    pub const fn lazy(&self) -> bool {
        self.lazy
    }

    /// Return repository-relative patterns to hydrate after the lazy checkout.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

#[derive(Deserialize)]
struct ProjectCheckoutDocument {
    #[serde(default = "default_project_config_version")]
    version: u32,
    hydrate: Option<RepositoryHydration>,
}

const fn default_project_config_version() -> u32 {
    PROJECT_CONFIG_VERSION
}

/// Failure to read the checkout projection from a committed project file.
#[derive(Debug, thiserror::Error)]
pub enum ProjectConfigurationError {
    /// The project file is not valid TOML for the consumed checkout fields.
    #[error("failed to parse committed crab.toml: {0}")]
    Parse(#[from] toml::de::Error),
    /// The project file uses a configuration version this client cannot interpret.
    #[error("unsupported crab.toml version {actual}; expected {expected}")]
    Version { expected: u32, actual: u32 },
}

/// Parse only the clone hydration fields from a committed `crab.toml` document.
///
/// Other project sections remain owned by their consumers. The version and the
/// complete hydration section are validated before any checkout policy is used.
pub fn parse_repository_hydration(
    document: &str,
) -> Result<Option<RepositoryHydration>, ProjectConfigurationError> {
    let document: ProjectCheckoutDocument = toml::from_str(document)?;
    if document.version != PROJECT_CONFIG_VERSION {
        return Err(ProjectConfigurationError::Version {
            expected: PROJECT_CONFIG_VERSION,
            actual: document.version,
        });
    }
    Ok(document.hydrate)
}

/// Merge explicit clone inputs with committed repository hydration policy.
///
/// An explicit hydration mode wins over the repository file. Explicit patterns
/// win over repository patterns and require a lazy first checkout so unmatched
/// paths stay dehydrated.
#[must_use]
pub fn resolve_checkout_hydration(
    explicit_lazy: Option<bool>,
    explicit_patterns: &[String],
    repository: Option<&RepositoryHydration>,
) -> CheckoutHydration {
    if !explicit_patterns.is_empty() {
        return CheckoutHydration {
            lazy: true,
            patterns: explicit_patterns.to_vec(),
        };
    }
    if let Some(lazy) = explicit_lazy {
        return CheckoutHydration {
            lazy,
            patterns: Vec::new(),
        };
    }
    let Some(repository) = repository else {
        return CheckoutHydration {
            lazy: true,
            patterns: Vec::new(),
        };
    };
    let patterns = repository.auto_patterns.clone().unwrap_or_default();
    CheckoutHydration {
        lazy: !patterns.is_empty() || repository.default == HydrationMode::Lazy,
        patterns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_eager_mode_overrides_clone_default() {
        let repository =
            parse_repository_hydration("version = 1\n[hydrate]\ndefault = \"eager\"\n")
                .unwrap()
                .unwrap();

        assert_eq!(
            resolve_checkout_hydration(None, &[], Some(&repository)),
            CheckoutHydration {
                lazy: false,
                patterns: Vec::new(),
            }
        );
    }

    #[test]
    fn explicit_lazy_mode_overrides_repository_eager_mode_and_patterns() {
        let repository =
            RepositoryHydration::new(HydrationMode::Eager, Some(vec!["models/**".to_owned()]));

        assert_eq!(
            resolve_checkout_hydration(Some(true), &[], Some(&repository)),
            CheckoutHydration {
                lazy: true,
                patterns: Vec::new(),
            }
        );
    }

    #[test]
    fn repository_patterns_select_a_lazy_checkout_and_post_checkout_hydration() {
        let repository =
            RepositoryHydration::new(HydrationMode::Eager, Some(vec!["models/**".to_owned()]));

        assert_eq!(
            resolve_checkout_hydration(None, &[], Some(&repository)),
            CheckoutHydration {
                lazy: true,
                patterns: vec!["models/**".to_owned()],
            }
        );
    }

    #[test]
    fn malformed_consumed_fields_and_unknown_hydration_keys_are_rejected() {
        for document in [
            "version = 2\n[hydrate]\ndefault = \"lazy\"\n",
            "[hydrate]\ndefault = \"sometimes\"\n",
            "[hydrate]\ndefault = \"lazy\"\nunknown = true\n",
        ] {
            assert!(parse_repository_hydration(document).is_err());
        }
    }
}
