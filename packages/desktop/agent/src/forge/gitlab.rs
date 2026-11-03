//! GitLab forge implementation.
//!
//! Talks to the GitLab REST API v4 (gitlab.com or self-hosted
//! instances). Authentication is via a personal access token or
//! OAuth token stored in the OS keychain. Rate-limit handling
//! checks `RateLimit-Remaining` and honours `Retry-After`.

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use tracing::warn;

use super::{
    Check, CheckStatus, CreatePrInput, Forge, ForgeRepo, Issue, IssueQuery, Pr, PrRef, PrState,
    Provider, Review, ReviewEvent,
};
use crate::{AgentError, Result};

/// GitLab REST API v4 implementation of the [`Forge`] trait.
///
/// Scoped to a single repository (project). Constructed with a token
/// and optional base URL (defaults to `https://gitlab.com/api/v4`
/// for gitlab.com; set a custom URL for self-hosted instances, e.g.
/// `https://gitlab.example.com/api/v4`).
pub struct GitLabForge {
    client: Client,
    base_url: String,
    repo: ForgeRepo,
}

impl GitLabForge {
    /// Create a new GitLab forge client.
    ///
    /// `token` is a PAT or OAuth token. `base_url` defaults to
    /// `https://gitlab.com/api/v4` when `None` (use a custom URL for
    /// self-hosted GitLab).
    pub fn new(token: &str, base_url: Option<&str>, repo: ForgeRepo) -> Result<Self> {
        let base_url = base_url
            .unwrap_or("https://gitlab.com/api/v4")
            .trim_end_matches('/')
            .to_owned();

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("crab-desktop-agent"));

        // GitLab uses PRIVATE-TOKEN header for PATs, but Bearer also works
        // for OAuth tokens. We use the standard Authorization header with
        // Bearer scheme which works for both OAuth and PATs (GitLab 12.2+).
        let auth_value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| AgentError::Other(format!("invalid GitLab token header value: {e}")))?;
        headers.insert(AUTHORIZATION, auth_value);

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| AgentError::Other(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            repo,
        })
    }

    /// Build a full URL for a GitLab API endpoint path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// URL-encode a GitLab project path (`owner/name` → `owner%2Fname`).
    fn project_path(repo: &ForgeRepo) -> String {
        format!("{}%2F{}", repo.owner, repo.name)
    }

    /// Check rate-limit headers and log a warning when throttled.
    /// Returns `Ok(response)` if the request succeeded, or an error
    /// if the server returned a rate-limit (429) or other HTTP error.
    async fn check_response(&self, resp: Response) -> Result<Response> {
        // GitLab uses RateLimit-Remaining (capital R).
        if let Some(remaining) = resp
            .headers()
            .get("ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            if remaining == 0 {
                let reset = resp
                    .headers()
                    .get("ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown");
                warn!(
                    remaining,
                    reset, "GitLab rate limit exhausted, resets at epoch {reset}"
                );
            } else if remaining < 10 {
                warn!(remaining, "GitLab rate limit running low");
            }
        }

        let status = resp.status();

        // Handle 429 Too Many Requests with Retry-After.
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown");
            warn!(
                retry_after,
                "GitLab rate-limited (429), Retry-After: {retry_after}"
            );
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "GitLab API rate-limited (429): {body}"
            )));
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "GitLab API returned {status}: {body}"
            )));
        }

        Ok(resp)
    }
}

#[async_trait::async_trait]
impl Forge for GitLabForge {
    fn provider(&self) -> Provider {
        Provider::GitLab
    }

    async fn list_prs(&self, repo: &ForgeRepo) -> Result<Vec<Pr>> {
        let project = Self::project_path(repo);
        let url = self.url(&format!("/projects/{project}/merge_requests?state=opened"));

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab list_prs request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gl_mrs: Vec<GlMergeRequest> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab list_prs parse error: {e}")))?;

        Ok(gl_mrs.into_iter().map(Into::into).collect())
    }

