//! Gitea forge implementation.
//!
//! Talks to the Gitea REST API v1 (self-hosted instances). Gitea's
//! API is modelled closely on GitHub's, so the structure mirrors
//! `github.rs`. Authentication is via a personal access token stored
//! in the OS keychain (Bearer header). Rate-limit handling checks
//! `X-RateLimit-Remaining` and honours `Retry-After`.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use tracing::warn;

use super::{
    Check, CheckStatus, CreatePrInput, Forge, ForgeRepo, Issue, IssueQuery, Pr, PrRef, PrState,
    Provider, Review, ReviewEvent,
};
use crate::{AgentError, Result};

/// Gitea REST API v1 implementation of the [`Forge`] trait.
///
/// Scoped to a single repository. Constructed with a token and a
/// base URL pointing to the Gitea instance (e.g.
/// `https://gitea.example.com/api/v1`). There is no default URL
/// since Gitea is always self-hosted.
pub struct GiteaForge {
    client: Client,
    base_url: String,
    repo: ForgeRepo,
}

impl GiteaForge {
    /// Create a new Gitea forge client.
    ///
    /// `token` is a personal access token. `base_url` is required and
    /// should point to the API root, e.g. `https://gitea.example.com/api/v1`.
    pub fn new(token: &str, base_url: &str, repo: ForgeRepo) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_owned();

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("crab-desktop-agent"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let auth_value = HeaderValue::from_str(&format!("token {token}"))
            .map_err(|e| AgentError::Other(format!("invalid Gitea token header value: {e}")))?;
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

    /// Build a full URL for a Gitea API endpoint path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Check rate-limit headers and log a warning when throttled.
    /// Returns `Ok(response)` if the request succeeded, or an error
    /// if the server returned a rate-limit (429) or other HTTP error.
    async fn check_response(&self, resp: Response) -> Result<Response> {
        // Gitea uses X-RateLimit-Remaining (same as GitHub).
        if let Some(remaining) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            if remaining == 0 {
                let reset = resp
                    .headers()
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown");
                warn!(
                    remaining,
                    reset, "Gitea rate limit exhausted, resets at epoch {reset}"
                );
            } else if remaining < 10 {
                warn!(remaining, "Gitea rate limit running low");
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
                "Gitea rate-limited (429), Retry-After: {retry_after}"
            );
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "Gitea API rate-limited (429): {body}"
            )));
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "Gitea API returned {status}: {body}"
            )));
        }

        Ok(resp)
    }
}

#[async_trait::async_trait]
impl Forge for GiteaForge {
    fn provider(&self) -> Provider {
        Provider::Gitea
    }

    async fn list_prs(&self, repo: &ForgeRepo) -> Result<Vec<Pr>> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls?state=open",
            repo.owner, repo.name
        ));

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_prs request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gt_prs: Vec<GtPullRequest> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_prs parse error: {e}")))?;

        Ok(gt_prs.into_iter().map(Into::into).collect())
    }

    async fn create_pr(&self, input: CreatePrInput) -> Result<Pr> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls",
            self.repo.owner, self.repo.name
        ));

        let body = GtCreatePrBody {
            title: &input.title,
            body: &input.body,
            head: &input.head,
            base: &input.base,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea create_pr request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gt_pr: GtPullRequest = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea create_pr parse error: {e}")))?;

        Ok(gt_pr.into())
    }

    async fn add_comment(&self, pr: &PrRef, body: &str) -> Result<()> {
        let url = self.url(&format!(
            "/repos/{}/{}/issues/{}/comments",
            pr.repo.owner, pr.repo.name, pr.number
        ));

        let payload = GtCommentBody { body };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea add_comment request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn submit_review(&self, pr: &PrRef, review: Review) -> Result<()> {
        // Gitea supports pull request reviews via the same endpoint
        // pattern as GitHub.
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            pr.repo.owner, pr.repo.name, pr.number
        ));

        let event = match review.event {
            ReviewEvent::Approve => "APPROVED",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        };

        let payload = GtReviewBody {
            body: &review.body,
            event,
        };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea submit_review request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn list_checks(&self, sha: &str) -> Result<Vec<Check>> {
        // Gitea exposes commit statuses via the same pattern as GitHub.
        let url = self.url(&format!(
            "/repos/{}/{}/statuses/{}",
            self.repo.owner, self.repo.name, sha
        ));

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_checks request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gt_statuses: Vec<GtCommitStatus> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_checks parse error: {e}")))?;

        Ok(gt_statuses.into_iter().map(Into::into).collect())
    }

    async fn list_issues(&self, repo: &ForgeRepo, q: IssueQuery) -> Result<Vec<Issue>> {
        let state = q.state.as_deref().unwrap_or("open");
        let limit = q.limit.unwrap_or(30);

        // Gitea uses `type=issues` to exclude pull requests from the
        // issues endpoint (PRs are also issues in Gitea, like GitHub).
        let url = self.url(&format!(
            "/repos/{}/{}/issues?state={}&type=issues&limit={}",
            repo.owner, repo.name, state, limit
        ));

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_issues request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gt_issues: Vec<GtIssue> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Gitea list_issues parse error: {e}")))?;

        Ok(gt_issues.into_iter().map(Into::into).collect())
    }

    async fn get_pr_detail(&self, _pr_number: u64) -> Result<super::PrDetail> {
        Err(AgentError::Other(
            "get_pr_detail not yet implemented for Gitea".to_owned(),
        ))
    }

    async fn list_pr_files(&self, _pr_number: u64) -> Result<Vec<super::PrFile>> {
        Err(AgentError::Other(
            "list_pr_files not yet implemented for Gitea".to_owned(),
        ))
    }

    async fn get_pr_timeline(&self, _pr_number: u64) -> Result<Vec<super::PrTimelineEntry>> {
        Err(AgentError::Other(
            "get_pr_timeline not yet implemented for Gitea".to_owned(),
        ))
    }

    async fn get_pr_review_comments(&self, _pr_number: u64) -> Result<Vec<super::PrInlineComment>> {
        Err(AgentError::Other(
            "get_pr_review_comments not yet implemented for Gitea".to_owned(),
        ))
    }
}

