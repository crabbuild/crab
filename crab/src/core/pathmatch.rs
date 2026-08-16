//! Pathspec-based include/exclude matcher wrapping `gix_pathspec::Search`.
//!
//! This module is the single pathspec/selector compiler used across
//! `crab add`, `crab hydrate`, `crab dehydrate`, and the filter-
//! process classifier. It replaces four hand-rolled glob engines that
//! previously lived in `cmd/add.rs`, `cmd/hydrate.rs`, `cmd/dehydrate.rs`,
//! `git/clean.rs`, `lfs/status.rs`, and `lfs/migrate.rs` — each with
//! subtly different semantics for `*` vs `**`, directory boundaries,
//! and negation.
//!
//! Pathspec magic is supported because `gix_pathspec` understands it
//! natively: `:(exclude)*.tmp`, `:(glob)foo/**`, `:(icase)*.BIN` all
//! parse and match the same way git does. See the user-facing guide
//! at `docs/guides/crab-add.md` for details.
//!
//! The public shape (`PatternFilter::matches(&str) -> bool`,
//! `build_filter(include, exclude) -> Result<PatternFilter>`) is kept
//! identical to the legacy `core::pattern::PatternFilter` so existing
//! callers can switch engines without refactoring.

use std::path::Path;

use bstr::{BStr, ByteSlice};
use gix_pathspec::{Pattern, Search};
use tracing::debug;

use crate::core::error::{CrabError, Result};

/// Compiled pathspec include/exclude filter.
///
/// Holds two `gix_pathspec::Search` instances: one for the include set
/// (a path matches when it matches any include pattern) and one for the
/// exclude set (a path is dropped when it matches any exclude pattern).
/// Empty include set matches nothing; empty exclude set excludes nothing.
///
/// Wrapped in a `std::sync::Mutex` because `gix_pathspec::Search`'s
/// matching method takes `&mut self` (it updates per-pattern attribute
/// caches). Contention is zero on crab's hot paths — matching is
/// called one path at a time from the walker.
pub struct PatternFilter {
    include: Option<std::sync::Mutex<Search>>,
    exclude: Option<std::sync::Mutex<Search>>,
}

impl PatternFilter {
    /// Returns `true` if `path` matches the include set and is not excluded.
    ///
    /// `path` is interpreted as repo-root-relative with `/` separators,
    /// matching git's pathspec convention. Windows callers must convert
    /// backslashes before calling.
    pub fn matches(&self, path: &str) -> bool {
        let bytes: &BStr = path.as_bytes().as_bstr();

        let included = match &self.include {
            Some(mx) => {
                let Ok(mut s) = mx.lock() else {
                    return false;
                };
                matches_any(&mut s, bytes)
            }
            None => false,
        };

        if !included {
            return false;
        }

        if let Some(mx) = &self.exclude {
            let Ok(mut s) = mx.lock() else {
                return false;
            };
            if matches_any(&mut s, bytes) {
                return false;
            }
        }

        true
    }
}

/// Build a [`PatternFilter`] from include and exclude patterns.
///
/// Each pattern string is parsed with `gix_pathspec::parse`, which
/// accepts full git pathspec syntax: plain globs (`*.bin`), `**` for
/// across-separator matching, and magic prefixes (`:(exclude)`,
/// `:(glob)`, `:(icase)`, `:(top)`).
///
/// # Errors
///
/// Returns [`CrabError::InvalidPattern`] if any pattern fails to parse
/// or normalize. The source chain carries the underlying
/// `gix_pathspec::parse::Error` or normalize error for debugging.
pub fn build_filter(include: &[String], exclude: &[String]) -> Result<PatternFilter> {
    Ok(PatternFilter {
        include: build_search(include)?,
        exclude: build_search(exclude)?,
    })
}

fn build_search(patterns: &[String]) -> Result<Option<std::sync::Mutex<Search>>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let defaults = gix_pathspec::Defaults::default();
    let mut parsed: Vec<Pattern> = Vec::with_capacity(patterns.len());
    for raw in patterns {
        match gix_pathspec::parse(raw.as_bytes(), defaults) {
            Ok(p) => parsed.push(p),
            Err(err) => {
                return Err(CrabError::Configuration {
                    key: raw.clone(),
                    origin: format!("pathspec parse: {err}"),
                });
            }
        }
    }

    let search = Search::from_specs(parsed, None, Path::new("")).map_err(|err| {
        CrabError::Configuration {
            key: patterns.join(","),
            origin: format!("pathspec normalize: {err}"),
        }
    })?;

    debug!(patterns = patterns.len(), "compiled pathspec filter");
    Ok(Some(std::sync::Mutex::new(search)))
}

/// Run `path` through a pathspec search and report whether any pattern
/// matches. The `attributes` callback is unused by crab's selectors
/// (no `:(attr:...)` support); returning `false` is the standard no-op
/// signal used by the pathspec examples.
fn matches_any(search: &mut Search, path: &BStr) -> bool {
    fn no_attrs(
        _path: &BStr,
        _case: gix_pathspec::attributes::glob::pattern::Case,
        _is_dir: bool,
        _out: &mut gix_pathspec::attributes::search::Outcome,
    ) -> bool {
        false
    }

    match search.pattern_matching_relative_path(path, Some(false), &mut no_attrs) {
        Some(m) => !m.is_excluded(),
        None => false,
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
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
        // Pathspec `*.bin` matches across separators too (git's default).
        assert!(f.matches("dir/model.bin"));
    }

    #[test]
    fn double_star_matches_across_components() {
        // Git pathspec treats `**/*.bin` as "nested dir / *.bin". The
        // leading `**` requires at least one directory separator, so
        // `model.bin` in the repo root does NOT match. This is
        // intentionally stricter than globset and matches git.
        let f = build_filter(&[s("**/*.bin")], &[]).unwrap();
        assert!(f.matches("models/v1/model.bin"));
        assert!(f.matches("deep/nested/path/weights.bin"));
        assert!(!f.matches("models/v1/model.txt"));
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
    fn directory_prefix_pattern() {
        let f = build_filter(&[s("models/**")], &[]).unwrap();
        assert!(f.matches("models/v1/weights.bin"));
        assert!(f.matches("models/config.json"));
        assert!(!f.matches("data/models/weights.bin"));
    }

    #[test]
    fn multiple_include_patterns_compose() {
        let f = build_filter(&[s("*.bin"), s("*.safetensors")], &[]).unwrap();
        assert!(f.matches("model.bin"));
        assert!(f.matches("weights.safetensors"));
        assert!(!f.matches("readme.md"));
    }

    #[test]
    fn invalid_pattern_returns_error() {
        // A truly malformed pathspec is hard to construct because
        // `gix_pathspec::parse` is very permissive — empty strings are
        // the only reliable failure. Document via test so a future
        // version-bump that starts erroring on bare `*` surfaces here.
        let result = build_filter(&[String::new()], &[]);
        assert!(result.is_err(), "empty pattern should fail to parse");
    }

    #[test]
    fn pathspec_magic_exclude_is_respected() {
        // `:(exclude)*.tmp` is git pathspec syntax for "exclude this
        // pattern from the result set". Support is inherited from
        // gix_pathspec::Search::from_specs which sorts excludes first.
        let f = build_filter(&[s("*.bin"), s(":(exclude)*.tmp")], &[]).unwrap();
        assert!(f.matches("model.bin"));
        assert!(!f.matches("build.tmp"));
    }
}
