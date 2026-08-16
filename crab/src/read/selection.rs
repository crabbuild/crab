//! Snapshot path selection shared by download and export commands.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::core::error::{CrabError, Result};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::read::{DownloadEntry, SnapshotReader};

/// Behavior when neither paths nor include patterns are provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptySelection {
    /// Treat an empty selector as an error.
    Reject,
    /// Select every materializable file in the snapshot.
    All,
}

/// Path/include/exclude selector for a snapshot.
pub struct SnapshotSelection<'a> {
    /// Exact file paths or trailing-slash subtree selectors.
    pub paths: &'a [String],
    /// Include glob patterns.
    pub include: &'a [String],
    /// Exclude glob patterns.
    pub exclude: &'a [String],
    /// Empty-selector behavior.
    pub empty: EmptySelection,
    /// Command name used in configuration errors.
    pub origin: &'static str,
}

/// Select materializable entries from a resolved snapshot.
pub async fn select_snapshot_entries(
    snapshot: &SnapshotReader,
    selection: SnapshotSelection<'_>,
) -> Result<Vec<DownloadEntry>> {
    if selection.paths.is_empty()
        && selection.include.is_empty()
        && selection.empty == EmptySelection::Reject
    {
        return Err(CrabError::Configuration {
            key: "requires at least one path selector or --include pattern".to_owned(),
            origin: selection.origin.to_owned(),
        });
    }

    let selectors = normalize_path_selectors_for(selection.paths, selection.origin)?;
    let include_filter = if selection.include.is_empty() {
        None
    } else {
        Some(build_filter(selection.include, selection.exclude)?)
    };
    let exclude_gate = if selection.exclude.is_empty() {
        None
    } else {
        Some(build_filter(&[String::from("**/*")], selection.exclude)?)
    };
    let select_all = selection.paths.is_empty()
        && selection.include.is_empty()
        && selection.empty == EmptySelection::All;

    let needs_walk = select_all || !selectors.prefixes.is_empty() || include_filter.is_some();
    let mut selected = BTreeMap::<String, DownloadEntry>::new();

    for exact in &selectors.exact {
        let entry = snapshot.entry_for_path(exact).await?;
        if include_allowed(&entry.path, exclude_gate.as_ref()) {
            selected.insert(entry.path.clone(), entry);
        }
    }

    if needs_walk {
        for entry in snapshot.list_entries().await? {
            let matches_prefix = selectors
                .prefixes
                .iter()
                .any(|prefix| entry.path.starts_with(prefix));
            let matches_include = include_filter
                .as_ref()
                .is_some_and(|filter| filter.matches(&entry.path));
            if (select_all || matches_prefix || matches_include)
                && include_allowed(&entry.path, exclude_gate.as_ref())
            {
                selected.insert(entry.path.clone(), entry);
            }
        }
    }

    Ok(selected.into_values().collect())
}

fn include_allowed(path: &str, exclude_gate: Option<&PatternFilter>) -> bool {
    exclude_gate.is_none_or(|filter| filter.matches(path))
}

#[derive(Debug, Default)]
pub(crate) struct PathSelectors {
    pub(crate) exact: Vec<String>,
    pub(crate) prefixes: Vec<String>,
}

#[cfg(test)]
pub(crate) fn normalize_path_selectors(paths: &[String]) -> Result<PathSelectors> {
    normalize_path_selectors_for(paths, "path selector")
}

fn normalize_path_selectors_for(paths: &[String], origin: &str) -> Result<PathSelectors> {
    let mut selectors = PathSelectors::default();
    for raw in paths {
        let is_prefix = raw.trim().ends_with('/');
        let normalized = normalize_repo_path_for(raw, origin)?;
        if is_prefix {
            selectors.prefixes.push(format!("{normalized}/"));
        } else {
            selectors.exact.push(normalized);
        }
    }
    Ok(selectors)
}

/// Normalize a repo-relative path selector.
pub fn normalize_repo_path(raw: &str) -> Result<String> {
    normalize_repo_path_for(raw, "path selector")
}

fn normalize_repo_path_for(raw: &str, origin: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: "path selector cannot be empty".to_owned(),
            origin: origin.to_owned(),
        });
    }
    if trimmed.starts_with('/') {
        return Err(CrabError::Configuration {
            key: format!("path must be repo-relative: {raw}"),
            origin: origin.to_owned(),
        });
    }
    if trimmed.contains('\\') {
        return Err(CrabError::Configuration {
            key: format!("path must use forward slashes, got backslash in {raw}"),
            origin: origin.to_owned(),
        });
    }

    let mut components = Vec::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(value) => {
                let part = value.to_string_lossy();
                components.push(part.to_string());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(CrabError::Configuration {
                    key: format!("path cannot contain '..': {raw}"),
                    origin: origin.to_owned(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: format!("path must be repo-relative: {raw}"),
                    origin: origin.to_owned(),
                });
            }
        }
    }

    if components.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("path does not name a file or subtree: {raw}"),
            origin: origin.to_owned(),
        });
    }

    Ok(components.join("/"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn normalize_repo_path_rejects_escape() {
        let err = normalize_repo_path("../secret").unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn normalize_repo_path_collapses_current_dir() {
        assert_eq!(
            normalize_repo_path("./models/./a.bin").unwrap(),
            "models/a.bin"
        );
    }

    #[test]
    fn normalize_repo_path_rejects_absolute() {
        let err = normalize_repo_path("/tmp/a.bin").unwrap_err();
        assert!(err.to_string().contains("repo-relative"));
    }
}
