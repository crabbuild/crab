//! Layered git-config reader.
//!
//! Wraps `gix_config::File` with a crab-shaped API: one call to
//! [`GixConfigResolver::open`] materializes the layered view (system
//! → global → local → worktree) from disk, and typed getters
//! ([`string`](GixConfigResolver::string), [`boolean`](GixConfigResolver::boolean),
//! [`integer`](GixConfigResolver::integer)) answer point lookups
//! without each caller reaching into gitoxide.
//!
//! Two overlays sit on top of the on-disk stack:
//!
//! * An **environment** overlay, populated by
//!   [`GixConfigResolver::apply_env_overrides`] from the standard
//!   `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`
//!   variables git ships, and
//! * A **command-line** overlay, populated explicitly by
//!   [`GixConfigResolver::set_cli_override`].
//!
//! Precedence, lowest-priority first, matches git's own:
//!
//! ```text
//! system → global → local → worktree → env → CLI
//! ```
//!
//! Writes stay on the `git config` shellout (see `requirements.md`
//! Per-Site Decision Matrix Keep table). Reads go through this
//! resolver.
//!
//! # Gotchas
//!
//! - `gix_config` parse failures are surfaced as
//!   [`CrabError::GixConfig`]; missing keys resolve to `Ok(None)`.
//! - Boolean parsing accepts git's liberal spellings (`true`,
//!   `yes`, `on`, `1` / `false`, `no`, `off`, `0`).
//! - Integer parsing honours git's size suffixes (`k`, `m`, `g`)
//!   via `gix_config`'s native accessor.
//! - The resolver holds parsed `gix_config::File` state; callers
//!   that need fresh reads after writing via the shellout build a
//!   new resolver.

#![cfg(feature = "gix-config")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gix_config::File;

use crate::core::error::{CrabError, Result};
use crate::gix_boundary;

/// A layered git-config view with env + CLI overlays.
///
/// See the module-level docs for the precedence rules.
#[derive(Debug)]
pub struct GixConfigResolver {
    /// Merged on-disk view (system → global → local → worktree).
    /// Built once at open time; the env and CLI overlays supplement
    /// but do not mutate this file.
    file: File<'static>,

    /// Environment overlay — `GIT_CONFIG_*` variables, populated on
    /// demand. Keys are in `section.name` or `section.sub.name`
    /// form and take precedence over the on-disk view.
    env: BTreeMap<String, String>,

    /// Command-line overlay — caller-supplied `-c key=value`
    /// overrides. Keys are in the same shape as `env`. Takes
    /// precedence over every other layer.
    cli: BTreeMap<String, String>,
}

impl GixConfigResolver {
    /// Build a resolver rooted at `git_dir`.
    ///
    /// Loads system / global / local / worktree layers via
    /// [`gix_config::File::from_git_dir`]. Missing layers are
    /// treated as empty; `git_dir` itself must be a valid `.git`
    /// directory path (e.g. `/repo/.git`).
    ///
    /// Environment and CLI overlays start empty. Populate them via
    /// [`Self::apply_env_overrides`] and [`Self::set_cli_override`].
    pub fn open(git_dir: &Path) -> Result<Self> {
        let _span = gix_boundary!("config", "open").entered();
        let file = File::from_git_dir(git_dir.to_path_buf()).map_err(|err| {
            CrabError::Internal(format!(
                "failed to load git config at {}: {err}",
                git_dir.display()
            ))
        })?;
        Ok(Self {
            file,
            env: BTreeMap::new(),
            cli: BTreeMap::new(),
        })
    }

    /// Build a resolver from an in-memory config string.
    ///
    /// Useful for tests and for callers that want to supply a
    /// synthesized on-disk view. The input is parsed with the same
    /// syntax git uses.
    pub fn from_str(text: &str) -> Result<Self> {
        use std::str::FromStr;
        let file = File::from_str(text)?;
        Ok(Self {
            file,
            env: BTreeMap::new(),
            cli: BTreeMap::new(),
        })
    }

