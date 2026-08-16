//! Workflow stage dependency contracts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::StageName;

/// Dependency on an input.
///
/// The full variant set is stable across YAML parsing, lockfiles, status
/// reporting, graph construction, and SDK workflow views. Runtime resolvers own
/// fetching, hashing, and materialization behind their own adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dep {
    /// Local path relative to the workflow root or stage working directory.
    Path(PathBuf),
    /// Path in another Crab repository revision.
    CrabRef {
        repo: String,
        rev: String,
        path: PathBuf,
    },
    /// Path in a Git repository revision.
    GitRef {
        url: String,
        rev: String,
        path: PathBuf,
    },
    /// External URL dependency.
    ///
    /// `digest` is optional; when present it pins content to a specific hash.
    /// When absent, the runtime resolver computes a digest from live content.
    Url { url: String, digest: Option<String> },
    /// OCI image reference pinned by manifest digest.
    OciImage { reference: String, digest: String },
    /// Output produced by another stage in the same workflow.
    StageOut { stage: StageName, out: PathBuf },
}

/// Return true for URL schemes accepted in DVC-style dependency fields.
#[must_use]
pub fn is_url_dep(value: &str) -> bool {
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
            | "adl"
            | "file"
            | "ssh"
            | "sftp"
            | "hdfs"
            | "webhdfs"
            | "remote"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_dep_schemes_are_stable() {
        for value in [
            "https://example.com/data.csv",
            "s3://bucket/data.csv",
            "gs://bucket/data.csv",
            "az://container/data.csv",
            "abfss://container/path",
            "file:///tmp/data.csv",
            "remote://datasets/raw.csv",
        ] {
            assert!(is_url_dep(value), "{value} should be a URL dep");
        }
        assert!(!is_url_dep("data/raw.csv"));
        assert!(!is_url_dep("C:/data/raw.csv"));
    }

    #[test]
    fn dep_stage_out_serializes_stage_name_contract() {
        let dep = Dep::StageOut {
            stage: StageName::parse("train@model").unwrap(),
            out: PathBuf::from("model.bin"),
        };
        let encoded = serde_json::to_string(&dep).unwrap();
        assert!(encoded.contains("train@model"));
        let decoded: Dep = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, dep);
    }
}