    async fn create_pr(&self, input: CreatePrInput) -> Result<Pr> {
        let project = Self::project_path(&self.repo);
        let url = self.url(&format!("/projects/{project}/merge_requests"));

        let body = GlCreateMrBody {
            title: &input.title,
            description: &input.body,
            source_branch: &input.head,
            target_branch: &input.base,
            // GitLab uses "work in progress" prefix or draft flag (API v4 13.2+).
            draft: input.draft,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab create_pr request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gl_mr: GlMergeRequest = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab create_pr parse error: {e}")))?;

        Ok(gl_mr.into())
    }

    async fn add_comment(&self, pr: &PrRef, body: &str) -> Result<()> {
        let project = Self::project_path(&pr.repo);
        let url = self.url(&format!(
            "/projects/{project}/merge_requests/{}/notes",
            pr.number
        ));

        let payload = GlNoteBody { body };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab add_comment request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn submit_review(&self, pr: &PrRef, review: Review) -> Result<()> {
        // GitLab doesn't have a single "submit review" endpoint like GitHub.
        // Instead, we post a note and optionally approve/unapprove the MR.
        let project = Self::project_path(&pr.repo);

        // Post the review body as a note if non-empty.
        if !review.body.is_empty() {
            let notes_url = self.url(&format!(
                "/projects/{project}/merge_requests/{}/notes",
                pr.number
            ));
            let payload = GlNoteBody { body: &review.body };

            let resp = self
                .client
                .post(&notes_url)
                .json(&payload)
                .send()
                .await
                .map_err(|e| AgentError::Other(format!("GitLab submit_review note failed: {e}")))?;

            self.check_response(resp).await?;
        }

        // Handle the review event (approve / request changes).
        match review.event {
            ReviewEvent::Approve => {
                let approve_url = self.url(&format!(
                    "/projects/{project}/merge_requests/{}/approve",
                    pr.number
                ));
                let resp = self.client.post(&approve_url).send().await.map_err(|e| {
                    AgentError::Other(format!("GitLab approve request failed: {e}"))
                })?;
                self.check_response(resp).await?;
            }
            ReviewEvent::RequestChanges => {
                // GitLab has no direct "request changes" API. The convention
                // is to unapprove (revoke approval) which signals the MR
                // needs work. The review note above carries the feedback.
                let unapprove_url = self.url(&format!(
                    "/projects/{project}/merge_requests/{}/unapprove",
                    pr.number
                ));
                let resp = self.client.post(&unapprove_url).send().await.map_err(|e| {
                    AgentError::Other(format!("GitLab unapprove request failed: {e}"))
                })?;
                self.check_response(resp).await?;
            }
            ReviewEvent::Comment => {
                // Comment-only review: the note was already posted above.
            }
        }

        Ok(())
    }

    async fn list_checks(&self, sha: &str) -> Result<Vec<Check>> {
        // GitLab exposes CI pipeline status via commit statuses and pipelines.
        // We use the commit statuses endpoint which aggregates pipeline jobs.
        let project = Self::project_path(&self.repo);
        let url = self.url(&format!(
            "/projects/{project}/repository/commits/{sha}/statuses"
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitLab list_checks request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gl_statuses: Vec<GlCommitStatus> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab list_checks parse error: {e}")))?;

        Ok(gl_statuses.into_iter().map(Into::into).collect())
    }

    async fn list_issues(&self, repo: &ForgeRepo, q: IssueQuery) -> Result<Vec<Issue>> {
        let project = Self::project_path(repo);
        let state = match q.state.as_deref() {
            Some("open") | None => "opened",
            Some("closed") => "closed",
            Some(other) => other,
        };
        let per_page = q.limit.unwrap_or(30);

        let url = self.url(&format!(
            "/projects/{project}/issues?state={state}&per_page={per_page}"
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitLab list_issues request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gl_issues: Vec<GlIssue> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitLab list_issues parse error: {e}")))?;

        Ok(gl_issues.into_iter().map(Into::into).collect())
    }

    async fn get_pr_detail(&self, _pr_number: u64) -> Result<super::PrDetail> {
        Err(AgentError::Other(
            "get_pr_detail not yet implemented for GitLab".to_owned(),
        ))
    }

    async fn list_pr_files(&self, _pr_number: u64) -> Result<Vec<super::PrFile>> {
        Err(AgentError::Other(
            "list_pr_files not yet implemented for GitLab".to_owned(),
        ))
    }

    async fn get_pr_timeline(&self, _pr_number: u64) -> Result<Vec<super::PrTimelineEntry>> {
        Err(AgentError::Other(
            "get_pr_timeline not yet implemented for GitLab".to_owned(),
        ))
    }

    async fn get_pr_review_comments(&self, _pr_number: u64) -> Result<Vec<super::PrInlineComment>> {
        Err(AgentError::Other(
            "get_pr_review_comments not yet implemented for GitLab".to_owned(),
        ))
    }
}

// --- Internal helper types for GitLab API deserialization ---

#[derive(serde::Serialize)]
struct GlCreateMrBody<'a> {
    title: &'a str,
    description: &'a str,
    source_branch: &'a str,
    target_branch: &'a str,
    draft: bool,
}

#[derive(serde::Serialize)]
struct GlNoteBody<'a> {
    body: &'a str,
}

#[derive(Debug, Deserialize)]
struct GlMergeRequest {
    iid: u64,
    title: String,
    state: String,
    draft: Option<bool>,
    web_url: String,
    created_at: String,
    updated_at: String,
    author: Option<GlUser>,
    source_branch: String,
    target_branch: String,
}

#[derive(Debug, Deserialize)]
struct GlUser {
    username: String,
}

/// GitLab commit status (from the statuses API).
#[derive(Debug, Deserialize)]
struct GlCommitStatus {
    name: String,
    status: String,
    target_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlIssue {
    iid: u64,
    title: String,
    state: String,
    web_url: String,
    created_at: String,
    author: Option<GlUser>,
    labels: Option<Vec<String>>,
}

// --- Conversions from GitLab types to forge types ---

impl From<GlMergeRequest> for Pr {
    fn from(gl: GlMergeRequest) -> Self {
        let state = match gl.state.as_str() {
            "opened" => PrState::Open,
            "merged" => PrState::Merged,
            "closed" => PrState::Closed,
            _ => PrState::Closed,
        };

        Self {
            number: gl.iid,
            title: gl.title,
            state,
            author: gl.author.map(|u| u.username).unwrap_or_default(),
            head_branch: gl.source_branch,
            base_branch: gl.target_branch,
            url: gl.web_url,
            draft: gl.draft.unwrap_or(false),
            created_at: gl.created_at,
            updated_at: gl.updated_at,
        }
    }
}

impl From<GlCommitStatus> for Check {
    fn from(gl: GlCommitStatus) -> Self {
        let status = match gl.status.as_str() {
            "pending" | "created" => CheckStatus::Queued,
            "running" => CheckStatus::InProgress,
            "success" | "failed" | "canceled" | "skipped" => CheckStatus::Completed,
            _ => CheckStatus::Queued,
        };

        // GitLab commit statuses carry the conclusion in the same `status`
        // field. Map to the conclusion string the UI expects.
        let conclusion = match gl.status.as_str() {
            "success" => Some("success".to_owned()),
            "failed" => Some("failure".to_owned()),
            "canceled" => Some("cancelled".to_owned()),
            "skipped" => Some("skipped".to_owned()),
            _ => None,
        };

        Self {
            name: gl.name,
            status,
            url: gl.target_url,
            conclusion,
        }
    }
}

impl From<GlIssue> for Issue {
    fn from(gl: GlIssue) -> Self {
        // Normalise GitLab's "opened" state to "open" for consistency
        // with the forge-agnostic Issue type.
        let state = match gl.state.as_str() {
            "opened" => "open".to_owned(),
            other => other.to_owned(),
        };

        Self {
            number: gl.iid,
            title: gl.title,
            state,
            author: gl.author.map(|u| u.username).unwrap_or_default(),
            created_at: gl.created_at,
            url: gl.web_url,
            labels: gl.labels.unwrap_or_default(),
        }
    }
}
