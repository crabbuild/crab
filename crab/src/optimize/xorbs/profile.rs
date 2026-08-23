//! Xorb optimization profile types and built-in profiles.
//!
//! A [`Profile`] describes how xorbs should be shaped: target size,
//! maximum xorbs per file, grouping strategy, and compression. Three
//! built-in profiles cover the most common workloads:
//!
//! | Name      | target_xorb_bytes | max_xorbs_per_file | group_by  | compression |
//! |-----------|-------------------|--------------------|-----------|-------------|
//! | `ml`      | 256 MiB           | 4                  | File      | Zstd(3)     |
//! | `dataset` | 64 MiB            | u32::MAX           | Directory | Zstd(5)     |
//! | `code`    | 16 MiB            | u32::MAX           | Hash      | Zstd(9)     |

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::core::config::{CompressionConfig, OptimizeXorbsConfig, ProfileOverride};
use crate::core::error::{CrabError, Result};

/// Minimum allowed `target_xorb_bytes`: 4 MiB.
const MIN_TARGET_XORB_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum allowed `target_xorb_bytes`: 2 GiB.
const MAX_TARGET_XORB_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Reserved built-in profile names that require `force_override = true`
/// to replace via config.
const RESERVED_NAMES: &[&str] = &["ml", "dataset", "code"];

/// Regex-like validation for profile names: `[a-z][a-z0-9-]{0,30}`.
fn is_valid_profile_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 31 {
        return false;
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Grouping strategy for xorb packing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    /// Group chunks by source file — keeps a single file's chunks
    /// together for locality.
    File,
    /// Group chunks by directory — co-locates related files.
    Directory,
    /// Group chunks by content hash — maximizes dedup across files.
    Hash,
}

impl fmt::Display for GroupBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File => f.write_str("file"),
            Self::Directory => f.write_str("directory"),
            Self::Hash => f.write_str("hash"),
        }
    }
}

impl GroupBy {
    /// Parse from a string value.
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s {
            "file" => Some(Self::File),
            "directory" => Some(Self::Directory),
            "hash" => Some(Self::Hash),
            _ => None,
        }
    }
}

/// An optimization profile describing the target xorb shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Target xorb size in bytes. Must be in `[4 MiB, 2 GiB]`.
    pub target_xorb_bytes: u64,
    /// Maximum number of xorbs a single file may span.
    pub max_xorbs_per_file: u32,
    /// How chunks are grouped into xorbs.
    pub group_by: GroupBy,
    /// Compression algorithm and level for destination xorbs.
    pub compression: CompressionConfig,
}

impl Profile {
    /// Built-in `ml` profile: 256 MiB xorbs, 4 per file, file-grouped, Zstd(3).
    ///
    /// Optimized for ML weight files (large, few files, sequential access).
    pub fn ml() -> Self {
        Self {
            target_xorb_bytes: 256 * 1024 * 1024,
            max_xorbs_per_file: 4,
            group_by: GroupBy::File,
            compression: CompressionConfig::Zstd { level: 3 },
        }
    }

    /// Built-in `dataset` profile: 64 MiB xorbs, unlimited per file,
    /// directory-grouped, Zstd(5).
    ///
    /// Optimized for mixed datasets (many medium files, directory locality).
    pub fn dataset() -> Self {
        Self {
            target_xorb_bytes: 64 * 1024 * 1024,
            max_xorbs_per_file: u32::MAX,
            group_by: GroupBy::Directory,
            compression: CompressionConfig::Zstd { level: 5 },
        }
    }

    /// Built-in `code` profile: 16 MiB xorbs, unlimited per file,
    /// hash-grouped, Zstd(9).
    ///
    /// Optimized for small-file code repos (high dedup, high compression).
    pub fn code() -> Self {
        Self {
            target_xorb_bytes: 16 * 1024 * 1024,
            max_xorbs_per_file: u32::MAX,
            group_by: GroupBy::Hash,
            compression: CompressionConfig::Zstd { level: 9 },
        }
    }

