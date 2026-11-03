//! Bitbucket Cloud forge implementation.
//!
//! Talks to the Bitbucket Cloud REST API v2 (bitbucket.org).
//! Authentication is via an OAuth token or app password stored in
//! the OS keychain. Rate-limit handling honours `Retry-After`.

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use tracing::warn;

use super::{
    Check, CheckStatus, CreatePrInput, Forge, ForgeRepo, Issue, IssueQuery, Pr, PrRef, PrState,
    Provider, Review, ReviewEvent,
};
use crate::{AgentError, Result};

/// Bitbucket Cloud REST API v2 implementation of the [`Forge`] trait.
///
/// Scoped to a single repository. Constructed with a token (OAuth or
/// app password) and optional base URL (defaults to
/// `https://api.bitbucket.org/2.0` for bitbucket.org).
pub struct BitbucketForge {
    client: Client,
    base_url: String,
    repo: ForgeRepo,
}

impl BitbucketForge {
    /// Create a new Bitbucket Cloud forge client.
    ///
    /// `token` is an OAuth token or app password. `base_url` defaults
    /// to `https://api.bitbucket.org/2.0` when `None`.
    pub fn new(token: &str, base_url: Option<&str>, repo: ForgeRepo) -> Result<Self> {
        let base_url = base_url
            .unwrap_or("https://api.bitbucket.org/2.0")
            .trim_end_matches('/')
            .to_owned();

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("crab-desktop-agent"));

        let auth_value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| AgentError::Other(format!("invalid Bitbucket token header value: {e}")))?;
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

    /// Build a full URL for a Bitbucket API endpoint path.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Check rate-limit headers and log a warning when throttled.
    /// Returns `Ok(response)` if the request succeeded, or an error
    /// if the server returned a rate-limit (429) or other HTTP error.
    async fn check_response(&self, resp: Response) -> Result<Response> {
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
                "Bitbucket rate-limited (429), Retry-After: {retry_after}"
            );
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "Bitbucket API rate-limited (429): {body}"
            )));
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "Bitbucket API returned {status}: {body}"
            )));
        }

        Ok(resp)
    }
}

#[async_trait::async_trait]
impl Forge for BitbucketForge {
    fn provider(&self) -> Provider {
        Provider::Bitbucket
    }

    async fn list_prs(&self, repo: &ForgeRepo) -> Result<Vec<Pr>> {
        let url = self.url(&format!(
            "/repositories/{}/{}/pullrequests?state=OPEN",
            repo.owner, repo.name
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("Bitbucket list_prs request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let paginated: BbPaginatedPrs = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket list_prs parse error: {e}")))?;

        Ok(paginated.values.into_iter().map(Into::into).collect())
    }

