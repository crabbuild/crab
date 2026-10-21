use serde::{Deserialize, Serialize};

use super::*;

const MAX_RELEASE_ASSETS: usize = 96;
const MAX_PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleasePublication {
    pub expected_version: u64,
    pub result_version: u64,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseAssetRecord {
    pub request_id: [u8; 16],
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub digest: String,
    pub uploader: RepositoryAuthor,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRecord {
    pub number: u64,
    pub create_submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub tag_name: String,
    pub tag_oid: Option<String>,
    pub target_oid: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
    pub publication_pending: Option<ReleasePublication>,
    pub version: u64,
    pub created_at_ms: u64,
    pub published_at_ms: Option<u64>,
    pub updated_at_ms: u64,
    pub deleted: bool,
    pub assets: Vec<ReleaseAssetRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateReleaseInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub tag_name: String,
    pub target_oid: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreateReleaseOutcome {
    Created(Box<ReleaseRecord>),
    PublicationPending(Box<ReleaseRecord>),
    RequestConflict,
    ReleaseConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateReleaseInput {
    pub number: u64,
    pub version: u64,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdateReleaseOutcome {
    Updated(Box<ReleaseRecord>),
    PublicationPending(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompleteReleasePublicationInput {
    pub number: u64,
    pub expected_version: u64,
    pub tag_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CompleteReleasePublicationOutcome {
    Completed(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteReleaseInput {
    pub number: u64,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DeleteReleaseOutcome {
    Deleted,
    NotFound,
    Conflict,
    PublicationPending,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseAssetReservation {
    pub release: u64,
    pub request_id: [u8; 16],
    pub expected_version: u64,
    pub name: String,
    pub content_type: String,
    pub uploader: RepositoryAuthor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReserveReleaseAssetInput {
    pub reservation: ReleaseAssetReservation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ReserveReleaseAssetOutcome {
    Reserved(ReleaseAssetReservation),
    Attached(Box<ReleaseRecord>),
    NotFound,
    Conflict,
    NameConflict,
    RequestConflict,
    AssetLimit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachReleaseAssetInput {
    pub reservation: ReleaseAssetReservation,
    pub size: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum AttachReleaseAssetOutcome {
    Attached(Box<ReleaseRecord>),
    NotFound,
    Conflict,
    NameConflict,
    RequestConflict,
    AssetLimit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteReleaseAssetInput {
    pub release: u64,
    pub request_id: [u8; 16],
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DeleteReleaseAssetOutcome {
    Deleted(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseListInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub include_drafts: bool,
    pub query: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleasePage {
    pub items: Vec<ReleaseRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseSubmissionKey {
    pub submission_id: [u8; 16],
}

mod operations;
pub(crate) use operations::{
    CompleteReleasePublication, CreateRelease, DeleteRelease, GetRelease, GetReleaseByTag,
    GetReleaseSubmission, ListReleases, UpdateRelease,
};

mod assets;
pub(crate) use assets::{AttachReleaseAsset, DeleteReleaseAsset, ReserveReleaseAsset};

mod validation;
use validation::{
    asset_reservation_digest, next_version, publication_result_version, release_digest,
    validate_asset_result, validate_create, validate_oid, validate_query, validate_reservation,
    validate_tag_name,
};

mod storage;
use storage::{
    bounded_release_page, insert_release, load_release, load_release_query, update_release_row,
    update_release_statement,
};

#[cfg(test)]
mod tests;
