//! Glob-based include/exclude pattern filter for hydrate and dehydrate commands.
//!
//! Historically this module built a [`PatternFilter`] on top of
//! `globset`. When the `gix-pathmatch` feature is enabled, construction
//! and matching are delegated to [`crate::core::pathmatch`] — a thin
//! wrapper over `gix_pathspec::Search` that honors git's pathspec
//! semantics (including `:(exclude)`, `:(glob)`, `:(icase)` magic). The
//! public API (`PatternFilter::matches(&str) -> bool`,
//! `build_filter(include, exclude) -> Result<PatternFilter>`) is the
//! same in both modes, so call sites remain unchanged.
//!
//! Patterns use `.gitattributes` glob syntax: `*` matches within a path
//! component, `**` matches across path components, `?` matches a single
//! character. The [`PatternFilter`] applies an include set minus an exclude
//! set to determine whether a path should be processed.

#[cfg(feature = "gix-pathmatch")]
pub use crate::core::pathmatch::{PatternFilter, build_filter};

#[cfg(not(feature = "gix-pathmatch"))]
pub use legacy::{PatternFilter, build_filter};

#[cfg(not(feature = "gix-pathmatch"))]
mod legacy {
    use globset::{Glob, GlobSet, GlobSetBuilder};

    use crate::core::error::Result;

    /// Compiled include/exclude glob filter.
    ///
    /// A path matches when it is in the include set and not in the exclude set.
    /// An empty include set matches nothing; an empty exclude set excludes nothing.
    pub struct PatternFilter {
        include: GlobSet,
        exclude: GlobSet,
    }

    impl PatternFilter {
        /// Returns `true` if `path` matches the include set and is not excluded.
        pub fn matches(&self, path: &str) -> bool {
            self.include.is_match(path) && !self.exclude.is_match(path)
        }
    }

    /// Build a [`PatternFilter`] from include and exclude glob patterns.
    ///
    /// Each pattern string uses `.gitattributes` glob syntax (`*`, `**`, `?`).
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::InvalidPattern`] if any pattern has invalid syntax.
    pub fn build_filter(include: &[String], exclude: &[String]) -> Result<PatternFilter> {
        let mut inc = GlobSetBuilder::new();
        for p in include {
            inc.add(Glob::new(p)?);
        }
        let mut exc = GlobSetBuilder::new();
        for p in exclude {
            exc.add(Glob::new(p)?);
        }
        Ok(PatternFilter {
            include: inc.build()?,
            exclude: exc.build()?,
        })
    }
}

#[cfg(test)]
#[cfg(not(feature = "gix-pathmatch"))]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_owned()
    }

    #[test]
    fn single_star_matches_extension() {
        let f = build_filter(&[s("*.bin")], &[]).unwrap();
        assert!(f.matches("model.bin"));
        assert!(f.matches("weights.bin"));
        assert!(!f.matches("model.txt"));
        // globset's `*` matches across separators, consistent with
        // .gitattributes pathspec behavior for bare extension globs.
        assert!(f.matches("dir/model.bin"));
    }

    #[test]
    fn double_star_matches_across_components() {
        let f = build_filter(&[s("**/*.bin")], &[]).unwrap();
        assert!(f.matches("model.bin"));
        assert!(f.matches("models/v1/model.bin"));
        assert!(f.matches("deep/nested/path/weights.bin"));
        assert!(!f.matches("models/v1/model.txt"));
    }

    #[test]
    fn question_mark_matches_single_char() {
        let f = build_filter(&[s("file?.txt")], &[]).unwrap();
        assert!(f.matches("file1.txt"));
        assert!(f.matches("fileA.txt"));
        assert!(!f.matches("file12.txt"));
        assert!(!f.matches("file.txt"));
    }

    #[test]
    fn exclude_subtracts_from_include() {
        let f = build_filter(&[s("**/*.bin")], &[s("**/archive/**")]).unwrap();
        assert!(f.matches("models/v1/model.bin"));
        assert!(!f.matches("models/archive/old.bin"));
    }

    #[test]
    fn empty_include_matches_nothing() {
        let f = build_filter(&[], &[]).unwrap();
        assert!(!f.matches("anything.txt"));
        assert!(!f.matches(""));
    }

    #[test]
    fn empty_exclude_excludes_nothing() {
        let f = build_filter(&[s("**/*")], &[]).unwrap();
        assert!(f.matches("any/path/file.txt"));
    }

    #[test]
    fn multiple_include_patterns_compose() {
        let f = build_filter(&[s("*.bin"), s("*.safetensors")], &[]).unwrap();
        assert!(f.matches("model.bin"));
        assert!(f.matches("weights.safetensors"));
        assert!(!f.matches("readme.md"));
    }

    #[test]
    fn multiple_exclude_patterns_compose() {
        let f = build_filter(&[s("**/*")], &[s("*.log"), s("**/tmp/**")]).unwrap();
        assert!(f.matches("src/main.rs"));
        assert!(!f.matches("debug.log"));
        assert!(!f.matches("build/tmp/cache.bin"));
    }

    #[test]
    fn invalid_pattern_returns_error() {
        let result = build_filter(&[s("[invalid")], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn directory_prefix_pattern() {
        let f = build_filter(&[s("models/**")], &[]).unwrap();
        assert!(f.matches("models/v1/weights.bin"));
        assert!(f.matches("models/config.json"));
        assert!(!f.matches("data/models/weights.bin"));
    }
}
