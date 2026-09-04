//! Path filtering for Git LFS fetch and smudge operations.

use crate::core::error::Result;
use crate::lfs::batch::PatternFilter;
use crate::lfs::config::LfsConfig;

/// Compiled `lfs.fetchinclude` / `lfs.fetchexclude` path filter.
pub struct FetchPathFilter {
    include: Option<PatternFilter>,
    exclude: Option<PatternFilter>,
}

impl FetchPathFilter {
    /// Builds a filter from resolved LFS configuration.
    ///
    /// Returns `None` when neither include nor exclude filtering is configured.
    pub fn from_config(config: &LfsConfig) -> Result<Option<Self>> {
        Self::from_patterns(
            config.fetch_include.as_deref(),
            config.fetch_exclude.as_deref(),
        )
    }

    /// Builds a filter from raw comma-separated include/exclude patterns.
    ///
    /// Returns `None` when neither include nor exclude filtering is configured.
    pub fn from_patterns(include: Option<&str>, exclude: Option<&str>) -> Result<Option<Self>> {
        let include = compile_fetch_filter(include)?;
        let exclude = compile_fetch_filter(exclude)?;
        if include.is_none() && exclude.is_none() {
            return Ok(None);
        }

        Ok(Some(Self { include, exclude }))
    }

    /// Returns whether a path should be smudged/fetched.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        if let Some(include) = &self.include
            && !include.matches(path)
        {
            return false;
        }

        if let Some(exclude) = &self.exclude
            && exclude.matches(path)
        {
            return false;
        }

        true
    }
}

/// Returns whether a path passes the given raw LFS fetch filters.
pub fn path_allowed_by_fetch_filters(
    path: &str,
    include: Option<&str>,
    exclude: Option<&str>,
) -> Result<bool> {
    Ok(FetchPathFilter::from_patterns(include, exclude)?
        .as_ref()
        .is_none_or(|filter| filter.allows(path)))
}

pub(crate) fn compile_fetch_filter(patterns: Option<&str>) -> Result<Option<PatternFilter>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    let normalized = normalize_fetch_filter_patterns(patterns);
    // Git LFS uses an empty value to clear a restriction. An empty generic
    // path matcher selects nothing, which would suppress every download.
    if normalized.is_empty() {
        return Ok(None);
    }
    PatternFilter::new(&normalized).map(Some)
}

fn normalize_fetch_filter_patterns(patterns: &str) -> String {
    patterns
        .split(',')
        .flat_map(normalize_fetch_filter_pattern)
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_fetch_filter_pattern(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Vec::new();
    }

    let root_relative = pattern.strip_prefix('/').unwrap_or(pattern);
    let trimmed = root_relative.trim_end_matches('/');
    if trimmed.is_empty() {
        return vec!["**/*".to_owned()];
    }

    if has_glob_metachar(trimmed) || trimmed.ends_with("/**") {
        return vec![trimmed.to_owned()];
    }

    vec![trimmed.to_owned(), format!("{trimmed}/**")]
}

fn has_glob_metachar(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patterns_clear_only_the_selected_restriction() {
        assert!(
            FetchPathFilter::from_patterns(Some(""), Some(""))
                .unwrap()
                .is_none()
        );
        let filter = FetchPathFilter::from_patterns(Some(""), Some("private"))
            .unwrap()
            .unwrap();
        assert!(filter.allows("public/asset.bin"));
        assert!(!filter.allows("private/asset.bin"));
        let filter = FetchPathFilter::from_patterns(Some("public"), Some(""))
            .unwrap()
            .unwrap();
        assert!(filter.allows("public/asset.bin"));
        assert!(!filter.allows("private/asset.bin"));
    }

    #[test]
    fn fetch_filters_allow_matching_include() {
        assert!(path_allowed_by_fetch_filters("foo/a.dat", Some("foo/**"), None).unwrap());
        assert!(!path_allowed_by_fetch_filters("bar/a.dat", Some("foo/**"), None).unwrap());
    }

    #[test]
    fn fetch_filters_reject_matching_exclude() {
        assert!(!path_allowed_by_fetch_filters("a.dat", None, Some("a*")).unwrap());
        assert!(path_allowed_by_fetch_filters("b.dat", None, Some("a*")).unwrap());
    }

    #[test]
    fn fetch_filters_apply_include_before_exclude() {
        assert!(
            !path_allowed_by_fetch_filters("foo/bar/a.dat", Some("foo/**"), Some("foo/bar/**"))
                .unwrap()
        );
        assert!(!path_allowed_by_fetch_filters("a.dat", Some("foo/**"), Some("a*")).unwrap());
    }

    #[test]
    fn fetch_filters_support_root_relative_directory_prefixes() {
        assert!(path_allowed_by_fetch_filters("foo/a.dat", Some("/foo"), None).unwrap());
        assert!(
            !path_allowed_by_fetch_filters("foo/bar/a.dat", Some("/foo"), Some("/foo/bar"))
                .unwrap()
        );
    }

    #[test]
    fn fetch_filter_normalization_preserves_globs_and_adds_directory_descendants() {
        assert_eq!(
            normalize_fetch_filter_patterns("/foo, a*, media/reallybigfiles"),
            "foo,foo/**,a*,media/reallybigfiles,media/reallybigfiles/**"
        );
    }
}
