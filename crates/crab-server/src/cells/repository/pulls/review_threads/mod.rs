use serde::{Deserialize, Serialize};

use crate::cells::repository::*;

const MAX_PAGE_WIRE_BYTES: usize = 1024 * 1024;
const MAX_SUGGESTED_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PullReviewThreadSide {
    Old,
    New,
}

impl PullReviewThreadSide {
    const fn code(self) -> u8 {
        match self {
            Self::Old => 0,
            Self::New => 1,
        }
    }

    fn from_code(value: u64) -> crate::cells::Result<Self> {
        match value {
            0 => Ok(Self::Old),
            1 => Ok(Self::New),
            _ => Err(crate::cells::Error::Invalid(
                "repository pull review thread side is invalid",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
    pub resolved: bool,
    pub resolved_by: Option<RepositoryAuthor>,
    pub resolved_at_ms: Option<u64>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyRecord {
    pub pull: u64,
    pub thread: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadPage {
    pub items: Vec<PullReviewThreadRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyPage {
    pub items: Vec<PullReviewReplyRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadKey {
    pub pull: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyKey {
    pub pull: u64,
    pub thread: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadSubmissionKey {
    pub pull: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplySubmissionKey {
    pub pull: Option<u64>,
    pub thread: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadListInput {
    pub pull: u64,
    pub before: Option<u64>,
    pub limit: u8,
    pub path: Option<Vec<u8>>,
    pub resolved: Option<bool>,
    pub comparison_base_oid: Option<String>,
    pub comparison_head_oid: Option<String>,
    pub outdated: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyListInput {
    pub pull: u64,
    pub thread: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewThreadInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewThreadOutcome {
    Created(Box<PullReviewThreadRecord>),
    PullNotFound,
    PullClosed,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewThreadInput {
    pub key: PullReviewThreadKey,
    pub actor: RepositoryAuthor,
    pub can_resolve: bool,
    pub version: u64,
    pub body: Option<String>,
    pub suggested_text: Option<Option<String>>,
    pub resolved: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewThreadOutcome {
    Updated(Box<PullReviewThreadRecord>),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewReplyInput {
    pub pull: u64,
    pub thread: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewReplyOutcome {
    Created(PullReviewReplyRecord),
    ThreadNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewReplyInput {
    pub key: PullReviewReplyKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewReplyOutcome {
    Updated(PullReviewReplyRecord),
    NotFound,
    Forbidden,
    Conflict,
}

mod operations;
pub(crate) use operations::{
    CreatePullReviewReply, CreatePullReviewThread, GetPullReviewReply,
    GetPullReviewReplySubmission, GetPullReviewThread, GetPullReviewThreadSubmission,
    ListPullReviewReplies, ListPullReviewThreads, UpdatePullReviewReply, UpdatePullReviewThread,
};

mod validation;
use validation::{
    next_version, same_author, validate_create_thread, validate_reply_key, validate_reply_list,
    validate_suggested_text, validate_thread_key, validate_thread_list, validate_thread_record,
};

mod storage;
use storage::{
    bounded_page, insert_reply_statement, insert_thread_statement, list_replies, list_threads,
    load_reply, load_reply_query, load_thread, load_thread_query, next_number, next_reply_number,
    pull_exists_query, pull_state, reply_digest, reply_from_row, thread_digest,
    thread_exists_query, thread_from_row, update_thread_statement,
};

#[cfg(test)]
mod tests;