    async fn create_pr(&self, input: CreatePrInput) -> Result<Pr> {
        let url = self.url(&format!(
            "/repositories/{}/{}/pullrequests",
            self.repo.owner, self.repo.name
        ));

        let body = BbCreatePrBody {
            title: &input.title,
            description: &input.body,
            source: BbBranchRef {
                branch: BbBranchName { name: &input.head },
            },
            destination: BbBranchRef {
                branch: BbBranchName { name: &input.base },
            },
            close_source_branch: false,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket create_pr request failed: {e}")))?;

        let resp = self.check_response(resp).await?;

        let bb_pr: BbPullRequest = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket create_pr parse error: {e}")))?;

        Ok(bb_pr.into())
    }

    async fn add_comment(&self, pr: &PrRef, body: &str) -> Result<()> {
        let url = self.url(&format!(
            "/repositories/{}/{}/pullrequests/{}/comments",
            pr.repo.owner, pr.repo.name, pr.number
        ));

        let payload = BbCommentBody {
            content: BbContent { raw: body },
        };

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket add_comment request failed: {e}")))?;

        self.check_response(resp).await?;
        Ok(())
    }

    async fn submit_review(&self, pr: &PrRef, review: Review) -> Result<()> {
        // Bitbucket Cloud doesn't have a single "submit review" endpoint
        // like GitHub. We post a comment for the review body and then
        // approve/unapprove the PR as needed.

        // Post the review body as a comment if non-empty.
        if !review.body.is_empty() {
            self.add_comment(pr, &review.body).await?;
        }

        match review.event {
            ReviewEvent::Approve => {
                let url = self.url(&format!(
                    "/repositories/{}/{}/pullrequests/{}/approve",
                    pr.repo.owner, pr.repo.name, pr.number
                ));
                let resp = self.client.post(&url).send().await.map_err(|e| {
                    AgentError::Other(format!("Bitbucket approve request failed: {e}"))
                })?;
                self.check_response(resp).await?;
            }
            ReviewEvent::RequestChanges => {
                // Bitbucket Cloud uses "request changes" via the
                // pullrequests/{id}/request-changes endpoint (API 2.0).
                let url = self.url(&format!(
                    "/repositories/{}/{}/pullrequests/{}/request-changes",
                    pr.repo.owner, pr.repo.name, pr.number
                ));
                let resp = self.client.post(&url).send().await.map_err(|e| {
                    AgentError::Other(format!("Bitbucket request-changes request failed: {e}"))
                })?;
                self.check_response(resp).await?;
            }
            ReviewEvent::Comment => {
                // Comment-only: the comment was already posted above.
            }
        }

        Ok(())
    }

    async fn list_checks(&self, sha: &str) -> Result<Vec<Check>> {
        // Bitbucket Cloud exposes build statuses (pipelines) via the
        // commit statuses endpoint.
        let url = self.url(&format!(
            "/repositories/{}/{}/commit/{}/statuses",
            self.repo.owner, self.repo.name, sha
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("Bitbucket list_checks request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let paginated: BbPaginatedStatuses = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket list_checks parse error: {e}")))?;

        Ok(paginated.values.into_iter().map(Into::into).collect())
    }

    async fn list_issues(&self, repo: &ForgeRepo, q: IssueQuery) -> Result<Vec<Issue>> {
        // Bitbucket Cloud issue tracker (must be enabled per-repo).
        // Uses a query parameter to filter by state.
        let state = match q.state.as_deref() {
            Some("open") | None => "new",
            Some("closed") => "resolved",
            Some(other) => other,
        };
        let limit = q.limit.unwrap_or(30);

        let url = self.url(&format!(
            "/repositories/{}/{}/issues?q=state=\"{state}\"&pagelen={limit}",
            repo.owner, repo.name
        ));

        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                AgentError::Other(format!("Bitbucket list_issues request failed: {e}"))
            })?;

        let resp = self.check_response(resp).await?;

        let paginated: BbPaginatedIssues = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("Bitbucket list_issues parse error: {e}")))?;

        Ok(paginated.values.into_iter().map(Into::into).collect())
    }

    async fn get_pr_detail(&self, _pr_number: u64) -> Result<super::PrDetail> {
        Err(AgentError::Other(
            "get_pr_detail not yet implemented for Bitbucket".to_owned(),
        ))
    }

    async fn list_pr_files(&self, _pr_number: u64) -> Result<Vec<super::PrFile>> {
        Err(AgentError::Other(
            "list_pr_files not yet implemented for Bitbucket".to_owned(),
        ))
    }

    async fn get_pr_timeline(&self, _pr_number: u64) -> Result<Vec<super::PrTimelineEntry>> {
        Err(AgentError::Other(
            "get_pr_timeline not yet implemented for Bitbucket".to_owned(),
        ))
    }

    async fn get_pr_review_comments(&self, _pr_number: u64) -> Result<Vec<super::PrInlineComment>> {
        Err(AgentError::Other(
            "get_pr_review_comments not yet implemented for Bitbucket".to_owned(),
        ))
    }
}

// --- Internal helper types for Bitbucket API serialization ---

#[derive(serde::Serialize)]
struct BbCreatePrBody<'a> {
    title: &'a str,
    description: &'a str,
    source: BbBranchRef<'a>,
    destination: BbBranchRef<'a>,
    close_source_branch: bool,
}

#[derive(serde::Serialize)]
struct BbBranchRef<'a> {
    branch: BbBranchName<'a>,
}

#[derive(serde::Serialize)]
struct BbBranchName<'a> {
    name: &'a str,
}

#[derive(serde::Serialize)]
struct BbCommentBody<'a> {
    content: BbContent<'a>,
}

