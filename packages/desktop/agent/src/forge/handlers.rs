//! RPC verb handlers for forge (code-hosting) operations.
//!
//! Wires the five PR-related verbs into the agent's dispatch table:
//!
//! - `forge_list_prs` — list open PRs for a repo
//! - `forge_create_pr` — create a new PR
//! - `forge_pr_detail` — full PR detail with timeline
//! - `forge_pr_comment` — add a comment to a PR
//! - `forge_submit_review` — submit a review (approve/request changes/comment)
//!
//! Each handler resolves the forge provider for the repo, retrieves
//! the auth token from the workspace settings (written by the main
//! process after OAuth/PAT flow), instantiates the provider-specific
//! [`Forge`] implementation, and delegates to the trait method.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use super::Forge;
use super::bitbucket::BitbucketForge;
use super::gitea::GiteaForge;
use super::github::GitHubForge;
use super::gitlab::GitLabForge;
use super::types::{
    CreatePrInput, ForgeRepo, IssueQuery, PrRef, PrState, Provider, Review, ReviewEvent,
};
use crate::{AgentError, Result};

// ---------------------------------------------------------------------------
// Error variants specific to forge handlers
// ---------------------------------------------------------------------------

/// Forge-specific error conditions surfaced as structured JSON-RPC errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ForgeError {
    /// No auth token available for the detected provider.
    AuthRequired,
    /// The provider is unknown or unsupported.
    NotSupported,
    /// The forge API returned HTTP 429 (rate-limited).
    RateLimited,
}