    /// Look up a profile by name, applying config overrides.
    ///
    /// Resolution order:
    /// 1. Check config overrides (`[optimize.xorbs.profiles.<name>]`).
    /// 2. Fall back to built-in profiles.
    /// 3. Return `NotFound` if neither exists.
    pub fn from_name(name: &str, cfg: &OptimizeXorbsConfig) -> Result<Self> {
        // Start with the built-in base (if any).
        let base = match name {
            "ml" => Some(Self::ml()),
            "dataset" => Some(Self::dataset()),
            "code" => Some(Self::code()),
            _ => None,
        };

        // Apply config override if present.
        if let Some(over) = cfg.profiles.get(name) {
            let mut profile = base.unwrap_or_else(|| {
                // Custom profile — start from code defaults as a base.
                Self::code()
            });
            apply_override(&mut profile, over, name)?;
            profile.validate_with_name(name)?;
            return Ok(profile);
        }

        match base {
            Some(profile) => Ok(profile),
            None => Err(CrabError::Configuration {
                key: format!("optimize.xorbs.profiles.{name}"),
                origin: "profile lookup".to_string(),
            }),
        }
    }

    /// Validate that all fields are within allowed ranges.
    pub fn validate(&self) -> Result<()> {
        self.validate_with_name("(unnamed)")
    }

    /// Validate with a profile name for error reporting.
    fn validate_with_name(&self, name: &str) -> Result<()> {
        if self.target_xorb_bytes < MIN_TARGET_XORB_BYTES
            || self.target_xorb_bytes > MAX_TARGET_XORB_BYTES
        {
            return Err(CrabError::OptimizeXorbsProfileOutOfRange {
                name: name.to_string(),
                bytes: self.target_xorb_bytes,
            });
        }
        Ok(())
    }

    /// Serialize the profile to a JSON string for journal storage.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&ProfileJson::from(self)).unwrap_or_else(|_| "{}".to_string())
    }

    /// Restore a profile recorded in an xorb optimization journal.
    pub fn from_json(raw: &str) -> Result<Self> {
        let stored: ProfileJson =
            serde_json::from_str(raw).map_err(|error| CrabError::Configuration {
                key: "xorb optimization journal profile".to_string(),
                origin: format!("invalid profile JSON: {error}"),
            })?;
        let group_by =
            GroupBy::from_str_value(&stored.group_by).ok_or_else(|| CrabError::Configuration {
                key: "xorb optimization journal profile.group_by".to_string(),
                origin: format!("unknown grouping strategy: {}", stored.group_by),
            })?;
        let compression = parse_compression_str(&stored.compression, "journal")?;
        let profile = Self {
            target_xorb_bytes: stored.target_xorb_bytes,
            max_xorbs_per_file: stored.max_xorbs_per_file,
            group_by,
            compression,
        };
        profile.validate_with_name("journal")?;
        Ok(profile)
    }
}

/// Check whether a profile name is reserved (built-in).
pub fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
}

/// Validate a profile name against the naming rules.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if !is_valid_profile_name(name) {
        return Err(CrabError::Configuration {
            key: format!("optimize.xorbs.profiles.{name}"),
            origin: "profile name must match [a-z][a-z0-9-]{{0,30}}".to_string(),
        });
    }
    Ok(())
}

/// Apply a [`ProfileOverride`] onto a base [`Profile`].
fn apply_override(profile: &mut Profile, over: &ProfileOverride, name: &str) -> Result<()> {
    if let Some(bytes) = over.target_xorb_bytes {
        profile.target_xorb_bytes = bytes;
    }
    if let Some(max) = over.max_xorbs_per_file {
        profile.max_xorbs_per_file = max;
    }
    if let Some(ref group) = over.group_by {
        profile.group_by =
            GroupBy::from_str_value(group).ok_or_else(|| CrabError::Configuration {
                key: format!("optimize.xorbs.profiles.{name}.group_by"),
                origin: format!("invalid group_by value: {group}"),
            })?;
    }
    if let Some(ref comp) = over.compression {
        profile.compression = parse_compression_str(comp, name)?;
    }
    Ok(())
}