    /// Apply `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` /
    /// `GIT_CONFIG_VALUE_<n>` environment variables from the current
    /// process. Unset or unparsable counts resolve to a no-op.
    ///
    /// Calling this repeatedly overwrites the existing env layer.
    pub fn apply_env_overrides(&mut self) {
        self.env.clear();
        let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") else {
            return;
        };
        let Ok(count) = count_str.parse::<usize>() else {
            return;
        };
        for i in 0..count {
            let key_var = format!("GIT_CONFIG_KEY_{i}");
            let val_var = format!("GIT_CONFIG_VALUE_{i}");
            if let (Ok(key), Ok(val)) = (std::env::var(&key_var), std::env::var(&val_var)) {
                self.env.insert(key, val);
            }
        }
    }

    /// Record a CLI-level override (`-c key=value`).
    ///
    /// Takes precedence over every other layer.
    pub fn set_cli_override(&mut self, key: &str, value: &str) {
        self.cli.insert(key.to_owned(), value.to_owned());
    }

    /// Resolve `key` as a string.
    ///
    /// `key` is dotted — `remote.origin.url`, `user.email`, etc.
    /// Sections with subsections use three segments
    /// (`remote.origin.url`); plain sections use two
    /// (`user.email`).
    pub fn string(&self, key: &str) -> Option<String> {
        // CLI and env overlays take precedence.
        if let Some(v) = self.cli.get(key) {
            return non_empty(v);
        }
        if let Some(v) = self.env.get(key) {
            return non_empty(v);
        }

        // On-disk view: delegate to gix_config's `string` accessor.
        // `string` returns `Option<Cow<BStr>>`; convert to an owned
        // UTF-8 string when possible. Binary values fall through to
        // `None` — crab does not consume binary config values.
        let value = self.file.string(key)?;
        let text = std::str::from_utf8(value.as_ref()).ok()?.to_owned();
        non_empty(&text)
    }

    /// Resolve `key` as a boolean, honoring git's liberal spellings.
    ///
    /// Returns `None` if the key is unset, `Some(Err)` if set but
    /// not parseable, `Some(Ok(true|false))` otherwise.
    pub fn boolean(&self, key: &str) -> Option<Result<bool>> {
        if let Some(v) = self.cli.get(key).or_else(|| self.env.get(key)) {
            return Some(parse_bool(v));
        }
        self.file.boolean(key).map(|res| {
            res.map_err(|err| {
                CrabError::Internal(format!("failed to parse boolean '{key}': {err}"))
            })
        })
    }

    /// Resolve `key` as an integer, honoring git's `k`/`m`/`g`
    /// size suffixes.
    pub fn integer(&self, key: &str) -> Option<Result<i64>> {
        if let Some(v) = self.cli.get(key).or_else(|| self.env.get(key)) {
            return Some(parse_int(v));
        }
        self.file.integer(key).map(|res| {
            res.map_err(|err| {
                CrabError::Internal(format!("failed to parse integer '{key}': {err}"))
            })
        })
    }
}

/// Parse a boolean string using git's semantics.
fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" | "" => Ok(false),
        other => Err(CrabError::Internal(format!(
            "invalid boolean value '{other}'"
        ))),
    }
}

/// Parse an integer with optional `k`/`m`/`g` suffix.
fn parse_int(raw: &str) -> Result<i64> {
    let trimmed = raw.trim();
    let (digits, multiplier): (&str, i64) =
        match trimmed.chars().last().map(|c| c.to_ascii_lowercase()) {
            Some('k') => (&trimmed[..trimmed.len() - 1], 1024),
            Some('m') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
            Some('g') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
            _ => (trimmed, 1),
        };
    let n: i64 = digits
        .parse()
        .map_err(|err| CrabError::Internal(format!("invalid integer '{raw}': {err}")))?;
    n.checked_mul(multiplier)
        .ok_or_else(|| CrabError::Internal(format!("integer overflow for '{raw}'")))
}