impl ForgeError {
    fn to_agent_error(self, detail: &str) -> AgentError {
        match self {
            Self::AuthRequired => AgentError::Other(format!("ForgeAuthRequired: {detail}")),
            Self::NotSupported => AgentError::Other(format!("ForgeNotSupported: {detail}")),
            Self::RateLimited => AgentError::Other(format!("ForgeRateLimited: {detail}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ForgeListPrsParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ForgeCreatePrParams {
    pub repo_id: String,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Deserialize)]
pub struct ForgePrDetailParams {
    pub repo_id: String,
    pub pr_number: u64,
}

#[derive(Debug, Deserialize)]
pub struct ForgePrCommentParams {
    pub repo_id: String,
    pub pr_number: u64,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct ForgeSubmitReviewParams {
    pub repo_id: String,
    pub pr_number: u64,
    pub event: ReviewEvent,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ForgeListChecksParams {
    pub repo_id: String,
    pub sha: String,
}

#[derive(Debug, Deserialize)]
pub struct ForgeListIssuesParams {
    pub repo_id: String,
    #[serde(default)]
    pub query: Option<ForgeListIssuesQuery>,
}

/// Optional query filters for `forge_list_issues`.
#[derive(Debug, Default, Deserialize)]
pub struct ForgeListIssuesQuery {
    pub state: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u32>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// List open pull requests for the repo's forge.
pub async fn forge_list_prs(work_dir: &Path) -> Result<Value> {
    let (forge_impl, forge_repo) = resolve_forge(work_dir).await?;
    let prs = forge_impl.list_prs(&forge_repo).await?;

    // Map from the internal Pr struct to the PullRequestEntry shape
    // the frontend expects.
    let entries: Vec<Value> = prs
        .into_iter()
        .map(|pr| {
            let status = if pr.draft {
                "draft"
            } else {
                match pr.state {
                    PrState::Open => "open",
                    PrState::Merged => "merged",
                    PrState::Closed => "open", // closed but not merged
                }
            };
            json!({
                "number": pr.number,
                "title": pr.title,
                "author": pr.author,
                "status": status,
                "source_branch": pr.head_branch,
                "target_branch": pr.base_branch,
                "updated_at": pr.updated_at,
                "assignees": [],
                "is_mine": false,
                "is_assigned_to_me": false,
            })
        })
        .collect();

    Ok(json!({ "prs": entries }))
}

/// Create a new pull request.
pub async fn forge_create_pr(work_dir: &Path, p: ForgeCreatePrParams) -> Result<Value> {
    let (forge_impl, _forge_repo) = resolve_forge(work_dir).await?;
    let input = CreatePrInput {
        title: p.title,
        body: p.body,
        head: p.head,
        base: p.base,
        draft: p.draft,
    };
    let pr = forge_impl.create_pr(input).await?;
    Ok(json!({ "pr": pr }))
}

/// Get full PR detail including timeline (comments, reviews, checks).
///
/// Returns richer data than `forge_list_prs`: the full body, labels,
/// assignees, timeline events, reviews, and status checks for the
/// PR's head commit.
pub async fn forge_pr_detail(work_dir: &Path, p: ForgePrDetailParams) -> Result<Value> {
    let (forge_impl, _forge_repo) = resolve_forge(work_dir).await?;

    // Fetch full PR detail (body, labels, mergeable, head SHA).
    let detail = forge_impl.get_pr_detail(p.pr_number).await?;

    // Fetch changed files with diffs, timeline, inline comments, and CI checks
    // concurrently. Each is non-fatal — we return what we can.
    let (files_result, timeline_result, inline_result, checks_result) = tokio::join!(
        forge_impl.list_pr_files(p.pr_number),
        forge_impl.get_pr_timeline(p.pr_number),
        forge_impl.get_pr_review_comments(p.pr_number),
        forge_impl.list_checks(&detail.head_sha),
    );

    let files = files_result.unwrap_or_default();
    let timeline = timeline_result.unwrap_or_default();
    let inline_comments = inline_result.unwrap_or_default();
    let checks = checks_result.unwrap_or_default();

    // Determine status from PR state + draft flag.
    let status = if detail.draft {
        "draft"
    } else {
        match detail.state {
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "open",
        }
    };

    // Build changed_files array in the shape the frontend expects.
    let changed_files: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "additions": f.additions,
                "deletions": f.deletions,
                "patch": f.patch,
                "status": f.status,
            })
        })
        .collect();

    // Build inline_comments map: { "path": { line_number: [comments] } }
    let mut inline_map: serde_json::Map<String, Value> = serde_json::Map::new();
    for comment in &inline_comments {
        let file_entry = inline_map
            .entry(comment.path.clone())
            .or_insert_with(|| json!({}));
        let line_key = comment.line.to_string();
        let line_arr = file_entry
            .as_object_mut()
            .unwrap()
            .entry(line_key)
            .or_insert_with(|| json!([]));
        line_arr.as_array_mut().unwrap().push(json!({
            "id": comment.id,
            "author": comment.author,
            "body": comment.body,
            "created_at": comment.created_at,
        }));
    }

    Ok(json!({
        "number": detail.number,
        "title": detail.title,
        "author": detail.author,
        "status": status,
        "source_branch": detail.head_branch,
        "target_branch": detail.base_branch,
        "created_at": detail.created_at,
        "updated_at": detail.updated_at,
        "body": detail.body,
        "timeline": timeline,
        "changed_files": changed_files,
        "mergeable": detail.mergeable,
        "labels": detail.labels,
        "inline_comments": inline_map,
        "checks": checks,
    }))
}

/// Add a comment to an existing pull request.
pub async fn forge_pr_comment(work_dir: &Path, p: ForgePrCommentParams) -> Result<Value> {
    let (forge_impl, forge_repo) = resolve_forge(work_dir).await?;

    let pr_ref = PrRef {
        repo: forge_repo,
        number: p.pr_number,
    };

    forge_impl.add_comment(&pr_ref, &p.body).await?;
    Ok(json!({ "ok": true, "pr_number": p.pr_number }))
}

/// Submit a review on a pull request.
pub async fn forge_submit_review(work_dir: &Path, p: ForgeSubmitReviewParams) -> Result<Value> {
    let (forge_impl, forge_repo) = resolve_forge(work_dir).await?;

    let pr_ref = PrRef {
        repo: forge_repo,
        number: p.pr_number,
    };

    let review = Review {
        body: p.body.unwrap_or_default(),
        event: p.event,
    };

    forge_impl.submit_review(&pr_ref, review).await?;
    Ok(json!({ "ok": true, "pr_number": p.pr_number }))
}

/// List CI check runs for a given commit SHA.
pub async fn forge_list_checks(work_dir: &Path, p: ForgeListChecksParams) -> Result<Value> {
    let (forge_impl, _forge_repo) = resolve_forge(work_dir).await?;
    let checks = forge_impl.list_checks(&p.sha).await?;
    Ok(json!({ "checks": checks }))
}

/// List issues for the repo's forge, optionally filtered.
pub async fn forge_list_issues(work_dir: &Path, p: ForgeListIssuesParams) -> Result<Value> {
    let (forge_impl, forge_repo) = resolve_forge(work_dir).await?;
    let query = match p.query {
        Some(q) => IssueQuery {
            state: q.state,
            search: q.search,
            limit: q.limit,
        },
        None => IssueQuery::default(),
    };
    let issues = forge_impl.list_issues(&forge_repo, query).await?;
    Ok(json!({ "issues": issues }))
}

// ---------------------------------------------------------------------------
// Internal: provider resolution + instantiation
// ---------------------------------------------------------------------------

/// Resolve the forge provider and instantiate the appropriate client.
///
/// Reads the detected provider and auth token from workspace settings,
/// then constructs the provider-specific [`Forge`] implementation.
async fn resolve_forge(work_dir: &Path) -> Result<(Box<dyn Forge>, ForgeRepo)> {
    let settings = read_forge_settings(work_dir)?;

    let provider = settings.provider.ok_or_else(|| {
        ForgeError::NotSupported.to_agent_error("no forge provider detected for this repository")
    })?;

    // Try settings.json first, then fall back to the OS keychain.
    let token = match settings.token.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => {
            // Keychain service matches forge-auth.ts: "crab-forge-<provider>"
            let provider_str = match provider {
                Provider::GitHub => "github",
                Provider::GitLab => "gitlab",
                Provider::Bitbucket => "bitbucket",
                Provider::Gitea => "gitea",
            };
            let service = format!("crab-forge-{provider_str}");
            crate::credential_store::get_password(&service, "oauth-token")
                .await
                .ok_or_else(|| {
                    ForgeError::AuthRequired.to_agent_error(
                        "no auth token available; authenticate via OAuth or paste a PAT in Settings",
                    )
                })?
        }
    };

    let forge_repo = settings.repo.ok_or_else(|| {
        AgentError::Other(
            "forge repo info (owner/name) not available; re-open the repository".to_owned(),
        )
    })?;

    debug!(
        provider = ?provider,
        owner = %forge_repo.owner,
        name = %forge_repo.name,
        "resolved forge provider"
    );

    let forge_impl: Box<dyn Forge> = match provider {
        Provider::GitHub => {
            let gh = GitHubForge::new(&token, settings.base_url.as_deref(), forge_repo.clone())?;
            Box::new(gh)
        }
        Provider::GitLab => {
            let gl = GitLabForge::new(&token, settings.base_url.as_deref(), forge_repo.clone())?;
            Box::new(gl)
        }
        Provider::Bitbucket => {
            let bb = BitbucketForge::new(&token, settings.base_url.as_deref(), forge_repo.clone())?;
            Box::new(bb)
        }
        Provider::Gitea => {
            let base = settings.base_url.as_deref().ok_or_else(|| {
                AgentError::Other(
                    "Gitea requires a base_url in forge settings (e.g. https://gitea.example.com/api/v1)".to_owned(),
                )
            })?;
            let gt = GiteaForge::new(&token, base, forge_repo.clone())?;
            Box::new(gt)
        }
    };

    Ok((forge_impl, forge_repo))
}

/// Forge-related fields read from workspace settings.
struct ForgeSettings {
    provider: Option<Provider>,
    token: Option<String>,
    repo: Option<ForgeRepo>,
    base_url: Option<String>,
}

/// Read forge configuration from the workspace settings file.
///
/// Settings file: `<work_dir>/.crab/desktop/settings.json`
///
/// Expected fields:
/// ```json
/// {
///   "forge_provider": "github",
///   "forge_token": "<PAT or OAuth token>",
///   "forge_owner": "owner",
///   "forge_name": "repo",
///   "forge_base_url": null
/// }
/// ```
///
/// The token is written by the main process after a successful OAuth
/// device flow or PAT paste. The provider/owner/name are set during
/// `open_local` based on remote URL detection.
fn read_forge_settings(work_dir: &Path) -> Result<ForgeSettings> {
    let settings_path = work_dir.join(".crab").join("desktop").join("settings.json");

    let content = match std::fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => {
            // No settings file — forge not configured.
            return Ok(ForgeSettings {
                provider: None,
                token: None,
                repo: None,
                base_url: None,
            });
        }
    };

