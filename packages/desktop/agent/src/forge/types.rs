//! Shared types for forge (code-hosting) integrations.
//!
//! All types derive `Serialize` + `Deserialize` so they can flow
//! directly through the JSON-RPC layer without manual conversion.

use serde::{Deserialize, Serialize};

/// Supported code-hosting providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    Bitbucket,
    Gitea,
}

/// Identifies a repository on a forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeRepo {
    pub owner: String,
    pub name: String,
    pub provider: Provider,
}

/// Reference to a specific pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: ForgeRepo,
    pub number: u64,
}

/// State of a pull/merge request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// Summary of a pull/merge request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub state: PrState,
    pub author: String,
    pub head_branch: String,
    pub base_branch: String,
    pub url: String,
    pub draft: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for creating a new pull/merge request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePrInput {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

/// Event type for a code review submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

/// A code review to submit on a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub body: String,
    pub event: ReviewEvent,
}

/// Status of a CI check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

/// A CI check associated with a commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
    pub conclusion: Option<String>,
}

/// Summary of a repository issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub author: String,
    pub created_at: String,
    pub url: String,
    pub labels: Vec<String>,
}

/// Query parameters for listing issues.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueQuery {
    pub state: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u32>,
}

// ── Full PR detail types ────────────────────────────────────────────

/// Full pull request detail including body, labels, and mergeable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrDetail {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: PrState,
    pub draft: bool,
    pub author: String,
    pub head_branch: String,
    pub head_sha: String,
    pub base_branch: String,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub labels: Vec<String>,
    pub mergeable: Option<bool>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files_count: u64,
}

/// A file changed in a pull request, with optional patch content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrFile {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
}

/// A comment or review event in the PR timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrTimelineEntry {
    pub id: String,
    pub kind: String,
    pub author: String,
    pub body: Option<String>,
    pub created_at: String,
    /// For review events: "approved", "changes_requested", "commented"
    pub verdict: Option<String>,
    /// For commit events
    pub sha: Option<String>,
    pub subject: Option<String>,
}

/// An inline (line-level) review comment on a PR diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrInlineComment {
    pub id: String,
    pub path: String,
    pub line: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}