/// Parse a compression string like `"zstd:3"` or `"lz4"`.
fn parse_compression_str(s: &str, profile_name: &str) -> Result<CompressionConfig> {
    match s {
        "none" => Ok(CompressionConfig::None),
        "lz4" => Ok(CompressionConfig::Lz4),
        other => {
            // Accept "zstd" (default level) or "zstd:N" or "zstd(N)".
            if other == "zstd" {
                return Ok(CompressionConfig::default());
            }
            let inner = other.strip_prefix("zstd:").or_else(|| {
                other
                    .strip_prefix("zstd(")
                    .and_then(|s| s.strip_suffix(')'))
            });
            if let Some(level_str) = inner {
                let level: i32 = level_str.parse().map_err(|_| CrabError::Configuration {
                    key: format!("optimize.xorbs.profiles.{profile_name}.compression"),
                    origin: format!("invalid zstd level: {level_str}"),
                })?;
                return Ok(CompressionConfig::Zstd { level });
            }
            Err(CrabError::Configuration {
                key: format!("optimize.xorbs.profiles.{profile_name}.compression"),
                origin: format!("unknown compression: {other}"),
            })
        }
    }
}

/// JSON-serializable form of a Profile for journal storage.
#[derive(Debug, Serialize, Deserialize)]
struct ProfileJson {
    target_xorb_bytes: u64,
    max_xorbs_per_file: u32,
    group_by: String,
    compression: String,
}

