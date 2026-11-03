//! GitHub forge implementation.
//!
//! Talks to the GitHub REST API v3 (or GitHub Enterprise with a custom
//! base URL). Authentication is via a personal access token or OAuth
//! token stored in the OS keychain. Rate-limit handling checks
//! `X-RateLimit-Remaining` and honours `Retry-After`.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use tracing::warn;

use super::{
    Check, CheckStatus, CreatePrInput, Forge, ForgeRepo, Issue, IssueQuery, Pr, PrDetail, PrFile,
    PrInlineComment, PrRef, PrState, PrTimelineEntry, Provider, Review, ReviewEvent,
};
use crate::{AgentError, Result};

/// GitHub REST API v3 implementation of the [`Forge`] trait.
///
/// Scoped to a single repository. Constructed with a token and
/// optional base URL (defaults to `https://api.github.com` for
/// github.com; set a custom URL for GitHub Enterprise).
pub struct GitHubForge {
    client: Client,
    base_url: String,
    repo: ForgeRepo,
}

impl GitHubForge {
    /// Create a new GitHub forge client.
    ///
    /// `token` is a PAT or OAuth token. `base_url` defaults to
    /// `https://api.github.com` when `None` (use a custom URL for
    /// GitHub Enterprise, e.g. `https://github.example.com/api/v3`).
    pub fn new(token: &str, base_url: Option<&str>, repo: ForgeRepo) -> Result<Self> {
        let base_url = base_url
            .unwrap_or("https://api.github.com")
            .trim_end_matches('/')
            .to_owned();

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("crab-desktop-agent"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );

        let auth_value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| AgentError::Other(format!("invalid GitHub token header value: {e}")))?;
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

    /// Build a full URL for a GitHub API endpoint path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Check rate-limit headers and log a warning when throttled.
    /// Returns `Ok(response)` if the request succeeded, or an error
    /// if the server returned a rate-limit (429) or other HTTP error.
    async fn check_response(&self, resp: Response) -> Result<Response> {
        // Log rate-limit state when remaining is low.
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
                    reset, "GitHub rate limit exhausted, resets at epoch {reset}"
                );
            } else if remaining < 10 {
                warn!(remaining, "GitHub rate limit running low");
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
                "GitHub rate-limited (429), Retry-After: {retry_after}"
            );
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "GitHub API rate-limited (429): {body}"
            )));
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "GitHub API returned {status}: {body}"
            )));
        }

        Ok(resp)
    }
}

#[async_trait::async_trait]
impl Forge for GitHubForge {
    fn provider(&self) -> Provider {
        Provider::GitHub
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
            .map_err(|e| AgentError::Other(format!("GitHub list_prs request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gh_prs: Vec<GhPullRequest> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub list_prs parse error: {e}")))?;

        Ok(gh_prs.into_iter().map(Into::into).collect())
    }