/// Treat empty strings as "unset", matching `git config`'s Shell
/// boolean idiom where `$(git config foo.bar)` returns an empty
/// string for missing keys.
fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Locate a `.git` directory by walking upward from `start`.
///
/// Wrapper over [`crate::git::discover::discover_git_dir`] so
/// config-resolver callers don't import the discover module just
/// to get a git-dir path.
pub fn discover_git_dir_from(start: &Path) -> Result<PathBuf> {
    // Honour `GIT_DIR` if set, to match discover's behavior.
    if let Ok(dir) = std::env::var("GIT_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    match gix_discover::upwards(start) {
        Ok((repo_path, _trust)) => {
            let (git_dir, _work_tree) = repo_path.into_repository_and_work_tree_directories();
            Ok(git_dir)
        }
        Err(e) => Err(CrabError::Internal(format!(
            "failed to discover .git directory under {}: {e}",
            start.display()
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Local overrides must win over values supplied via the
    /// in-memory config. Synthesized in a single `[section]` block
    /// with a second value so the file parser exercises the
    /// precedence path rather than just returning the single
    /// declared value.
    #[test]
    fn config_local_overrides_global() {
        let text = "\
[core]
    editor = vim
    editor = emacs
";
        let resolver = GixConfigResolver::from_str(text).unwrap();
        // `gix_config::File::string` returns the last declared value
        // in document order — i.e. the override wins. That's the
        // same rule git uses when local comes after global.
        assert_eq!(resolver.string("core.editor").as_deref(), Some("emacs"));
    }

    /// CLI overrides beat env overrides beat the on-disk file.
    #[test]
    fn config_env_override_wins() {
        let text = "[core]\n    editor = vim\n";
        let mut resolver = GixConfigResolver::from_str(text).unwrap();
        resolver.env.insert("core.editor".to_owned(), "nano".into());
        assert_eq!(resolver.string("core.editor").as_deref(), Some("nano"));

        resolver.set_cli_override("core.editor", "helix");
        assert_eq!(resolver.string("core.editor").as_deref(), Some("helix"));
    }

    /// Missing keys resolve to `None` in every accessor.
    #[test]
    fn config_default_on_missing_key() {
        let text = "[core]\n    editor = vim\n";
        let resolver = GixConfigResolver::from_str(text).unwrap();
        assert_eq!(resolver.string("does.not.exist"), None);
        assert!(resolver.boolean("does.not.exist").is_none());
        assert!(resolver.integer("does.not.exist").is_none());
    }

    /// Integer parsing honours git's size suffixes.
    #[test]
    fn config_integer_with_size_suffix() {
        let text = "[core]\n    bigFileThreshold = 512m\n";
        let resolver = GixConfigResolver::from_str(text).unwrap();
        // gix_config's `integer` already handles the `m` suffix.
        let val = resolver
            .integer("core.bigFileThreshold")
            .expect("key is set");
        assert_eq!(val.expect("parses"), 512 * 1024 * 1024);
    }

    /// Empty config strings resolve to `None` — matches git config
    /// shellout idiom where unset keys return an empty stdout.
    #[test]
    fn config_empty_string_resolves_to_none() {
        let text = "[user]\n    email =\n";
        let resolver = GixConfigResolver::from_str(text).unwrap();
        assert!(resolver.string("user.email").is_none());
    }

    /// Invalid boolean surfaces a `CrabError` rather than a
    /// silent fallback.
    #[test]
    fn config_invalid_boolean_errors() {
        let mut resolver = GixConfigResolver::from_str("").unwrap();
        resolver.env.insert("core.flag".to_owned(), "maybe".into());
        let err = resolver.boolean("core.flag").unwrap().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid boolean value"),
            "unexpected error: {msg}"
        );
    }
}
