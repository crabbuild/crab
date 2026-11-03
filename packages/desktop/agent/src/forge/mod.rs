//! Forge (code-hosting) integration layer.
//!
//! Provides a provider-agnostic interface to GitHub, GitLab,
//! Bitbucket, and Gitea. Each provider implements [`Forge`] and is
//! selected based on the remote URL or manual override. Concrete
//! implementations live in sibling modules and use `reqwest` with
//! OS-keychain-backed tokens.

pub mod auth;
pub mod bitbucket;
pub mod detect;
pub mod gitea;
pub mod github;
pub mod gitlab;
pub mod handlers;
pub mod types;

pub use detect::{
    DetectedForge, DetectedProvider, detect_and_persist, detect_from_url, detect_provider,
};
pub use types::{
    Check, CheckStatus, CreatePrInput, ForgeRepo, Issue, IssueQuery, Pr, PrDetail, PrFile,
    PrInlineComment, PrRef, PrState, PrTimelineEntry, Provider, Review, ReviewEvent,
};

use crate::Result;

/// Provider-agnostic interface to a code-hosting forge.
///
/// Implementations handle authentication (OS keychain), rate-limit
/// token buckets, and Retry-After honour (logged as `warn!` when
/// throttled).
#[async_trait::async_trait]
pub trait Forge: Send + Sync {
    /// Which provider this instance targets.
    fn provider(&self) -> Provider;

    /// List open pull/merge requests for a repository.
    async fn list_prs(&self, repo: &ForgeRepo) -> Result<Vec<Pr>>;

    /// Create a new pull/merge request.
    async fn create_pr(&self, input: CreatePrInput) -> Result<Pr>;

    /// Add a comment to an existing pull request.
    async fn add_comment(&self, pr: &PrRef, body: &str) -> Result<()>;

    /// Submit a review (approve, request changes, or comment).
    async fn submit_review(&self, pr: &PrRef, review: Review) -> Result<()>;

    /// List CI check runs for a commit SHA.
    async fn list_checks(&self, sha: &str) -> Result<Vec<Check>>;

    /// List issues for a repository, optionally filtered.
    async fn list_issues(&self, repo: &ForgeRepo, q: IssueQuery) -> Result<Vec<Issue>>;

    /// Get full detail for a single PR (body, labels, mergeable, head SHA).
    async fn get_pr_detail(&self, pr_number: u64) -> Result<PrDetail>;

    /// List files changed in a PR with their diffs.
    async fn list_pr_files(&self, pr_number: u64) -> Result<Vec<PrFile>>;

    /// Get the timeline (comments + reviews) for a PR.
    async fn get_pr_timeline(&self, pr_number: u64) -> Result<Vec<PrTimelineEntry>>;

    /// Get inline (line-level) review comments for a PR.
    async fn get_pr_review_comments(&self, pr_number: u64) -> Result<Vec<PrInlineComment>>;
}
