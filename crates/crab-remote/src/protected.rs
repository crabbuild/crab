use crab_auth::PushRefUpdate;
use crab_metadata::{manifests::Manifest, receipts::PushCommitReceipt};
use crab_storage::StagedWrite;
use serde::{Deserialize, Serialize};

/// Current server-verified capsule publication plan format.
pub const PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION: u32 = 3;

/// Manifest-format plan still emitted by the legacy CLI publisher.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPushPlan {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_plan_id: Option<String>,
    pub repo_prefix: String,
    pub push_id: String,
    pub upload_prefix: String,
    pub base_manifest_generation: Option<u64>,
    pub base_manifest_etag: Option<String>,
    pub ref_updates: Vec<PushRefUpdate>,
    pub candidate_manifest: Manifest,
    pub push_commit_receipt: Option<PushCommitReceipt>,
    pub staged_objects: Vec<StagedWrite>,
}

/// Server-verified protected publication plan for one protocol-v2 capsule.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtectedCapsulePushPlan {
    pub schema_version: u32,
    pub repo_prefix: String,
    pub push_id: String,
    pub upload_prefix: String,
    pub base_root_digest: String,
    pub transaction_id: String,
    pub run_hash: String,
    pub run_size: u64,
    pub ref_updates: Vec<PushRefUpdate>,
    pub staged_objects: Vec<StagedWrite>,
}