// --- Internal helper types for Gitea API serialization ---

#[derive(serde::Serialize)]
struct GtCreatePrBody<'a> {
    title: &'a str,
    body: &'a str,
    head: &'a str,
    base: &'a str,
}

#[derive(serde::Serialize)]
struct GtCommentBody<'a> {
    body: &'a str,
}

#[derive(serde::Serialize)]
struct GtReviewBody<'a> {
    body: &'a str,
    event: &'a str,
}

// --- Internal helper types for Gitea API deserialization ---

#[derive(Debug, Deserialize)]
struct GtPullRequest {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: Option<GtUser>,
    head: Option<GtBranch>,
    base: Option<GtBranch>,
}

#[derive(Debug, Deserialize)]
struct GtUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GtBranch {
    #[serde(rename = "ref")]
    ref_name: String,
}

/// Gitea commit status (from the statuses API).
#[derive(Debug, Deserialize)]
struct GtCommitStatus {
    context: String,
    status: String,
    target_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GtIssue {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    created_at: String,
    user: Option<GtUser>,
    labels: Option<Vec<GtLabel>>,
}

#[derive(Debug, Deserialize)]
struct GtLabel {
    name: String,
}

// --- Conversions from Gitea types to forge types ---

impl From<GtPullRequest> for Pr {
    fn from(gt: GtPullRequest) -> Self {
        let state = match gt.state.as_str() {
            "open" => PrState::Open,
            "closed" => PrState::Closed,
            _ => PrState::Closed,
        };

        Self {
            number: gt.number,
            title: gt.title,
            state,
            author: gt.user.map(|u| u.login).unwrap_or_default(),
            head_branch: gt.head.map(|b| b.ref_name).unwrap_or_default(),
            base_branch: gt.base.map(|b| b.ref_name).unwrap_or_default(),
            url: gt.html_url,
            // Gitea doesn't expose a dedicated draft field in the PR
            // list response (it uses title prefix "WIP:" convention).
            draft: false,
            created_at: gt.created_at,
            updated_at: gt.updated_at,
        }
    }
}

impl From<GtCommitStatus> for Check {
    fn from(gt: GtCommitStatus) -> Self {
        // Gitea statuses: pending, success, error, failure, warning.
        let (status, conclusion) = match gt.status.as_str() {
            "pending" => (CheckStatus::Queued, None),
            "success" => (CheckStatus::Completed, Some("success".to_owned())),
            "failure" => (CheckStatus::Completed, Some("failure".to_owned())),
            "error" => (CheckStatus::Completed, Some("failure".to_owned())),
            "warning" => (CheckStatus::Completed, Some("neutral".to_owned())),
            _ => (CheckStatus::Queued, None),
        };

        Self {
            name: gt.context,
            status,
            url: gt.target_url,
            conclusion,
        }
    }
}

impl From<GtIssue> for Issue {
    fn from(gt: GtIssue) -> Self {
        Self {
            number: gt.number,
            title: gt.title,
            state: gt.state,
            author: gt.user.map(|u| u.login).unwrap_or_default(),
            created_at: gt.created_at,
            url: gt.html_url,
            labels: gt
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
        }
    }
}