impl From<&Profile> for ProfileJson {
    fn from(p: &Profile) -> Self {
        Self {
            target_xorb_bytes: p.target_xorb_bytes,
            max_xorbs_per_file: p.max_xorbs_per_file,
            group_by: p.group_by.to_string(),
            compression: p.compression.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ml_profile_is_valid() {
        let p = Profile::ml();
        p.validate().unwrap();
        assert_eq!(p.target_xorb_bytes, 256 * 1024 * 1024);
        assert_eq!(p.max_xorbs_per_file, 4);
        assert_eq!(p.group_by, GroupBy::File);
        assert_eq!(p.compression, CompressionConfig::Zstd { level: 3 });
    }

    #[test]
    fn builtin_dataset_profile_is_valid() {
        let p = Profile::dataset();
        p.validate().unwrap();
        assert_eq!(p.target_xorb_bytes, 64 * 1024 * 1024);
        assert_eq!(p.max_xorbs_per_file, u32::MAX);
        assert_eq!(p.group_by, GroupBy::Directory);
        assert_eq!(p.compression, CompressionConfig::Zstd { level: 5 });
    }

    #[test]
    fn builtin_code_profile_is_valid() {
        let p = Profile::code();
        p.validate().unwrap();
        assert_eq!(p.target_xorb_bytes, 16 * 1024 * 1024);
        assert_eq!(p.max_xorbs_per_file, u32::MAX);
        assert_eq!(p.group_by, GroupBy::Hash);
        assert_eq!(p.compression, CompressionConfig::Zstd { level: 9 });
    }

    #[test]
    fn journal_profile_round_trips() {
        let profile = Profile::dataset();

        let restored = Profile::from_json(&profile.to_json()).unwrap();

        assert_eq!(restored, profile);
    }

    #[test]
    fn journal_profile_rejects_unknown_grouping() {
        let error = Profile::from_json(
            r#"{"target_xorb_bytes":67108864,"max_xorbs_per_file":4294967295,"group_by":"bucket","compression":"zstd:5"}"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown grouping strategy"));
    }

    #[test]
    fn validate_rejects_too_small() {
        let mut p = Profile::ml();
        p.target_xorb_bytes = 1024; // 1 KiB — below 4 MiB minimum
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("E0330"));
    }

    #[test]
    fn validate_rejects_too_large() {
        let mut p = Profile::ml();
        p.target_xorb_bytes = 3 * 1024 * 1024 * 1024; // 3 GiB — above 2 GiB max
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("E0330"));
    }

    #[test]
    fn validate_accepts_boundary_values() {
        let mut p = Profile::ml();
        p.target_xorb_bytes = MIN_TARGET_XORB_BYTES;
        p.validate().unwrap();

        p.target_xorb_bytes = MAX_TARGET_XORB_BYTES;
        p.validate().unwrap();
    }

    #[test]
    fn profile_name_validation() {
        assert!(is_valid_profile_name("ml"));
        assert!(is_valid_profile_name("my-custom-profile"));
        assert!(is_valid_profile_name("a012345678901234567890123456789")); // 31 chars
        assert!(!is_valid_profile_name("")); // empty
        assert!(!is_valid_profile_name("0bad")); // starts with digit
        assert!(!is_valid_profile_name("Bad")); // uppercase
        assert!(!is_valid_profile_name("a_underscore")); // underscore
        assert!(!is_valid_profile_name(
            "a0123456789012345678901234567890" // 32 chars — too long
        ));
    }

    #[test]
    fn reserved_names_detected() {
        assert!(is_reserved_name("ml"));
        assert!(is_reserved_name("dataset"));
        assert!(is_reserved_name("code"));
        assert!(!is_reserved_name("custom"));
    }

    #[test]
    fn from_name_returns_builtin() {
        let cfg = OptimizeXorbsConfig::default();
        let p = Profile::from_name("ml", &cfg).unwrap();
        assert_eq!(p, Profile::ml());
    }

    #[test]
    fn from_name_applies_override() {
        let mut cfg = OptimizeXorbsConfig::default();
        cfg.profiles.insert(
            "ml".to_string(),
            ProfileOverride {
                target_xorb_bytes: Some(128 * 1024 * 1024),
                max_xorbs_per_file: None,
                group_by: None,
                compression: None,
            },
        );
        let p = Profile::from_name("ml", &cfg).unwrap();
        assert_eq!(p.target_xorb_bytes, 128 * 1024 * 1024);
        // Other fields unchanged from ml() base.
        assert_eq!(p.max_xorbs_per_file, 4);
    }

    #[test]
    fn from_name_unknown_profile_errors() {
        let cfg = OptimizeXorbsConfig::default();
        let err = Profile::from_name("nonexistent", &cfg).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn profile_json_round_trip() {
        let p = Profile::ml();
        let json = p.to_json();
        assert!(json.contains("268435456")); // 256 MiB
        assert!(json.contains("file"));
    }

    #[test]
    fn compression_parsing() {
        assert_eq!(
            parse_compression_str("none", "t").unwrap(),
            CompressionConfig::None
        );
        assert_eq!(
            parse_compression_str("lz4", "t").unwrap(),
            CompressionConfig::Lz4
        );
        assert_eq!(
            parse_compression_str("zstd:5", "t").unwrap(),
            CompressionConfig::Zstd { level: 5 }
        );
        assert_eq!(
            parse_compression_str("zstd(9)", "t").unwrap(),
            CompressionConfig::Zstd { level: 9 }
        );
        assert_eq!(
            parse_compression_str("zstd", "t").unwrap(),
            CompressionConfig::default()
        );
        assert!(parse_compression_str("brotli", "t").is_err());
    }

    #[test]
    fn group_by_display_and_parse() {
        assert_eq!(GroupBy::File.to_string(), "file");
        assert_eq!(GroupBy::Directory.to_string(), "directory");
        assert_eq!(GroupBy::Hash.to_string(), "hash");

        assert_eq!(GroupBy::from_str_value("file"), Some(GroupBy::File));
        assert_eq!(
            GroupBy::from_str_value("directory"),
            Some(GroupBy::Directory)
        );
        assert_eq!(GroupBy::from_str_value("hash"), Some(GroupBy::Hash));
        assert_eq!(GroupBy::from_str_value("invalid"), None);
    }
}