    async fn create_pr(&self, input: CreatePrInput) -> Result<Pr> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls",
            self.repo.owner, self.repo.name
        ));

        let body = GhCreatePrBody {
            title: &input.title,
            body: &input.body,
            head: &input.head,
            base: &input.base,
            draft: input.draft,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub create_pr request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let gh_pr: GhPullRequest = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub create_pr parse error: {e}")))?;

        Ok(gh_pr.into())
    }

    async fn add_comment(&self, pr: &PrRef, body: &str) -> Result<()> {
        let url = self.url(&format!(
            "/repos/{}/{}/issues/{}/comments",
            pr.repo.owner, pr.repo.name, pr.number
        ));

        let payload = GhCommentBody { body };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub add_comment request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn submit_review(&self, pr: &PrRef, review: Review) -> Result<()> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            pr.repo.owner, pr.repo.name, pr.number
        ));

        let event = match review.event {
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        };

        let payload = GhReviewBody {
            body: &review.body,
            event,
        };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub submit_review request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn list_checks(&self, sha: &str) -> Result<Vec<Check>> {
        let url = self.url(&format!(
            "/repos/{}/{}/commits/{}/check-runs",
            self.repo.owner, self.repo.name, sha
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitHub list_checks request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gh_checks: GhCheckRunsResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub list_checks parse error: {e}")))?;

        Ok(gh_checks.check_runs.into_iter().map(Into::into).collect())
    }

    async fn list_issues(&self, repo: &ForgeRepo, q: IssueQuery) -> Result<Vec<Issue>> {
        let state = q.state.as_deref().unwrap_or("open");
        let per_page = q.limit.unwrap_or(30);

        let url = self.url(&format!(
            "/repos/{}/{}/issues?state={}&per_page={}",
            repo.owner, repo.name, state, per_page
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitHub list_issues request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gh_issues: Vec<GhIssue> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub list_issues parse error: {e}")))?;

        // GitHub's issues endpoint also returns PRs; filter them out.
        Ok(gh_issues
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .map(Into::into)
            .collect())
    }

    async fn get_pr_detail(&self, pr_number: u64) -> Result<PrDetail> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}",
            self.repo.owner, self.repo.name, pr_number
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitHub get_pr_detail request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gh: GhPrFull = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub get_pr_detail parse error: {e}")))?;

        let state = match gh.state.as_str() {
            "open" => PrState::Open,
            "closed" if gh.merged.unwrap_or(false) => PrState::Merged,
            _ => PrState::Closed,
        };

        Ok(PrDetail {
            number: gh.number,
            title: gh.title,
            body: gh.body.unwrap_or_default(),
            state,
            draft: gh.draft.unwrap_or(false),
            author: gh.user.map(|u| u.login).unwrap_or_default(),
            head_branch: gh.head.ref_name,
            head_sha: gh.head.sha,
            base_branch: gh.base.ref_name,
            url: gh.html_url,
            created_at: gh.created_at,
            updated_at: gh.updated_at,
            labels: gh.labels.into_iter().map(|l| l.name).collect(),
            mergeable: gh.mergeable,
            additions: gh.additions.unwrap_or(0),
            deletions: gh.deletions.unwrap_or(0),
            changed_files_count: gh.changed_files.unwrap_or(0),
        })
    }

    async fn list_pr_files(&self, pr_number: u64) -> Result<Vec<PrFile>> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/files?per_page=100",
            self.repo.owner, self.repo.name, pr_number
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("GitHub list_pr_files request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let gh_files: Vec<GhPrFile> = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("GitHub list_pr_files parse error: {e}")))?;

        Ok(gh_files.into_iter().map(Into::into).collect())
    }

    async fn get_pr_timeline(&self, pr_number: u64) -> Result<Vec<PrTimelineEntry>> {
        let mut timeline = Vec::new();

        // Fetch issue comments (general PR comments).
        let comments_url = self.url(&format!(
            "/repos/{}/{}/issues/{}/comments?per_page=100",
            self.repo.owner, self.repo.name, pr_number
        ));
        if let Ok(resp) = self.client.get(&comments_url).send().await {
            if let Ok(resp) = self.check_response(resp).await {
                if let Ok(comments) = resp.json::<Vec<GhIssueComment>>().await {
                    for c in comments {
                        timeline.push(PrTimelineEntry {
                            id: c.id.to_string(),
                            kind: "comment".to_owned(),
                            author: c.user.map(|u| u.login).unwrap_or_default(),
                            body: Some(c.body),
                            created_at: c.created_at,
                            verdict: None,
                            sha: None,
                            subject: None,
                        });
                    }
                }
            }
        }

        // Fetch reviews.
        let reviews_url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews?per_page=100",
            self.repo.owner, self.repo.name, pr_number
        ));
        if let Ok(resp) = self.client.get(&reviews_url).send().await {
            if let Ok(resp) = self.check_response(resp).await {
                if let Ok(reviews) = resp.json::<Vec<GhReview>>().await {
                    for r in reviews {
                        let verdict = match r.state.as_str() {
                            "APPROVED" => Some("approved".to_owned()),
                            "CHANGES_REQUESTED" => Some("changes_requested".to_owned()),
                            "COMMENTED" => Some("commented".to_owned()),
                            _ => None,
                        };
                        timeline.push(PrTimelineEntry {
                            id: r.id.to_string(),
                            kind: "review".to_owned(),
                            author: r.user.map(|u| u.login).unwrap_or_default(),
                            body: if r.body.is_empty() {
                                None
                            } else {
                                Some(r.body)
                            },
                            created_at: r.submitted_at.unwrap_or_default(),
                            verdict,
                            sha: None,
                            subject: None,
                        });
                    }
                }
            }
        }

        // Sort by created_at.
        timeline.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(timeline)
    }

    async fn get_pr_review_comments(&self, pr_number: u64) -> Result<Vec<PrInlineComment>> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/comments?per_page=100",
            self.repo.owner, self.repo.name, pr_number
        ));

        let resp = self.client.get(&url).send().await.map_err(|e| {
            AgentError::Other(format!("GitHub get_pr_review_comments request failed: {e}"))
        })?;

        let resp = self.check_response(resp).await?;

        let gh_comments: Vec<GhPrReviewComment> = resp.json().await.map_err(|e| {
            AgentError::Other(format!("GitHub get_pr_review_comments parse error: {e}"))
        })?;

        Ok(gh_comments.into_iter().map(Into::into).collect())
    }
}

// --- Internal helper types for GitHub API deserialization ---

#[derive(serde::Serialize)]
struct GhCreatePrBody<'a> {
    title: &'a str,
    body: &'a str,
    head: &'a str,
    base: &'a str,
    draft: bool,
}

