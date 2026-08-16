//! Dataset/model release manifest data model.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};

pub const RELEASE_MANIFEST_SCHEMA_VERSION: u16 = 1;
pub const RELEASE_MANIFEST_DIGEST_PREFIX: &str = "b3:";

/// Deterministic release manifest binding a Git revision to Crab content.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub schema_version: u16,
    pub release_id: String,
    pub revision: ReleaseRevision,
    pub selected_refs: BTreeMap<String, ReleaseRefTarget>,
    pub crab: ReleaseCrabInventory,
    pub workflow: ReleaseWorkflowMetadata,
    pub signature: ReleaseSignature,
}

impl ReleaseManifest {
    /// Return a copy with order-insensitive child lists sorted canonically.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let mut manifest = self.clone();
        manifest.crab.large_files.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.file_hash.cmp(&right.file_hash))
                .then_with(|| left.size.cmp(&right.size))
        });
        manifest.workflow.stages.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.stage_hash.cmp(&right.stage_hash))
        });
        for stage in &mut manifest.workflow.stages {
            stage.outs.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then_with(|| left.file_hash.cmp(&right.file_hash))
                    .then_with(|| left.size.cmp(&right.size))
            });
        }
        manifest
    }

    /// Return a copy suitable for detached-signature identity checks.
    #[must_use]
    pub fn unsigned_identity(&self) -> Self {
        let mut manifest = self.clone();
        manifest.signature = ReleaseSignature::default();
        manifest
    }

    /// Serialize the normalized manifest as compact deterministic JSON bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&self.normalized())
            .map_err(|e| CrabError::Internal(format!("release manifest serialize: {e}")))
    }

    /// Return the Blake3 content digest of the canonical manifest bytes.
    pub fn content_digest(&self) -> Result<String> {
        let bytes = self.canonical_bytes()?;
        Ok(format!(
            "{RELEASE_MANIFEST_DIGEST_PREFIX}{}",
            blake3::hash(&bytes).to_hex()
        ))
    }

    /// Return the digest over manifest content excluding detached-signature metadata.
    pub fn identity_digest(&self) -> Result<String> {
        self.unsigned_identity().content_digest()
    }
}

/// Git revision selected for the release.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseRevision {
    pub requested: String,
    pub commit: String,
}

/// Selected Git ref target included in a release manifest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseRefTarget {
    pub oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peeled_oid: Option<String>,
}

/// Crab-managed large-file inventory included in a release manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseCrabInventory {
    pub large_files: Vec<ReleaseLargeFile>,
}

/// One Crab pointer-backed file recorded by a release manifest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseLargeFile {
    pub path: String,
    pub file_hash: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard_hint: Option<String>,
}

/// Workflow metadata captured alongside repository content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseWorkflowMetadata {
    pub params: BTreeMap<String, String>,
    pub metrics: BTreeMap<String, String>,
    pub stages: Vec<ReleaseWorkflowStage>,
}

/// Workflow stage identity included in a release manifest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseWorkflowStage {
    pub name: String,
    pub stage_hash: String,
    pub outs: Vec<ReleaseWorkflowOutput>,
}

/// One workflow output included in a release manifest stage.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseWorkflowOutput {
    pub path: String,
    pub file_hash: String,
    pub size: u64,
}

/// Detached signature metadata for a release manifest.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseSignature {
    pub state: ReleaseSignatureState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_digest: Option<String>,
}

impl Default for ReleaseSignature {
    fn default() -> Self {
        Self {
            state: ReleaseSignatureState::Unsigned,
            key_id: None,
            signature_digest: None,
        }
    }
}

/// Signature state reported by release verification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSignatureState {
    Unsigned,
    Signed,
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn sample_manifest() -> ReleaseManifest {
        ReleaseManifest {
            schema_version: RELEASE_MANIFEST_SCHEMA_VERSION,
            release_id: "model-v1".to_owned(),
            revision: ReleaseRevision {
                requested: "v1".to_owned(),
                commit: "0123456789012345678901234567890123456789".to_owned(),
            },
            selected_refs: BTreeMap::from([(
                "refs/tags/v1".to_owned(),
                ReleaseRefTarget {
                    oid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    peeled_oid: Some("0123456789012345678901234567890123456789".to_owned()),
                },
            )]),
            crab: ReleaseCrabInventory {
                large_files: vec![
                    ReleaseLargeFile {
                        path: "z.bin".to_owned(),
                        file_hash: "b3:2222".to_owned(),
                        size: 2,
                        shard_hint: None,
                    },
                    ReleaseLargeFile {
                        path: "a.bin".to_owned(),
                        file_hash: "b3:1111".to_owned(),
                        size: 1,
                        shard_hint: Some("b3:aaaa".to_owned()),
                    },
                ],
            },
            workflow: ReleaseWorkflowMetadata {
                params: BTreeMap::from([("model.lr".to_owned(), "0.01".to_owned())]),
                metrics: BTreeMap::from([("accuracy".to_owned(), "0.95".to_owned())]),
                stages: vec![
                    ReleaseWorkflowStage {
                        name: "train".to_owned(),
                        stage_hash: "b3:bbbb".to_owned(),
                        outs: vec![ReleaseWorkflowOutput {
                            path: "model.bin".to_owned(),
                            file_hash: "b3:3333".to_owned(),
                            size: 3,
                        }],
                    },
                    ReleaseWorkflowStage {
                        name: "prep".to_owned(),
                        stage_hash: "b3:aaaa".to_owned(),
                        outs: vec![ReleaseWorkflowOutput {
                            path: "features.bin".to_owned(),
                            file_hash: "b3:4444".to_owned(),
                            size: 4,
                        }],
                    },
                ],
            },
            signature: ReleaseSignature::default(),
        }
    }

    #[test]
    fn canonical_bytes_sort_lists() -> TestResult {
        let left = sample_manifest();
        let mut right = sample_manifest();
        right.crab.large_files.reverse();
        right.workflow.stages.reverse();

        assert_eq!(left.canonical_bytes()?, right.canonical_bytes()?);
        assert_eq!(left.content_digest()?, right.content_digest()?);
        Ok(())
    }

    #[test]
    fn digest_changes_on_content_change() -> TestResult {
        let left = sample_manifest();
        let mut right = sample_manifest();
        right.crab.large_files[0].file_hash = "b3:changed".to_owned();

        assert_ne!(left.content_digest()?, right.content_digest()?);
        Ok(())
    }

    #[test]
    fn identity_digest_ignores_detached_signature_metadata() -> TestResult {
        let left = sample_manifest();
        let mut right = sample_manifest();
        right.signature = ReleaseSignature {
            state: ReleaseSignatureState::Signed,
            key_id: Some("key-1".to_owned()),
            signature_digest: Some("b3:abc".to_owned()),
        };

        assert_ne!(left.content_digest()?, right.content_digest()?);
        assert_eq!(left.identity_digest()?, right.identity_digest()?);
        Ok(())
    }

    #[test]
    fn canonical_bytes_are_compact_json() -> TestResult {
        let bytes = sample_manifest().canonical_bytes()?;
        let text = String::from_utf8(bytes)?;

        assert!(text.starts_with("{\"schema_version\":1,"));
        assert!(text.contains("\"large_files\":[{\"path\":\"a.bin\""));
        assert!(!text.contains('\n'));
        Ok(())
    }
}
