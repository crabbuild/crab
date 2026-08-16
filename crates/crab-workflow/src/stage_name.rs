//! Workflow stage-name contract.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Result, WorkflowError};

/// Validated stage name.
///
/// Naming rules are intentionally narrower than filesystems allow:
/// names round-trip through YAML keys, SQL columns, ref names, and
/// directory paths, so the grammar is a conservative ASCII subset.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StageName(String);

impl StageName {
    /// Parse a base or expanded stage name.
    ///
    /// Accepts both base names (`^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`)
    /// and expanded names (`base@suffix`) where:
    /// - `base` satisfies the base grammar;
    /// - `@` is the separator;
    /// - `suffix` matches `^[a-zA-Z0-9_-]{1,64}$`;
    /// - total length is capped at 128 bytes.
    pub fn parse(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return Err(stage_name_invalid(s, "stage name must not be empty"));
        }

        if let Some(at_pos) = s.find('@') {
            if bytes.len() > 128 {
                return Err(stage_name_invalid(
                    s,
                    "expanded stage name must be 128 bytes or fewer",
                ));
            }

            let base = &s[..at_pos];
            let suffix = &s[at_pos + 1..];

            if suffix.contains('@') {
                return Err(stage_name_invalid(
                    s,
                    "expanded stage name may contain at most one '@' separator",
                ));
            }

            Self::validate_base(base, s)?;

            if suffix.is_empty() {
                return Err(stage_name_invalid(
                    s,
                    "expanded stage name suffix must not be empty",
                ));
            }
            if suffix.len() > 64 {
                return Err(stage_name_invalid(
                    s,
                    "expanded stage name suffix must be 64 bytes or fewer",
                ));
            }
            for &b in suffix.as_bytes() {
                let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
                if !ok {
                    return Err(stage_name_invalid(
                        s,
                        "expanded stage name suffix may only contain ASCII letters, digits, '_' or '-'",
                    ));
                }
            }

            Ok(Self(s.to_owned()))
        } else {
            if bytes.len() > 64 {
                return Err(stage_name_invalid(
                    s,
                    "stage name must be 64 bytes or fewer",
                ));
            }
            Self::validate_base(s, s)?;
            Ok(Self(s.to_owned()))
        }
    }

    fn validate_base(base: &str, full_name: &str) -> Result<()> {
        let bytes = base.as_bytes();
        if bytes.is_empty() {
            return Err(stage_name_invalid(
                full_name,
                "stage name must not be empty",
            ));
        }
        if bytes.len() > 64 {
            return Err(stage_name_invalid(
                full_name,
                "stage name must be 64 bytes or fewer",
            ));
        }

        let first = bytes[0];
        let first_ok = first.is_ascii_alphabetic() || first == b'_';
        if !first_ok {
            return Err(stage_name_invalid(
                full_name,
                "stage name must start with an ASCII letter or underscore",
            ));
        }

        for &b in &bytes[1..] {
            let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
            if !ok {
                return Err(stage_name_invalid(
                    full_name,
                    "stage name may only contain ASCII letters, digits, '_' or '-'",
                ));
            }
        }

        Ok(())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` if this is an expanded name.
    #[must_use]
    pub fn is_expanded(&self) -> bool {
        self.0.contains('@')
    }

    /// Returns the base portion before `@`, or the full name if not expanded.
    #[must_use]
    pub fn base_name(&self) -> &str {
        match self.0.find('@') {
            Some(pos) => &self.0[..pos],
            None => &self.0,
        }
    }

    /// Parse an effective stage name, permitting dot-separated segments.
    ///
    /// Effective names are produced by multi-root workflow merging. Each
    /// segment must satisfy the base stage-name grammar.
    pub fn parse_effective(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(stage_name_invalid(s, "stage name must not be empty"));
        }
        if s.contains('.') {
            for segment in s.split('.') {
                if segment.is_empty() {
                    return Err(stage_name_invalid(
                        s,
                        "dotted stage name segments must not be empty",
                    ));
                }
                Self::parse(segment)?;
            }
            Ok(Self(s.to_owned()))
        } else {
            Self::parse(s)
        }
    }

    /// Build a dotted compound name from a nested-yaml prefix and leaf stage.
    ///
    /// Each prefix segment is validated as a stage-name token. Dots are
    /// introduced only by this constructor or by [`Self::parse_effective`].
    pub fn from_joined(prefix: &str, leaf: &StageName) -> Result<Self> {
        if prefix.is_empty() {
            return Ok(leaf.clone());
        }
        for segment in prefix.split('.') {
            Self::parse(segment)?;
        }
        Ok(Self(format!("{prefix}.{}", leaf.as_str())))
    }
}

impl fmt::Display for StageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for StageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn stage_name_invalid(name: &str, reason: &'static str) -> WorkflowError {
    WorkflowError::StageNameInvalid {
        name: name.to_owned(),
        reason,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn stage_name_accepts_valid_names() {
        for valid in [
            "clean",
            "train_v2",
            "a",
            "Stage-Name-123",
            "_underscored",
            "a234567890123456789012345678901234567890123456789012345678901234",
        ] {
            let parsed = StageName::parse(valid)
                .unwrap_or_else(|e| panic!("expected '{valid}' to parse, got {e}"));
            assert_eq!(parsed.as_str(), valid);
        }
    }

    #[test]
    fn stage_name_rejects_invalid_names() {
        let too_long = "a".repeat(65);
        let invalid: &[&str] = &[
            "",
            &too_long,
            "with space",
            "with/slash",
            "with:colon",
            "123starts-with-digit",
            "-starts-with-dash",
            "cafe\u{301}",
        ];
        for name in invalid {
            let err = StageName::parse(name)
                .err()
                .unwrap_or_else(|| panic!("expected '{name}' to be rejected"));
            assert!(
                matches!(err, WorkflowError::StageNameInvalid { .. }),
                "wrong error variant for '{name}': {err}"
            );
        }
    }

    #[test]
    fn stage_name_display_and_as_ref() {
        let name = StageName::parse("train").unwrap();
        assert_eq!(name.to_string(), "train");
        assert_eq!(<StageName as AsRef<str>>::as_ref(&name), "train");
    }

    #[test]
    fn stage_name_accepts_expanded_names() {
        for valid in [
            "preprocess@raw_a",
            "train@0",
            "build@uk",
            "train@resnet-imagenet",
            "_stage@suffix-123",
            "a@b",
        ] {
            let parsed = StageName::parse(valid)
                .unwrap_or_else(|e| panic!("expected '{valid}' to parse, got {e}"));
            assert_eq!(parsed.as_str(), valid);
            assert!(parsed.is_expanded());
        }
    }

    #[test]
    fn stage_name_expanded_rejects_invalid() {
        let invalid: &[&str] = &[
            "a@b@c",
            "@suffix",
            "base@",
            "base@suf fix",
            "base@suf/fix",
            "123@suffix",
            &format!("base@{}", "a".repeat(65)),
        ];
        for name in invalid {
            let err = StageName::parse(name)
                .err()
                .unwrap_or_else(|| panic!("expected '{name}' to be rejected"));
            assert!(
                matches!(err, WorkflowError::StageNameInvalid { .. }),
                "wrong error variant for '{name}': {err}"
            );
        }
    }

    #[test]
    fn stage_name_expanded_total_length_cap() {
        let base = "a".repeat(64);
        let suffix = "b".repeat(64);
        let name = format!("{base}@{suffix}");
        assert_eq!(name.len(), 129);
        let err = StageName::parse(&name).unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));

        let base = "a".repeat(63);
        let suffix = "b".repeat(64);
        let name = format!("{base}@{suffix}");
        assert_eq!(name.len(), 128);
        StageName::parse(&name).expect("128-byte expanded name should parse");
    }

    #[test]
    fn stage_name_base_name_splits_expansion_suffix() {
        let base = StageName::parse("train").unwrap();
        assert_eq!(base.base_name(), "train");

        let expanded = StageName::parse("preprocess@raw_a").unwrap();
        assert!(expanded.is_expanded());
        assert_eq!(expanded.base_name(), "preprocess");
    }

    #[test]
    fn parse_effective_accepts_dotted_segments() {
        let parsed = StageName::parse_effective("data.clean").unwrap();
        assert_eq!(parsed.as_str(), "data.clean");
    }

    #[test]
    fn parse_effective_rejects_empty_dotted_segments() {
        let err = StageName::parse_effective("data..clean").unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }

    #[test]
    fn from_joined_validates_prefix_segments() {
        let leaf = StageName::parse("clean").unwrap();
        let joined = StageName::from_joined("data.prep", &leaf).unwrap();
        assert_eq!(joined.as_str(), "data.prep.clean");

        let err = StageName::from_joined("1bad", &leaf).unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }
}
