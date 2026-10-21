#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryAuthor {
    pub issuer: String,
    pub subject: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusRecord {
    pub number: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub context: String,
    pub state: u8,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateCommitStatusInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub context: String,
    pub state: u8,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateCommitStatusOutcome {
    Created(Box<CommitStatusRecord>),
    RequestConflict,
    ContextLimit,
    SubmissionLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusSubmissionKey {
    pub oid: String,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusCatalog {
    pub statuses: Vec<CommitStatusRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LabelRecord {
    pub number: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LabelCatalog {
    pub labels: Vec<LabelRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateLabelInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateLabelOutcome {
    Archived,
    Created(LabelRecord),
    RequestConflict,
    NameConflict,
    NotFound,
    LimitReached,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateLabelInput {
    pub number: u64,
    pub version: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateLabelOutcome {
    Archived,
    Updated(LabelRecord),
    NotFound,
    NameConflict,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeleteLabelInput {
    pub number: u64,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DeleteLabelOutcome {
    Archived,
    Deleted,
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateIssueInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateIssueOutcome {
    Archived,
    Created(Box<IssueRecord>),
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueRecord {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateCommentInput {
    pub submission_id: [u8; 16],
    pub issue: u64,
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommentRecord {
    pub issue: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateCommentOutcome {
    Archived,
    Created(CommentRecord),
    IssueNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommentKey {
    pub issue: u64,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateIssueInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_manage_metadata: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<u8>,
    pub label_ids: Option<Vec<u64>>,
    pub assignee_subjects: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateIssueOutcome {
    Archived,
    Updated(Box<IssueRecord>),
    NotFound,
    Forbidden,
    LabelForbidden,
    LabelInvalid,
    AssigneeForbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateCommentInput {
    pub key: CommentKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateCommentOutcome {
    Archived,
    Updated(CommentRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueSummary {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub comment_count: u64,
    pub reaction_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListCommentsInput {
    pub issue: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CommentPage {
    Found {
        items: Vec<CommentRecord>,
        next: Option<u64>,
    },
    IssueNotFound,
}
