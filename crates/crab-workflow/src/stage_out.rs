//! Workflow stage output contracts.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{OutKind, Result, StageName, WorkflowError};

/// An output produced by a stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Out {
    pub path: PathBuf,
    pub kind: OutKind,
    pub cache: bool,
    #[serde(default = "default_true")]
    pub push: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub persist: bool,
    pub max_bytes: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl Out {
    /// Build an output with cached, pushed, non-persisted defaults.
    #[must_use]
    pub fn new(path: PathBuf, kind: OutKind) -> Self {
        Self {
            path,
            kind,
            cache: true,
            push: true,
            remote: None,
            persist: false,
            max_bytes: None,
        }
    }

    /// Return true when this output names a DVC-style external URL.
    #[must_use]
    pub fn is_external_url(&self) -> bool {
        is_external_url_out_path(&self.path)
    }

    /// Validate traversal, cache, push, remote, and external-output rules.
    ///
    /// Ordinary cached stage outs are repo-relative. Absolute paths and
    /// external URLs are accepted only for uncached external outputs.
    pub fn validate(&self, stage_name: &StageName) -> Result<()> {
        if self.path.is_absolute() && self.cache {
            return Err(WorkflowError::StageOutMalformed {
                stage: stage_name.as_str().to_owned(),
                path: self.path.clone(),
                reason: "absolute out paths must set cache: false",
            });
        }
        if self.is_external_url() {
            if self.cache {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage_name.as_str().to_owned(),
                    path: self.path.clone(),
                    reason: "external URL outs must set cache: false",
                });
            }
            if self.push {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage_name.as_str().to_owned(),
                    path: self.path.clone(),
                    reason: "external URL outs must set push: false",
                });
            }
            if self.kind == OutKind::Stdout {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage_name.as_str().to_owned(),
                    path: self.path.clone(),
                    reason: "external URL outs cannot use kind: stdout",
                });
            }
            if self.remote.is_some() {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage_name.as_str().to_owned(),
                    path: self.path.clone(),
                    reason: "external URL outs cannot set remote",
                });
            }
            return Ok(());
        }
        for component in self.path.components() {
            if matches!(component, Component::ParentDir) {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage_name.as_str().to_owned(),
                    path: self.path.clone(),
                    reason: "out path must not contain '..' components",
                });
            }
        }
        if self
            .remote
            .as_deref()
            .is_some_and(|remote| remote.trim().is_empty())
        {
            return Err(WorkflowError::StageOutMalformed {
                stage: stage_name.as_str().to_owned(),
                path: self.path.clone(),
                reason: "out remote name must not be empty",
            });
        }
        Ok(())
    }
}

/// Return true when a path value is stored as a DVC-style external output URL.
#[must_use]
pub fn is_external_url_out_path(path: &Path) -> bool {
    let value = path.to_string_lossy();
    is_external_url_out(value.as_ref())
}

/// Return true for URL schemes accepted in DVC-style external output fields.
#[must_use]
pub fn is_external_url_out(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once("://") else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http"
            | "https"
            | "s3"
            | "s3a"
            | "gs"
            | "az"
            | "azure"
            | "abfs"
            | "abfss"
            | "ssh"
            | "sftp"
            | "hdfs"
            | "webhdfs"
            | "remote"
    )
}

/// Validate a `wdir` value: must be relative, no `..` traversal.
pub fn validate_wdir(wdir: &Path, stage_name: &StageName) -> Result<()> {
    if wdir.is_absolute() {
        return Err(WorkflowError::WdirInvalid {
            stage: stage_name.as_str().to_owned(),
            path: wdir.to_path_buf(),
            reason: "wdir must be a relative path",
        });
    }
    for component in wdir.components() {
        if matches!(component, Component::ParentDir) {
            return Err(WorkflowError::WdirInvalid {
                stage: stage_name.as_str().to_owned(),
                path: wdir.to_path_buf(),
                reason: "wdir must not contain '..' components",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_validate_rejects_absolute_paths() {
        let stage = StageName::parse("train").unwrap();
        let out = Out::new(PathBuf::from("/etc/passwd"), OutKind::File);
        let err = out.validate(&stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::StageOutMalformed { reason, .. } if reason.contains("cache: false"))
        );
    }

    #[test]
    fn out_validate_accepts_uncached_absolute_external_output() {
        let stage = StageName::parse("train").unwrap();
        let mut out = Out::new(PathBuf::from("/tmp/crab-external-output"), OutKind::File);
        out.cache = false;
        out.push = false;
        out.validate(&stage)
            .expect("uncached absolute external output should validate");
    }

    #[test]
    fn out_validate_rejects_cached_external_url_output() {
        let stage = StageName::parse("train").unwrap();
        let out = Out::new(PathBuf::from("s3://bucket/model.pkl"), OutKind::File);
        let err = out.validate(&stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::StageOutMalformed { reason, .. } if reason.contains("cache: false"))
        );
    }

    #[test]
    fn out_validate_rejects_stdout_external_url_output() {
        let stage = StageName::parse("train").unwrap();
        let mut out = Out::new(
            PathBuf::from("https://example.com/model.pkl"),
            OutKind::Stdout,
        );
        out.cache = false;
        out.push = false;
        let err = out.validate(&stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::StageOutMalformed { reason, .. } if reason.contains("stdout"))
        );
    }

    #[test]
    fn out_validate_rejects_double_dot_components() {
        let stage = StageName::parse("train").unwrap();
        let out = Out::new(PathBuf::from("outs/../escape"), OutKind::File);
        let err = out.validate(&stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::StageOutMalformed { reason, .. } if reason.contains(".."))
        );
    }

    #[test]
    fn out_validate_accepts_relative_paths() {
        let stage = StageName::parse("train").unwrap();
        let out = Out::new(PathBuf::from("outs/model.pkl"), OutKind::File);
        out.validate(&stage).expect("relative path should validate");
    }

    #[test]
    fn wdir_validate_accepts_relative_paths() {
        let stage = StageName::parse("train").unwrap();
        validate_wdir(Path::new("training"), &stage).expect("relative path should validate");
        validate_wdir(Path::new("sub/dir"), &stage).expect("nested relative path should validate");
    }

    #[test]
    fn wdir_validate_rejects_absolute_paths() {
        let stage = StageName::parse("train").unwrap();
        let err = validate_wdir(Path::new("/absolute/path"), &stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::WdirInvalid { reason, .. } if reason.contains("relative")),
            "wrong error: {err}"
        );
    }

    #[test]
    fn wdir_validate_rejects_parent_traversal() {
        let stage = StageName::parse("train").unwrap();
        let err = validate_wdir(Path::new("sub/../escape"), &stage).unwrap_err();
        assert!(
            matches!(err, WorkflowError::WdirInvalid { reason, .. } if reason.contains("..")),
            "wrong error: {err}"
        );
    }
}