#[derive(serde::Serialize)]
struct GhCommentBody<'a> {
    body: &'a str,
}

#[derive(serde::Serialize)]
struct GhReviewBody<'a> {
    body: &'a str,
    event: &'a str,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    number: u64,
    title: String,
    state: String,
    draft: Option<bool>,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: Option<GhUser>,
    head: GhRef,
    base: GhRef,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhRef {
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GhCheckRunsResponse {
    check_runs: Vec<GhCheckRun>,
}

#[derive(Debug, Deserialize)]
struct GhCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    created_at: String,
    user: Option<GhUser>,
    labels: Option<Vec<GhLabel>>,
    pull_request: Option<GhIssuePrRef>,
}

/// Marker struct — if present, the "issue" is actually a PR.
#[derive(Debug, Deserialize)]
struct GhIssuePrRef {
    #[allow(dead_code)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

// --- Conversions from GitHub types to forge types ---

impl From<GhPullRequest> for Pr {
    fn from(gh: GhPullRequest) -> Self {
        let state = match gh.state.as_str() {
            "open" => PrState::Open,
            "closed" => PrState::Closed,
            _ => PrState::Closed,
        };

        Self {
            number: gh.number,
            title: gh.title,
            state,
            author: gh.user.map(|u| u.login).unwrap_or_default(),
            head_branch: gh.head.ref_name,
            base_branch: gh.base.ref_name,
            url: gh.html_url,
            draft: gh.draft.unwrap_or(false),
            created_at: gh.created_at,
            updated_at: gh.updated_at,
        }
    }
}

impl From<GhCheckRun> for Check {
    fn from(gh: GhCheckRun) -> Self {
        let status = match gh.status.as_str() {
            "queued" => CheckStatus::Queued,
            "in_progress" => CheckStatus::InProgress,
            "completed" => CheckStatus::Completed,
            _ => CheckStatus::Queued,
        };

        Self {
            name: gh.name,
            status,
            url: gh.html_url,
            conclusion: gh.conclusion,
        }
    }
}

impl From<GhIssue> for Issue {
    fn from(gh: GhIssue) -> Self {
        Self {
            number: gh.number,
            title: gh.title,
            state: gh.state,
            author: gh.user.map(|u| u.login).unwrap_or_default(),
            created_at: gh.created_at,
            url: gh.html_url,
            labels: gh
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
        }
    }
}

// --- Full PR detail types ---

/// Full PR response from GET /repos/{owner}/{repo}/pulls/{number}
#[derive(Debug, Deserialize)]
struct GhPrFull {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    draft: Option<bool>,
    merged: Option<bool>,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: Option<GhUser>,
    head: GhRef,
    base: GhRef,
    labels: Vec<GhLabel>,
    mergeable: Option<bool>,
    additions: Option<u64>,
    deletions: Option<u64>,
    changed_files: Option<u64>,
}

/// File entry from GET /repos/{owner}/{repo}/pulls/{number}/files
#[derive(Debug, Deserialize)]
struct GhPrFile {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
    patch: Option<String>,
}

impl From<GhPrFile> for PrFile {
    fn from(gh: GhPrFile) -> Self {
        Self {
            path: gh.filename,
            status: gh.status,
            additions: gh.additions,
            deletions: gh.deletions,
            patch: gh.patch,
        }
    }
}

/// Issue comment from GET /repos/{owner}/{repo}/issues/{number}/comments
#[derive(Debug, Deserialize)]
struct GhIssueComment {
    id: u64,
    body: String,
    created_at: String,
    user: Option<GhUser>,
}

/// Review from GET /repos/{owner}/{repo}/pulls/{number}/reviews
#[derive(Debug, Deserialize)]
struct GhReview {
    id: u64,
    state: String,
    body: String,
    submitted_at: Option<String>,
    user: Option<GhUser>,
}

/// Inline review comment from GET /repos/{owner}/{repo}/pulls/{number}/comments
#[derive(Debug, Deserialize)]
struct GhPrReviewComment {
    id: u64,
    path: String,
    /// The line in the diff this comment applies to.
    #[serde(default)]
    line: Option<u64>,
    /// Legacy field — original_line or position-based.
    #[serde(default)]
    original_line: Option<u64>,
    body: String,
    created_at: String,
    user: Option<GhUser>,
}

impl From<GhPrReviewComment> for PrInlineComment {
    fn from(gh: GhPrReviewComment) -> Self {
        // Prefer `line`, fall back to `original_line`, default to 0.
        let line = gh.line.or(gh.original_line).unwrap_or(0);
        Self {
            id: gh.id.to_string(),
            path: gh.path,
            line,
            author: gh.user.map(|u| u.login).unwrap_or_default(),
            body: gh.body,
            created_at: gh.created_at,
        }
    }
}