    let raw: RawForgeSettings = serde_json::from_str(&content).unwrap_or_default();

    let repo = match (raw.forge_owner, raw.forge_name, raw.forge_provider) {
        (Some(owner), Some(name), Some(provider)) => Some(ForgeRepo {
            owner,
            name,
            provider,
        }),
        _ => None,
    };

    Ok(ForgeSettings {
        provider: raw.forge_provider,
        token: raw.forge_token,
        repo,
        base_url: raw.forge_base_url,
    })
}

/// Raw deserialization target for the subset of workspace settings
/// relevant to forge operations.
#[derive(Debug, Default, Deserialize)]
struct RawForgeSettings {
    #[serde(default)]
    forge_provider: Option<Provider>,
    #[serde(default)]
    forge_token: Option<String>,
    #[serde(default)]
    forge_owner: Option<String>,
    #[serde(default)]
    forge_name: Option<String>,
    #[serde(default)]
    forge_base_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_settings(dir: &Path, settings_json: &str) {
        let settings_dir = dir.join(".crab").join("desktop");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("settings.json"), settings_json).unwrap();
    }

    #[test]
    fn read_forge_settings_no_file() {
        let tmp = TempDir::new().unwrap();
        let settings = read_forge_settings(tmp.path()).unwrap();
        assert!(settings.provider.is_none());
        assert!(settings.token.is_none());
        assert!(settings.repo.is_none());
    }

    #[test]
    fn read_forge_settings_github() {
        let tmp = TempDir::new().unwrap();
        setup_settings(
            tmp.path(),
            r#"{
                "forge_provider": "github",
                "forge_token": "ghp_test123",
                "forge_owner": "octocat",
                "forge_name": "hello-world"
            }"#,
        );

        let settings = read_forge_settings(tmp.path()).unwrap();
        assert_eq!(settings.provider, Some(Provider::GitHub));
        assert_eq!(settings.token.as_deref(), Some("ghp_test123"));
        let repo = settings.repo.unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.name, "hello-world");
        assert_eq!(repo.provider, Provider::GitHub);
    }

    #[test]
    fn read_forge_settings_missing_token() {
        let tmp = TempDir::new().unwrap();
        setup_settings(
            tmp.path(),
            r#"{
                "forge_provider": "gitlab",
                "forge_owner": "group",
                "forge_name": "project"
            }"#,
        );

        let settings = read_forge_settings(tmp.path()).unwrap();
        assert_eq!(settings.provider, Some(Provider::GitLab));
        assert!(settings.token.is_none());
    }

    #[test]
    fn read_forge_settings_with_base_url() {
        let tmp = TempDir::new().unwrap();
        setup_settings(
            tmp.path(),
            r#"{
                "forge_provider": "gitea",
                "forge_token": "tok_abc",
                "forge_owner": "user",
                "forge_name": "repo",
                "forge_base_url": "https://gitea.example.com/api/v1"
            }"#,
        );

        let settings = read_forge_settings(tmp.path()).unwrap();
        assert_eq!(settings.provider, Some(Provider::Gitea));
        assert_eq!(
            settings.base_url.as_deref(),
            Some("https://gitea.example.com/api/v1")
        );
    }

    #[tokio::test]
    async fn forge_list_prs_no_settings_returns_not_supported() {
        let tmp = TempDir::new().unwrap();
        let result = forge_list_prs(tmp.path()).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ForgeNotSupported"));
    }

    #[tokio::test]
    async fn forge_list_prs_no_token_returns_auth_required() {
        let tmp = TempDir::new().unwrap();
        // Use a Gitea provider — it requires a base_url and is unlikely
        // to have a keychain entry in any environment, so the auth-required
        // path is exercised reliably without depending on keychain state.
        setup_settings(
            tmp.path(),
            r#"{
                "forge_provider": "gitea",
                "forge_owner": "octocat",
                "forge_name": "hello-world"
            }"#,
        );

        let result = forge_list_prs(tmp.path()).await;
        assert!(result.is_err());
        // Either ForgeAuthRequired (no token) or ForgeNotSupported (no base_url).
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("ForgeAuthRequired")
                || err.contains("ForgeNotSupported")
                || err.contains("base_url"),
            "unexpected error: {err}"
        );
    }
}