#[derive(serde::Serialize)]
struct BbContent<'a> {
    raw: &'a str,
}

// --- Internal helper types for Bitbucket API deserialization ---

/// Bitbucket paginated response wrapper for pull requests.
#[derive(Debug, Deserialize)]
struct BbPaginatedPrs {
    values: Vec<BbPullRequest>,
}

#[derive(Debug, Deserialize)]
struct BbPullRequest {
    id: u64,
    title: String,
    state: String,
    author: Option<BbUser>,
    source: BbPrBranch,
    destination: BbPrBranch,
    links: BbPrLinks,
    created_on: String,
    updated_on: String,
}

#[derive(Debug, Deserialize)]
struct BbUser {
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct BbPrBranch {
    branch: BbPrBranchName,
}

#[derive(Debug, Deserialize)]
struct BbPrBranchName {
    name: String,
}

#[derive(Debug, Deserialize)]
struct BbPrLinks {
    html: BbLink,
}

#[derive(Debug, Deserialize)]
struct BbLink {
    href: String,
}

/// Bitbucket paginated response wrapper for commit statuses.
#[derive(Debug, Deserialize)]
struct BbPaginatedStatuses {
    values: Vec<BbCommitStatus>,
}

#[derive(Debug, Deserialize)]
struct BbCommitStatus {
    name: String,
    state: String,
    url: Option<String>,
}

/// Bitbucket paginated response wrapper for issues.
#[derive(Debug, Deserialize)]
struct BbPaginatedIssues {
    values: Vec<BbIssue>,
}

#[derive(Debug, Deserialize)]
struct BbIssue {
    id: u64,
    title: String,
    state: String,
    created_on: Option<String>,
    reporter: Option<BbUser>,
    links: BbIssueLinks,
}

#[derive(Debug, Deserialize)]
struct BbIssueLinks {
    html: BbLink,
}

// --- Conversions from Bitbucket types to forge types ---

impl From<BbPullRequest> for Pr {
    fn from(bb: BbPullRequest) -> Self {
        let state = match bb.state.as_str() {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "DECLINED" | "SUPERSEDED" => PrState::Closed,
            _ => PrState::Closed,
        };

        Self {
            number: bb.id,
            title: bb.title,
            state,
            author: bb.author.map(|u| u.display_name).unwrap_or_default(),
            head_branch: bb.source.branch.name,
            base_branch: bb.destination.branch.name,
            url: bb.links.html.href,
            // Bitbucket Cloud doesn't have a native "draft" concept.
            draft: false,
            created_at: bb.created_on,
            updated_at: bb.updated_on,
        }
    }
}

impl From<BbCommitStatus> for Check {
    fn from(bb: BbCommitStatus) -> Self {
        // Bitbucket statuses: INPROGRESS, SUCCESSFUL, FAILED, STOPPED.
        let (status, conclusion) = match bb.state.as_str() {
            "INPROGRESS" => (CheckStatus::InProgress, None),
            "SUCCESSFUL" => (CheckStatus::Completed, Some("success".to_owned())),
            "FAILED" => (CheckStatus::Completed, Some("failure".to_owned())),
            "STOPPED" => (CheckStatus::Completed, Some("cancelled".to_owned())),
            _ => (CheckStatus::Queued, None),
        };

        Self {
            name: bb.name,
            status,
            url: bb.url,
            conclusion,
        }
    }
}

impl From<BbIssue> for Issue {
    fn from(bb: BbIssue) -> Self {
        // Normalise Bitbucket states ("new", "open", "resolved", "closed",
        // etc.) to the forge-agnostic convention.
        let state = match bb.state.as_str() {
            "new" | "open" => "open".to_owned(),
            "resolved" | "closed" | "invalid" | "duplicate" | "wontfix" => "closed".to_owned(),
            other => other.to_owned(),
        };

        Self {
            number: bb.id,
            title: bb.title,
            state,
            author: bb.reporter.map(|u| u.display_name).unwrap_or_default(),
            created_at: bb.created_on.unwrap_or_default(),
            url: bb.links.html.href,
            labels: Vec::new(), // Bitbucket issues don't have labels
        }
    }
}
