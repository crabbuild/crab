//! Workflow parameter reference contracts.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, WorkflowError};

/// Reference into a params file.
///
/// Bare refs such as `model.lr` resolve against the workflow's declared params
/// files, or `params.yaml` by default. File-scoped refs such as
/// `custom.yaml: [epochs]` resolve against that file only. A file-scoped ref
/// with no key tracks every scalar in the file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParamRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

impl ParamRef {
    /// Parse a dotted-key reference, rejecting empty strings.
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(WorkflowError::ParamRefInvalid {
                key: "params".to_owned(),
                reason: "param reference must not be empty",
            });
        }
        Ok(Self {
            file: None,
            key: Some(s.to_owned()),
        })
    }

    /// Build a reference to a key in a specific params file.
    pub fn parse_in_file(file: PathBuf, key: &str) -> Result<Self> {
        validate_param_file(&file)?;
        if key.is_empty() {
            return Err(WorkflowError::ParamRefInvalid {
                key: format!("params '{}'", file.display()),
                reason: "param reference must not be empty",
            });
        }
        Ok(Self {
            file: Some(file),
            key: Some(key.to_owned()),
        })
    }

    /// Build a reference that tracks every scalar in a params file.
    pub fn all_in_file(file: PathBuf) -> Result<Self> {
        validate_param_file(&file)?;
        Ok(Self {
            file: Some(file),
            key: None,
        })
    }

    /// Borrow the underlying dotted key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.key.as_deref().unwrap_or("")
    }

    /// Params file explicitly attached to this ref, if any.
    #[must_use]
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Specific dotted key attached to this ref, if any.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Whether this ref tracks every scalar value in its file.
    #[must_use]
    pub fn tracks_all(&self) -> bool {
        self.file.is_some() && self.key.is_none()
    }

    /// Stable key used in hashes, lockfiles, and status diffs.
    #[must_use]
    pub fn lock_key_for(&self, key: &str) -> String {
        match &self.file {
            Some(file) => format!("{}:{key}", file.display()),
            None => key.to_owned(),
        }
    }
}

impl fmt::Display for ParamRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.file, &self.key) {
            (Some(file), Some(key)) => write!(f, "{}:{key}", file.display()),
            (Some(file), None) => write!(f, "{}:*", file.display()),
            (None, Some(key)) => f.write_str(key),
            (None, None) => f.write_str("<invalid-param-ref>"),
        }
    }
}

fn validate_param_file(file: &Path) -> Result<()> {
    if file.as_os_str().is_empty() {
        return Err(WorkflowError::ParamRefInvalid {
            key: "params".to_owned(),
            reason: "params file path must not be empty",
        });
    }
    if file.is_absolute() {
        return Err(WorkflowError::ParamRefInvalid {
            key: format!("params '{}'", file.display()),
            reason: "params file path must be relative",
        });
    }
    for component in file.components() {
        if matches!(component, Component::ParentDir) {
            return Err(WorkflowError::ParamRefInvalid {
                key: format!("params '{}'", file.display()),
                reason: "params file path must not contain '..' components",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_ref_uses_key_only() {
        let param = ParamRef::parse("model.lr").unwrap();
        assert_eq!(param.as_str(), "model.lr");
        assert_eq!(param.file(), None);
        assert_eq!(param.key(), Some("model.lr"));
        assert_eq!(param.lock_key_for("model.lr"), "model.lr");
    }

    #[test]
    fn file_scoped_ref_formats_lock_key() {
        let param = ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "epochs").unwrap();
        assert_eq!(param.file(), Some(Path::new("custom.yaml")));
        assert_eq!(param.key(), Some("epochs"));
        assert_eq!(param.to_string(), "custom.yaml:epochs");
        assert_eq!(param.lock_key_for("epochs"), "custom.yaml:epochs");
    }

    #[test]
    fn all_in_file_tracks_every_scalar() {
        let param = ParamRef::all_in_file(PathBuf::from("params.json")).unwrap();
        assert!(param.tracks_all());
        assert_eq!(param.key(), None);
        assert_eq!(param.to_string(), "params.json:*");
    }

    #[test]
    fn param_ref_rejects_empty_key() {
        let err = ParamRef::parse("").unwrap_err();
        assert!(matches!(err, WorkflowError::ParamRefInvalid { .. }));
    }

    #[test]
    fn file_scoped_ref_rejects_empty_key() {
        let err = ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "").unwrap_err();
        assert!(matches!(err, WorkflowError::ParamRefInvalid { .. }));
    }

    #[test]
    fn param_file_rejects_absolute_or_parent_paths() {
        let absolute = ParamRef::all_in_file(PathBuf::from("/tmp/params.yaml")).unwrap_err();
        assert!(matches!(absolute, WorkflowError::ParamRefInvalid { .. }));

        let parent = ParamRef::all_in_file(PathBuf::from("../params.yaml")).unwrap_err();
        assert!(matches!(parent, WorkflowError::ParamRefInvalid { .. }));
    }
}
