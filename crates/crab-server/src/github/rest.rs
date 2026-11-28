//! GitHub REST v3 endpoints.
//!
//! Each endpoint runs one internal GraphQL document against the resolvers
//! behind `/api/graphql`, then reshapes the data into GitHub's REST JSON, so
//! both surfaces share a single mapping from native data.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query},
    http::{HeaderValue, Method, StatusCode, Uri, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    COMPATIBLE_VERSION, Context, Dispatch, SCHEMA, error,
    graphql::{self, Request},
    ids::{NodeId, decode_comment_rest_id, rest_id},
    native::{Failure, query},
    objects,
};
use crate::{auth::Principal, server::Server};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/v3", get(root))
        .route("/api/v3/", get(root))
        .route("/api/v3/meta", get(meta))
        .route("/api/v3/rate_limit", get(rate_limit))
        .route("/api/v3/user", get(current_user))
        .route("/api/v3/user/orgs", get(user_orgs))
        .route("/api/v3/user/repos", get(user_repos).post(create_user_repo))
        .route("/api/v3/users/{login}", get(user))
        .route("/api/v3/users/{login}/repos", get(user_login_repos))
        .route("/api/v3/orgs/{org}", get(user))
        .route(
            "/api/v3/orgs/{org}/repos",
            get(org_repos).post(create_org_repo),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}",
            get(repository).delete(delete_repository),
        )
        .route("/api/v3/repos/{owner}/{repo}/readme", get(readme))
        .route("/api/v3/repos/{owner}/{repo}/branches", get(branches))
        .route(
            "/api/v3/repos/{owner}/{repo}/git/refs/heads/{*branch}",
            delete(delete_branch),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/labels",
            get(labels).post(create_label),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/labels/{name}",
            get(label).patch(update_label).delete(delete_label),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues",
            get(issues).post(create_issue),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/{number}",
            get(issue).patch(update_issue),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/{number}/comments",
            get(comments).post(create_comment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/comments/{id}",
            get(comment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls",
            get(pulls).post(create_pull),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}",
            get(pull).patch(update_pull),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/files",
            get(pull_files),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/merge",
            put(merge_pull).get(merged),
        )
}

/// Request state for one REST handler.
pub(super) struct Rest {
    context: Context,
    uri: Uri,
    accept: String,
}

impl FromRequestParts<Arc<Server>> for Rest {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> Result<Self, Failure> {
        let principal = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| failure(StatusCode::UNAUTHORIZED, "Requires authentication"))?;
        let dispatch =
            parts.extensions.get::<Dispatch>().cloned().ok_or_else(|| {
                failure(StatusCode::INTERNAL_SERVER_ERROR, "Dispatch unavailable")
            })?;
        Ok(Self {
            context: Context::new(Arc::clone(server), principal, &dispatch, &parts.headers),
            uri: parts.uri.clone(),
            accept: parts
                .headers
                .get(header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

type Outcome<T> = Result<T, Failure>;

fn failure(status: StatusCode, message: &str) -> Failure {
    Failure {
        status,
        message: message.to_owned(),
    }
}

impl Rest {
    fn api(&self, path: &str) -> String {
        self.context
            .url(&format!("api/v3/{}", path.trim_start_matches('/')))
    }

    fn require_account(&self) -> Outcome<()> {
        let principal = self.context.principal();
        if principal.is_user() || principal.is_local() {
            Ok(())
        } else {
            Err(failure(StatusCode::UNAUTHORIZED, "Requires authentication"))
        }
    }

    /// Runs `document` through the GraphQL resolvers. The first resolver
    /// error decides the HTTP status, mirroring GitHub's REST errors.
    async fn graphql(&self, document: &str, variables: Value) -> Outcome<Value> {
        let schema = SCHEMA
            .as_ref()
            .map_err(|_| failure(StatusCode::INTERNAL_SERVER_ERROR, "GraphQL is unavailable"))?;
        let Value::Object(variables) = variables else {
            return Err(failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid variables",
            ));
        };
        let request = Request {
            query: document.to_owned(),
            operation_name: None,
            variables,
        };
        let mut response = graphql::execute(
            schema,
            &self.context,
            &objects::Query,
            &objects::Mutation,
            &request,
        )
        .await;
        if let Some(error) = response["errors"]
            .as_array()
            .and_then(|errors| errors.first())
        {
            let message = error["message"].as_str().unwrap_or("Internal error");
            let status = match error["type"].as_str() {
                Some("NOT_FOUND") => StatusCode::NOT_FOUND,
                Some("FORBIDDEN") => StatusCode::FORBIDDEN,
                Some("UNPROCESSABLE" | "EXCESSIVE_PAGE_SIZE") => StatusCode::UNPROCESSABLE_ENTITY,
                _ if error.get("extensions").is_some() => {
                    tracing::error!(%message, "REST document failed GraphQL validation");
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                _ => StatusCode::BAD_GATEWAY,
            };
            return Err(failure(status, message));
        }
        Ok(response["data"].take())
    }

    /// A Link header for the next page, carrying the native cursor.
    fn link(&self, next: Option<&str>) -> Option<HeaderValue> {
        let next = next.filter(|cursor| !cursor.is_empty())?;
        let mut pairs: Vec<(String, String)> = self
            .uri
            .query()
            .map(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .filter(|(name, _)| name != "after" && name != "page")
                    .map(|(name, value)| (name.into_owned(), value.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        pairs.push(("after".into(), next.into()));
        let mut encoded = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in &pairs {
            encoded.append_pair(name, value);
        }
        let url = format!("{}?{}", self.context.url(self.uri.path()), encoded.finish());
        HeaderValue::from_str(&format!("<{url}>; rel=\"next\"")).ok()
    }

    fn page(&self, value: Value, next: Option<&str>) -> Response {
        let mut response = Json(value).into_response();
        if let Some(link) = self.link(next) {
            response.headers_mut().insert(header::LINK, link);
        }
        response
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Paging {
    per_page: Option<u64>,
    page: Option<u64>,
    after: Option<String>,
}

impl Paging {
    fn first(&self) -> Outcome<u64> {
        let size = self.per_page.unwrap_or(30);
        if size > 100 {
            return Ok(100);
        }
        Ok(size.max(1))
    }
}

const ACTOR: &str = "fragment actor on Actor { __typename login ...on User { id name } ...on Organization { id name } ...on Bot { id } }";

const REPO: &str = "fragment repo on Repository { id name nameWithOwner description homepageUrl isTemplate url isPrivate isFork isArchived visibility createdAt updatedAt pushedAt owner { ...actor } defaultBranchRef { name } repositoryTopics(first: 100) { nodes { topic { name } } } }";

const REPO_DETAIL: &str = "fragment detail on Repository { viewerPermission mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed deleteBranchOnMerge parent { ...repo } }";

const LABEL: &str = "fragment label on Label { id name color description }";

const MILESTONE: &str = "fragment milestone on Milestone { id number title description state dueOn createdAt updatedAt closedAt }";

const ISSUE: &str = "fragment issue on Issue { __typename id number title body state stateReason createdAt updatedAt closedAt url author { ...actor } assignees(first: 10) { nodes { ...actor } } labels(first: 100) { nodes { ...label } } comments { totalCount } }";

const PULL: &str = "fragment pull on PullRequest { __typename id number title body state createdAt updatedAt closedAt mergedAt url isDraft merged author { ...actor } assignees(first: 10) { nodes { ...actor } } labels(first: 100) { nodes { ...label } } comments { totalCount } baseRefName headRefName headRefOid baseRef { target { oid } } headRepository { ...repo } headRepositoryOwner { ...actor } repository { ...repo } }";

const PULL_DETAIL: &str = "fragment pullDetail on PullRequest { baseRefOid mergeStateStatus mergeCommit { oid } mergedBy { ...actor } milestone { ...milestone } additions deletions changedFiles commits { totalCount } reviewRequests(first: 100) { nodes { requestedReviewer { __typename ...on User { ...actor } ...on Bot { ...actor } ...on Team { id name slug description } } } } autoMergeRequest { mergeMethod } }";

fn document(parts: &[&str]) -> String {
    parts.join("\n")
}

fn node_rest_id(value: &Value) -> Value {
    value
        .as_str()
        .and_then(NodeId::decode)
        .map_or(Value::Null, |node| Value::from(rest_id(&node)))
}

fn user_json(rest: &Rest, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let login = value["login"].as_str().unwrap_or_default();
    json!({
        "login": login,
        "id": node_rest_id(&value["id"]),
        "node_id": value["id"],
        "avatar_url": "",
        "gravatar_id": "",
        "url": rest.api(&format!("users/{login}")),
        "html_url": rest.context.url(login),
        "type": if value["__typename"] == "Organization" { "Organization" } else { "User" },
        "site_admin": false,
    })
}

fn repository_json(rest: &Rest, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let full = value["nameWithOwner"].as_str().unwrap_or_default();
    let mut repository = json!({
        "id": node_rest_id(&value["id"]),
        "node_id": value["id"],
        "name": value["name"],
        "full_name": full,
        "private": value["isPrivate"],
        "owner": user_json(rest, &value["owner"]),
        "html_url": value["url"],
        "description": value["description"],
        "fork": value["isFork"],
        "url": rest.api(&format!("repos/{full}")),
        "clone_url": rest.context.url(&format!("{full}.git")),
        "homepage": value["homepageUrl"],
        "is_template": value["isTemplate"],
        "size": 0,
        "stargazers_count": 0,
        "watchers_count": 0,
        "language": null,
        "has_issues": true,
        "has_projects": true,
        "has_wiki": false,
        "has_pages": false,
        "has_downloads": false,
        "has_discussions": false,
        "archived": value["isArchived"],
        "disabled": false,
        "visibility": value["visibility"].as_str().map(str::to_ascii_lowercase),
        "default_branch": value["defaultBranchRef"]["name"],
        "topics": value["repositoryTopics"]["nodes"].as_array().into_iter().flatten().map(|topic| topic["topic"]["name"].clone()).collect::<Vec<_>>(),
        "created_at": value["createdAt"],
        "updated_at": value["updatedAt"],
        "pushed_at": value["pushedAt"],
    });
    if let Some(permission) = value["viewerPermission"].as_str() {
        let rank = ["READ", "TRIAGE", "WRITE", "MAINTAIN", "ADMIN"]
            .iter()
            .position(|level| *level == permission)
            .unwrap_or(0);
        repository["permissions"] = json!({
            "admin": rank >= 4, "maintain": rank >= 3, "push": rank >= 2, "triage": rank >= 1, "pull": true,
        });
        for (from, to) in [
            ("mergeCommitAllowed", "allow_merge_commit"),
            ("squashMergeAllowed", "allow_squash_merge"),
            ("rebaseMergeAllowed", "allow_rebase_merge"),
            ("autoMergeAllowed", "allow_auto_merge"),
            ("deleteBranchOnMerge", "delete_branch_on_merge"),
        ] {
            repository[to] = value[from].clone();
        }
    }
    if value.get("parent").is_some_and(|parent| !parent.is_null()) {
        repository["parent"] = repository_json(rest, &value["parent"]);
        repository["source"] = repository["parent"].clone();
    }
    repository
}

fn label_json(rest: &Rest, full: &str, value: &Value) -> Value {
    let name = value["name"].as_str().unwrap_or_default();
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    json!({
        "id": node_rest_id(&value["id"]),
        "node_id": value["id"],
        "url": rest.api(&format!("repos/{full}/labels/{encoded}")),
        "name": name,
        "color": value["color"],
        "default": false,
        "description": value["description"],
    })
}

fn milestone_json(rest: &Rest, full: &str, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let number = value["number"].as_u64().unwrap_or(0);
    json!({
        "id": node_rest_id(&value["id"]),
        "node_id": value["id"],
        "number": number,
        "title": value["title"],
        "description": value["description"],
        "state": value["state"].as_str().map(str::to_ascii_lowercase),
        "due_on": value["dueOn"],
        "created_at": value["createdAt"],
        "updated_at": value["updatedAt"],
        "closed_at": value["closedAt"],
        "url": rest.api(&format!("repos/{full}/milestones/{number}")),
        "html_url": rest.context.url(&format!("{full}/milestone/{number}")),
    })
}

fn people(rest: &Rest, value: &Value) -> Vec<Value> {
    value["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|user| user_json(rest, user))
        .collect()
}

/// GitHub's issue JSON, which also represents pull requests in issue lists.
fn issue_json(rest: &Rest, full: &str, value: &Value) -> Value {
    let number = value["number"].as_u64().unwrap_or(0);
    let assignees = people(rest, &value["assignees"]);
    let pull = value["__typename"] == "PullRequest";
    let state = match value["state"].as_str() {
        Some("OPEN") => "open",
        _ => "closed",
    };
    let mut issue = json!({
        "id": node_rest_id(&value["id"]),
        "node_id": value["id"],
        "url": rest.api(&format!("repos/{full}/issues/{number}")),
        "repository_url": rest.api(&format!("repos/{full}")),
        "comments_url": rest.api(&format!("repos/{full}/issues/{number}/comments")),
        "html_url": value["url"],
        "number": number,
        "state": state,
        "state_reason": value["stateReason"].as_str().map(str::to_ascii_lowercase),
        "title": value["title"],
        "body": value["body"],
        "user": user_json(rest, &value["author"]),
        "labels": value["labels"]["nodes"].as_array().into_iter().flatten().map(|label| label_json(rest, full, label)).collect::<Vec<_>>(),
        "assignee": assignees.first().cloned().unwrap_or(Value::Null),
        "assignees": assignees,
        "locked": false,
        "comments": value["comments"]["totalCount"],
        "created_at": value["createdAt"],
        "updated_at": value["updatedAt"],
        "closed_at": value["closedAt"],
        "author_association": "NONE",
    });
    if value.get("milestone").is_some() {
        issue["milestone"] = milestone_json(rest, full, &value["milestone"]);
    }
    if pull {
        issue["pull_request"] = json!({
            "url": rest.api(&format!("repos/{full}/pulls/{number}")),
            "html_url": value["url"],
            "diff_url": format!("{}.diff", value["url"].as_str().unwrap_or_default()),
            "patch_url": format!("{}.patch", value["url"].as_str().unwrap_or_default()),
            "merged_at": value["mergedAt"],
        });
    }
    issue
}

fn pull_json(rest: &Rest, full: &str, value: &Value) -> Value {
    let number = value["number"].as_u64().unwrap_or(0);
    let mut pull = issue_json(rest, full, value);
    let side = |name: &str, sha: &Value, owner: &Value, repository: &Value| {
        let login = owner["login"].as_str().unwrap_or_default();
        json!({
            "label": format!("{login}:{name}"),
            "ref": name,
            "sha": sha,
            "user": user_json(rest, owner),
            "repo": repository_json(rest, repository),
        })
    };
    let fields = json!({
        "url": rest.api(&format!("repos/{full}/pulls/{number}")),
        "issue_url": rest.api(&format!("repos/{full}/issues/{number}")),
        "diff_url": format!("{}.diff", value["url"].as_str().unwrap_or_default()),
        "patch_url": format!("{}.patch", value["url"].as_str().unwrap_or_default()),
        "statuses_url": rest.api(&format!("repos/{full}/statuses/{}", value["headRefOid"].as_str().unwrap_or_default())),
        "draft": value["isDraft"],
        "merged": value["merged"],
        "merged_at": value["mergedAt"],
        "merge_commit_sha": value["mergeCommit"]["oid"],
        "merged_by": user_json(rest, &value["mergedBy"]),
        "mergeable": null,
        "mergeable_state": value["mergeStateStatus"].as_str().map(str::to_ascii_lowercase),
        "head": side(
            value["headRefName"].as_str().unwrap_or_default(),
            &value["headRefOid"],
            &value["headRepositoryOwner"],
            &value["headRepository"],
        ),
        "base": side(
            value["baseRefName"].as_str().unwrap_or_default(),
            match &value["baseRefOid"] {
                Value::Null => &value["baseRef"]["target"]["oid"],
                oid => oid,
            },
            &value["repository"]["owner"],
            &value["repository"],
        ),
    });
    if let (Some(pull), Value::Object(fields)) = (pull.as_object_mut(), fields) {
        pull.remove("pull_request");
        pull.extend(fields);
    }
    for (from, to) in [
        ("additions", "additions"),
        ("deletions", "deletions"),
        ("changedFiles", "changed_files"),
    ] {
        if let Some(value) = value.get(from) {
            pull[to] = value.clone();
        }
    }
    if let Some(commits) = value.get("commits") {
        pull["commits"] = commits["totalCount"].clone();
    }
    // Single pull requests select these; listings omit what they never read
    // rather than report none.
    if let Some(requests) = value.get("reviewRequests") {
        let reviewers = || {
            requests["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|node| &node["requestedReviewer"])
        };
        pull["requested_reviewers"] = reviewers()
            .filter(|reviewer| reviewer["__typename"] != "Team")
            .map(|reviewer| user_json(rest, reviewer))
            .collect();
        pull["requested_teams"] = reviewers()
            .filter(|reviewer| reviewer["__typename"] == "Team")
            .map(|team| {
                json!({
                    "id": node_rest_id(&team["id"]),
                    "node_id": team["id"],
                    "name": team["name"],
                    "slug": team["slug"],
                    "description": team["description"],
                })
            })
            .collect();
    }
    if let Some(auto_merge) = value.get("autoMergeRequest") {
        pull["auto_merge"] = auto_merge["mergeMethod"]
            .as_str()
            .map_or(Value::Null, |method| {
                json!({
                    "enabled_by": null,
                    "merge_method": method.to_ascii_lowercase(),
                    "commit_title": null,
                    "commit_message": null,
                })
            });
    }
    pull
}

fn comment_json(rest: &Rest, full: &str, number: u64, value: &Value) -> Value {
    let id = value["fullDatabaseId"]
        .as_str()
        .and_then(|id| id.parse::<u64>().ok());
    json!({
        "id": id,
        "node_id": value["id"],
        "url": rest.api(&format!("repos/{full}/issues/comments/{}", id.unwrap_or_default())),
        "html_url": value["url"],
        "issue_url": rest.api(&format!("repos/{full}/issues/{number}")),
        "body": value["body"],
        "user": user_json(rest, &value["author"]),
        "created_at": value["createdAt"],
        "updated_at": value["updatedAt"],
        "author_association": "NONE",
    })
}

async fn root(rest: Rest) -> Response {
    Json(json!({
        "current_user_url": rest.api("user"),
        "repository_url": rest.api("repos/{owner}/{repo}"),
        "user_url": rest.api("users/{user}"),
        "organization_url": rest.api("orgs/{org}"),
        "issues_url": rest.api("issues"),
        "rate_limit_url": rest.api("rate_limit"),
    }))
    .into_response()
}

async fn meta() -> Response {
    Json(json!({
        "installed_version": COMPATIBLE_VERSION,
        "verifiable_password_authentication": false,
    }))
    .into_response()
}

async fn rate_limit() -> Response {
    let reset = super::now_seconds() + 3600;
    let bucket = json!({"limit": 5000, "used": 0, "remaining": 5000, "reset": reset});
    Json(
        json!({"resources": {"core": bucket, "graphql": bucket, "search": bucket}, "rate": bucket}),
    )
    .into_response()
}

async fn current_user(rest: Rest) -> Outcome<Response> {
    rest.require_account()?;
    let data = rest
        .graphql(&document(&[ACTOR, "{ viewer { ...actor } }"]), json!({}))
        .await?;
    let mut user = user_json(&rest, &data["viewer"]);
    user["name"] = data["viewer"]["name"].clone();
    user["email"] = Value::Null;
    Ok(Json(user).into_response())
}

async fn user(rest: Rest, Path(login): Path<String>) -> Outcome<Response> {
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                "query($login: String!) { repositoryOwner(login: $login) { ...actor } }",
            ]),
            json!({"login": login}),
        )
        .await?;
    let owner = &data["repositoryOwner"];
    if owner.is_null() {
        return Err(failure(StatusCode::NOT_FOUND, "Not Found"));
    }
    let mut user = user_json(&rest, owner);
    user["name"] = owner["name"].clone();
    Ok(Json(user).into_response())
}

async fn user_orgs(rest: Rest) -> Outcome<Response> {
    rest.require_account()?;
    let data = rest
        .graphql(
            "{ viewer { organizations(first: 100) { nodes { id login description } } } }",
            json!({}),
        )
        .await?;
    let organizations: Vec<Value> = data["viewer"]["organizations"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|organization| {
            let login = organization["login"].as_str().unwrap_or_default();
            json!({
                "login": login,
                "id": node_rest_id(&organization["id"]),
                "node_id": organization["id"],
                "url": rest.api(&format!("orgs/{login}")),
                "repos_url": rest.api(&format!("orgs/{login}/repos")),
                "avatar_url": "",
                "description": organization["description"],
            })
        })
        .collect();
    Ok(Json(organizations).into_response())
}

async fn repository_page(
    rest: &Rest,
    owner: &str,
    selection: &str,
    paging: &Paging,
) -> Outcome<Response> {
    let document = document(&[
        ACTOR,
        REPO,
        &format!(
            "query($first: Int!, $after: String) {{ {owner} {{ repositories(first: $first, after: $after{selection}) {{ nodes {{ ...repo }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
        ),
    ]);
    let data = rest
        .graphql(
            &document,
            json!({"first": paging.first()?, "after": paging.after}),
        )
        .await?;
    let connection = data
        .as_object()
        .and_then(|data| data.values().next())
        .map(|owner| owner["repositories"].clone())
        .unwrap_or(Value::Null);
    if connection.is_null() {
        return Err(failure(StatusCode::NOT_FOUND, "Not Found"));
    }
    let repositories: Vec<Value> = connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|repository| repository_json(rest, repository))
        .collect();
    let next = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .filter(|more| *more)
        .and_then(|_| connection["pageInfo"]["endCursor"].as_str());
    Ok(rest.page(Value::Array(repositories), next))
}

async fn user_repos(rest: Rest, Query(paging): Query<Paging>) -> Outcome<Response> {
    rest.require_account()?;
    reject_page(&paging)?;
    repository_page(
        &rest,
        "viewer",
        ", ownerAffiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]",
        &paging,
    )
    .await
}

async fn user_login_repos(
    rest: Rest,
    Path(login): Path<String>,
    Query(paging): Query<Paging>,
) -> Outcome<Response> {
    reject_page(&paging)?;
    let owner = format!("repositoryOwner(login: {})", Value::String(login));
    repository_page(&rest, &owner, ", ownerAffiliations: [OWNER]", &paging).await
}

async fn org_repos(
    rest: Rest,
    Path(org): Path<String>,
    Query(paging): Query<Paging>,
) -> Outcome<Response> {
    reject_page(&paging)?;
    let owner = format!("organization(login: {})", Value::String(org));
    repository_page(&rest, &owner, "", &paging).await
}

/// Native lists resume from opaque cursors; Link headers carry them, so
/// numbered pages beyond the first have no stable meaning.
fn reject_page(paging: &Paging) -> Outcome<()> {
    if paging.page.is_some_and(|page| page > 1) && paging.after.is_none() {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Crab paginates with cursors; follow the Link header instead of page numbers",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct NewRepository {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    auto_init: bool,
    #[serde(default)]
    gitignore_template: Option<String>,
    #[serde(default)]
    license_template: Option<String>,
}

async fn create_repository(
    rest: &Rest,
    owner: Option<String>,
    input: NewRepository,
) -> Outcome<Response> {
    rest.require_account()?;
    if input.auto_init
        || input
            .gitignore_template
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || input
            .license_template
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The GitHub API creates empty Crab repositories; add initial files in Crab or push them",
        ));
    }
    let private = match input.visibility.as_deref() {
        Some("public") => false,
        Some("private") => true,
        Some(_) => {
            return Err(failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Visibility must be public or private",
            ));
        }
        None => input.private.unwrap_or(false),
    };
    let owner_id = match owner {
        Some(owner) => {
            let data = rest
                .graphql(
                    "query($login: String!) { organization(login: $login) { id } }",
                    json!({"login": owner}),
                )
                .await?;
            Some(data["organization"]["id"].clone())
        }
        None => None,
    };
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                REPO,
                REPO_DETAIL,
                "mutation($input: CreateRepositoryInput!) { createRepository(input: $input) { repository { ...repo ...detail } } }",
            ]),
            json!({"input": {
                "name": input.name,
                "description": input.description,
                "visibility": if private { "PRIVATE" } else { "PUBLIC" },
                "ownerId": owner_id,
            }}),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(repository_json(
            rest,
            &data["createRepository"]["repository"],
        )),
    )
        .into_response())
}

async fn create_user_repo(rest: Rest, Json(input): Json<NewRepository>) -> Outcome<Response> {
    create_repository(&rest, None, input).await
}

async fn create_org_repo(
    rest: Rest,
    Path(org): Path<String>,
    Json(input): Json<NewRepository>,
) -> Outcome<Response> {
    create_repository(&rest, Some(org), input).await
}

async fn repository(rest: Rest, Path((owner, repo)): Path<(String, String)>) -> Outcome<Response> {
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                REPO,
                REPO_DETAIL,
                "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { ...repo ...detail } }",
            ]),
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    Ok(Json(repository_json(&rest, &data["repository"])).into_response())
}

/// Deletes a repository. GitHub requires `delete_repo` scope on top of the
/// administrator role that Crab's deletion command checks.
async fn delete_repository(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
) -> Outcome<Response> {
    if rest
        .context
        .principal()
        .personal_token()
        .is_some_and(|token| !token.has(crate::auth::personal_tokens::Scope::DeleteRepo))
    {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Must have admin rights to Repository.",
        ));
    }
    let id = rest
        .graphql(
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id nameWithOwner } }",
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    let Some(NodeId::Repository(uuid)) = id["repository"]["id"].as_str().and_then(NodeId::decode)
    else {
        return Err(failure(StatusCode::NOT_FOUND, "Not Found"));
    };
    let path = format!("/api/repositories/{uuid}/deletion");
    let status = rest.context.native.get(&path).await?;
    if status["can_delete"] != true {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Must have admin rights to Repository.",
        ));
    }
    rest.context
        .native
        .send(
            Method::DELETE,
            &path,
            Some(uuid),
            Some(&json!({
                "request_id": Uuid::now_v7(),
                "version": status["version"],
                "confirmation": id["repository"]["nameWithOwner"],
            })),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Revision {
    #[serde(rename = "ref")]
    reference: Option<String>,
}

/// The root README, located case-insensitively like GitHub's.
async fn readme(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Query(revision): Query<Revision>,
) -> Outcome<Response> {
    let base = format!("/api/repos/{owner}/{repo}");
    let mut pairs = vec![("path", ""), ("limit", "200")];
    if let Some(reference) = &revision.reference {
        pairs.push(("rev", reference));
    }
    let tree = rest
        .context
        .native
        .get(&format!("{base}/tree?{}", query(&pairs)))
        .await?;
    let readme = tree["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["kind"] == "Blob")
        .filter_map(|entry| entry["path"].as_str())
        .filter(|path| {
            let lower = path.to_ascii_lowercase();
            lower == "readme" || lower.starts_with("readme.")
        })
        .min_by_key(|path| (!path.to_ascii_lowercase().ends_with(".md"), path.len()))
        .map(str::to_owned)
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
    let mut pairs = vec![("path", readme.as_str())];
    if let Some(reference) = &revision.reference {
        pairs.push(("rev", reference));
    }
    let bytes = rest
        .context
        .native
        .bytes(&format!("{base}/blob?{}", query(&pairs)))
        .await?;
    let html = rest.context.url(&format!(
        "{owner}/{repo}/blob/{}/{readme}",
        revision.reference.as_deref().unwrap_or("HEAD")
    ));
    Ok(Json(json!({
        "type": "file",
        "encoding": "base64",
        "size": bytes.len(),
        "name": readme,
        "path": readme,
        "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "url": rest.api(&format!("repos/{owner}/{repo}/contents/{readme}")),
        "html_url": html,
        "download_url": null,
    }))
    .into_response())
}

async fn branches(rest: Rest, Path((owner, repo)): Path<(String, String)>) -> Outcome<Response> {
    let data = rest
        .graphql(
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { refs(refPrefix: \"refs/heads/\", first: 100) { nodes { name target { oid } } } } }",
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    let branches: Vec<Value> = data["repository"]["refs"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|branch| {
            let sha = &branch["target"]["oid"];
            json!({
                "name": branch["name"],
                "commit": {"sha": sha, "url": rest.api(&format!("repos/{owner}/{repo}/commits/{}", sha.as_str().unwrap_or_default()))},
                "protected": false,
            })
        })
        .collect();
    Ok(Json(branches).into_response())
}

/// Deletes a branch at its current tip, the same compare-and-delete the
/// browser performs, so a concurrent push is never discarded.
async fn delete_branch(
    rest: Rest,
    Path((owner, repo, branch)): Path<(String, String, String)>,
) -> Outcome<Response> {
    let data = rest
        .graphql(
            "query($owner: String!, $name: String!, $ref: String!) { repository(owner: $owner, name: $name) { id ref(qualifiedName: $ref) { target { oid } } } }",
            json!({"owner": owner, "name": repo, "ref": format!("refs/heads/{branch}")}),
        )
        .await?;
    let Some(oid) = data["repository"]["ref"]["target"]["oid"].as_str() else {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Reference does not exist",
        ));
    };
    let uuid = match data["repository"]["id"].as_str().and_then(NodeId::decode) {
        Some(NodeId::Repository(uuid)) => uuid,
        _ => return Err(failure(StatusCode::NOT_FOUND, "Not Found")),
    };
    rest.context
        .native
        .send(
            Method::DELETE,
            &format!("/api/repos/{owner}/{repo}/branches"),
            Some(uuid),
            Some(&json!({"name": branch, "expected_oid": oid})),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn labels(rest: Rest, Path((owner, repo)): Path<(String, String)>) -> Outcome<Response> {
    let data = rest
        .graphql(
            &document(&[
                LABEL,
                "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { labels(first: 100) { nodes { ...label } } } }",
            ]),
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    let full = format!("{owner}/{repo}");
    let labels: Vec<Value> = data["repository"]["labels"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|label| label_json(&rest, &full, label))
        .collect();
    Ok(Json(labels).into_response())
}

/// The native label named `name`, with its number and version.
async fn native_label(rest: &Rest, owner: &str, repo: &str, name: &str) -> Outcome<(Uuid, Value)> {
    let catalog = rest
        .context
        .native
        .get(&format!("/api/repos/{owner}/{repo}/labels"))
        .await?;
    let label = catalog["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|label| {
            label["name"]
                .as_str()
                .is_some_and(|label| label.eq_ignore_ascii_case(name))
        })
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
    let entry = rest
        .context
        .server
        .repositories
        .get(&(owner.to_owned(), repo.to_owned()))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
    Ok((entry.id, label))
}

fn native_label_json(rest: &Rest, full: &str, repository: Uuid, label: &Value) -> Value {
    let node = NodeId::Label(repository, label["id"].as_u64().unwrap_or(0)).encode();
    label_json(
        rest,
        full,
        &json!({"id": node, "name": label["name"], "color": label["color"], "description": label["description"]}),
    )
}

async fn label(
    rest: Rest,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Outcome<Response> {
    let (repository, label) = native_label(&rest, &owner, &repo, &name).await?;
    Ok(Json(native_label_json(
        &rest,
        &format!("{owner}/{repo}"),
        repository,
        &label,
    ))
    .into_response())
}

#[derive(Deserialize)]
struct LabelInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn create_label(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Json(input): Json<LabelInput>,
) -> Outcome<Response> {
    let name = input
        .name
        .ok_or_else(|| failure(StatusCode::UNPROCESSABLE_ENTITY, "Validation Failed"))?;
    let entry = rest
        .context
        .server
        .repositories
        .get(&(owner.clone(), repo.clone()))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
    let color = input.color.unwrap_or_else(|| "ededed".into());
    let created = rest
        .context
        .native
        .send(
            Method::POST,
            &format!("/api/repos/{owner}/{repo}/labels"),
            Some(entry.id),
            Some(&json!({
                "request_id": Uuid::now_v7(),
                "name": name,
                "color": color.trim_start_matches('#'),
                "description": input.description,
            })),
        )
        .await?;
    let label = if created["id"].is_u64() {
        created
    } else {
        native_label(&rest, &owner, &repo, &name).await?.1
    };
    Ok((
        StatusCode::CREATED,
        Json(native_label_json(
            &rest,
            &format!("{owner}/{repo}"),
            entry.id,
            &label,
        )),
    )
        .into_response())
}

async fn update_label(
    rest: Rest,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Json(input): Json<LabelInput>,
) -> Outcome<Response> {
    let (repository, label) = native_label(&rest, &owner, &repo, &name).await?;
    let renamed = input
        .new_name
        .or(input.name)
        .unwrap_or_else(|| label["name"].as_str().unwrap_or_default().to_owned());
    let edited = rest
        .context
        .native
        .send(
            Method::PATCH,
            &format!("/api/repos/{owner}/{repo}/labels/{}", label["id"]),
            Some(repository),
            Some(&json!({
                "version": label["version"],
                "name": renamed,
                "color": input.color.as_deref().map_or_else(|| label["color"].clone(), |color| json!(color.trim_start_matches('#'))),
                "description": input.description.map_or_else(|| label["description"].clone(), Value::String),
            })),
        )
        .await?;
    let label = if edited["id"].is_u64() {
        edited
    } else {
        native_label(&rest, &owner, &repo, &renamed).await?.1
    };
    Ok(Json(native_label_json(
        &rest,
        &format!("{owner}/{repo}"),
        repository,
        &label,
    ))
    .into_response())
}

async fn delete_label(
    rest: Rest,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Outcome<Response> {
    let (repository, label) = native_label(&rest, &owner, &repo, &name).await?;
    rest.context
        .native
        .send(
            Method::DELETE,
            &format!("/api/repos/{owner}/{repo}/labels/{}", label["id"]),
            Some(repository),
            Some(&json!({"version": label["version"]})),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IssueFilters {
    state: Option<String>,
    labels: Option<String>,
    assignee: Option<String>,
    creator: Option<String>,
    milestone: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    since: Option<String>,
    per_page: Option<u64>,
    page: Option<u64>,
    after: Option<String>,
}

/// Translates GitHub's issue list parameters into the shared search
/// grammar. Like GitHub, the list includes pull requests.
async fn issues(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Query(filters): Query<IssueFilters>,
) -> Outcome<Response> {
    let paging = Paging {
        per_page: filters.per_page,
        page: filters.page,
        after: filters.after.clone(),
    };
    reject_page(&paging)?;
    let quote = |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
    let mut terms = vec![format!("repo:{owner}/{repo}")];
    match filters.state.as_deref().unwrap_or("open") {
        "all" => {}
        "closed" => terms.push("is:closed".into()),
        _ => terms.push("is:open".into()),
    }
    if let Some(labels) = filters
        .labels
        .as_deref()
        .filter(|labels| !labels.is_empty())
    {
        // GitHub requires every listed label.
        for label in labels.split(',') {
            terms.push(format!("label:{}", quote(label.trim())));
        }
    }
    for (value, qualifier) in [
        (&filters.assignee, "assignee"),
        (&filters.creator, "author"),
    ] {
        match value.as_deref() {
            None | Some("*") => {}
            Some("none") => terms.push(format!("no:{qualifier}")),
            Some(login) => terms.push(format!("{qualifier}:{}", quote(login))),
        }
    }
    match filters.milestone.as_deref() {
        None | Some("*") => {}
        Some("none") => terms.push("no:milestone".into()),
        Some(milestone) => terms.push(format!("milestone:{}", quote(milestone))),
    }
    if let Some(since) = &filters.since {
        terms.push(format!("updated:>={since}"));
    }
    terms.push(format!(
        "sort:{}-{}",
        filters.sort.as_deref().unwrap_or("created"),
        if filters.direction.as_deref() == Some("asc") {
            "asc"
        } else {
            "desc"
        }
    ));
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                LABEL,
                ISSUE,
                "fragment pullRow on PullRequest { __typename id number title body state createdAt updatedAt closedAt mergedAt url author { ...actor } assignees(first: 10) { nodes { ...actor } } labels(first: 100) { nodes { ...label } } comments { totalCount } }",
                "query($query: String!, $first: Int!, $after: String) { search(type: ISSUE, query: $query, first: $first, after: $after) { nodes { ...issue ...pullRow } pageInfo { hasNextPage endCursor } } }",
            ]),
            json!({"query": terms.join(" "), "first": paging.first()?, "after": paging.after}),
        )
        .await?;
    let full = format!("{owner}/{repo}");
    let connection = &data["search"];
    let items: Vec<Value> = connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|issue| issue_json(&rest, &full, issue))
        .collect();
    let next = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .filter(|more| *more)
        .and_then(|_| connection["pageInfo"]["endCursor"].as_str());
    Ok(rest.page(Value::Array(items), next))
}

async fn issue(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Outcome<Response> {
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                LABEL,
                MILESTONE,
                ISSUE,
                "fragment pullRow on PullRequest { __typename id number title body state createdAt updatedAt closedAt mergedAt url author { ...actor } assignees(first: 10) { nodes { ...actor } } labels(first: 100) { nodes { ...label } } comments { totalCount } milestone { ...milestone } }",
                "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { ...issue ...on Issue { milestone { ...milestone } } ...pullRow } } }",
            ]),
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    Ok(Json(issue_json(
        &rest,
        &format!("{owner}/{repo}"),
        &data["repository"]["issueOrPullRequest"],
    ))
    .into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IssueInput {
    title: Option<String>,
    body: Option<String>,
    state: Option<String>,
    state_reason: Option<String>,
    labels: Option<Vec<Value>>,
    assignees: Option<Vec<String>>,
    assignee: Option<String>,
    milestone: Option<Value>,
}

/// Resolves REST names into the node IDs GraphQL mutations take: label
/// names, assignee logins, and a milestone number.
async fn metadata(
    rest: &Rest,
    owner: &str,
    repo: &str,
    input: &IssueInput,
) -> Outcome<Map<String, Value>> {
    let mut resolved = Map::new();
    let data = rest
        .graphql(
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id labels(first: 100) { nodes { id name } } assignableUsers(first: 100) { nodes { id login } } } }",
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    let repository = &data["repository"];
    resolved.insert("repositoryId".into(), repository["id"].clone());
    if let Some(labels) = &input.labels {
        let catalog = repository["labels"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let ids = labels
            .iter()
            .map(|label| {
                let name = label
                    .as_str()
                    .or_else(|| label["name"].as_str())
                    .unwrap_or_default();
                catalog
                    .iter()
                    .find(|candidate| {
                        candidate["name"]
                            .as_str()
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                    })
                    .map(|candidate| candidate["id"].clone())
                    .ok_or_else(|| {
                        failure(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            &format!("Label '{name}' does not exist"),
                        )
                    })
            })
            .collect::<Outcome<Vec<_>>>()?;
        resolved.insert("labelIds".into(), Value::Array(ids));
    }
    let logins: Option<Vec<String>> = input
        .assignees
        .clone()
        .or_else(|| input.assignee.clone().map(|login| vec![login]));
    if let Some(logins) = logins {
        let roster = repository["assignableUsers"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let ids = logins
            .iter()
            .map(|login| {
                roster
                    .iter()
                    .find(|candidate| {
                        candidate["login"]
                            .as_str()
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(login))
                    })
                    .map(|candidate| candidate["id"].clone())
                    .ok_or_else(|| {
                        failure(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            &format!("'{login}' cannot be assigned"),
                        )
                    })
            })
            .collect::<Outcome<Vec<_>>>()?;
        resolved.insert("assigneeIds".into(), Value::Array(ids));
    }
    match &input.milestone {
        None => {}
        Some(Value::Null) => {
            resolved.insert("milestoneId".into(), Value::Null);
        }
        Some(number) => {
            let number = number
                .as_u64()
                .or_else(|| number.as_str().and_then(|number| number.parse().ok()))
                .ok_or_else(|| {
                    failure(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Milestone must be a number",
                    )
                })?;
            let data = rest
                .graphql(
                    "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { milestone(number: $number) { id } } }",
                    json!({"owner": owner, "name": repo, "number": number}),
                )
                .await?;
            let id = data["repository"]["milestone"]["id"].clone();
            if id.is_null() {
                return Err(failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Milestone does not exist",
                ));
            }
            resolved.insert("milestoneId".into(), id);
        }
    }
    Ok(resolved)
}

async fn create_issue(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Json(input): Json<IssueInput>,
) -> Outcome<Response> {
    let title = input
        .title
        .clone()
        .ok_or_else(|| failure(StatusCode::UNPROCESSABLE_ENTITY, "Validation Failed"))?;
    let mut mutation = metadata(&rest, &owner, &repo, &input).await?;
    mutation.insert("title".into(), json!(title));
    mutation.insert("body".into(), json!(input.body.unwrap_or_default()));
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                LABEL,
                ISSUE,
                "mutation($input: CreateIssueInput!) { createIssue(input: $input) { issue { ...issue } } }",
            ]),
            json!({"input": mutation}),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(issue_json(
            &rest,
            &format!("{owner}/{repo}"),
            &data["createIssue"]["issue"],
        )),
    )
        .into_response())
}

async fn update_issue(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(input): Json<IssueInput>,
) -> Outcome<Response> {
    apply_issue_update(&rest, &owner, &repo, number, input).await?;
    issue(rest, Path((owner, repo, number))).await
}

/// Applies a REST issue or pull request edit; GitHub shares one shape.
async fn apply_issue_update(
    rest: &Rest,
    owner: &str,
    repo: &str,
    number: u64,
    input: IssueInput,
) -> Outcome<()> {
    let id = rest
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { __typename ...on Issue { id state } ...on PullRequest { id state } } } }",
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    let subject = &id["repository"]["issueOrPullRequest"];
    let pull = subject["__typename"] == "PullRequest";
    let node = subject["id"].clone();
    let mut mutation = metadata(rest, owner, repo, &input).await?;
    mutation.remove("repositoryId");
    for name in ["title", "body"] {
        let value = match name {
            "title" => &input.title,
            _ => &input.body,
        };
        if let Some(value) = value {
            mutation.insert(name.into(), json!(value));
        }
    }
    let closing = input.state.as_deref() == Some("closed") && subject["state"] == "OPEN";
    let reopening = input.state.as_deref() == Some("open") && subject["state"] != "OPEN";
    let edit = if pull {
        "updatePullRequest"
    } else {
        "updateIssue"
    };
    let key = if pull { "pullRequestId" } else { "id" };
    if !mutation.is_empty() {
        mutation.insert(key.into(), node.clone());
        let input_type = if pull {
            "UpdatePullRequestInput"
        } else {
            "UpdateIssueInput"
        };
        rest.graphql(
            &format!(
                "mutation($input: {input_type}!) {{ {edit}(input: $input) {{ clientMutationId }} }}"
            ),
            json!({"input": mutation}),
        )
        .await?;
    }
    if closing || reopening {
        let (name, input_type, field, extra) = match (pull, closing) {
            (true, true) => (
                "closePullRequest",
                "ClosePullRequestInput",
                "pullRequestId",
                Value::Null,
            ),
            (true, false) => (
                "reopenPullRequest",
                "ReopenPullRequestInput",
                "pullRequestId",
                Value::Null,
            ),
            (false, true) => (
                "closeIssue",
                "CloseIssueInput",
                "issueId",
                input
                    .state_reason
                    .as_deref()
                    .map_or(Value::Null, |reason| json!(reason.to_ascii_uppercase())),
            ),
            (false, false) => ("reopenIssue", "ReopenIssueInput", "issueId", Value::Null),
        };
        let mut state = json!({ field: node });
        if !extra.is_null() {
            state["stateReason"] = extra;
        }
        rest.graphql(
            &format!(
                "mutation($input: {input_type}!) {{ {name}(input: $input) {{ clientMutationId }} }}"
            ),
            json!({"input": state}),
        )
        .await?;
    }
    Ok(())
}

async fn comments(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Query(paging): Query<Paging>,
) -> Outcome<Response> {
    reject_page(&paging)?;
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                "fragment comment on IssueComment { id fullDatabaseId url body createdAt updatedAt author { ...actor } }",
                "fragment comments on IssueCommentConnection { nodes { ...comment } pageInfo { hasNextPage endCursor } }",
                "query($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { ...on Issue { comments(first: $first, after: $after) { ...comments } } ...on PullRequest { comments(first: $first, after: $after) { ...comments } } } } }",
            ]),
            json!({"owner": owner, "name": repo, "number": number, "first": paging.first()?, "after": paging.after}),
        )
        .await?;
    let full = format!("{owner}/{repo}");
    let connection = &data["repository"]["issueOrPullRequest"]["comments"];
    let items: Vec<Value> = connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|comment| comment_json(&rest, &full, number, comment))
        .collect();
    let next = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .filter(|more| *more)
        .and_then(|_| connection["pageInfo"]["endCursor"].as_str());
    Ok(rest.page(Value::Array(items), next))
}

#[derive(Deserialize)]
struct CommentInput {
    body: String,
}

async fn create_comment(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(input): Json<CommentInput>,
) -> Outcome<Response> {
    let id = rest
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issueOrPullRequest(number: $number) { ...on Issue { id } ...on PullRequest { id } } } }",
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                "mutation($input: AddCommentInput!) { addComment(input: $input) { commentEdge { node { id fullDatabaseId url body createdAt updatedAt author { ...actor } } } } }",
            ]),
            json!({"input": {"subjectId": id["repository"]["issueOrPullRequest"]["id"], "body": input.body}}),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(comment_json(
            &rest,
            &format!("{owner}/{repo}"),
            number,
            &data["addComment"]["commentEdge"]["node"],
        )),
    )
        .into_response())
}

async fn comment(
    rest: Rest,
    Path((owner, repo, id)): Path<(String, String, u64)>,
) -> Outcome<Response> {
    let Some((discussion, number, comment)) = decode_comment_rest_id(id) else {
        return Err(failure(StatusCode::NOT_FOUND, "Not Found"));
    };
    let entry = rest
        .context
        .server
        .repositories
        .get(&(owner.clone(), repo.clone()))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
    let node = NodeId::Comment {
        repository: entry.id,
        discussion,
        number,
        comment,
    }
    .encode();
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                "query($id: ID!) { node(id: $id) { ...on IssueComment { id fullDatabaseId url body createdAt updatedAt author { ...actor } } } }",
            ]),
            json!({"id": node}),
        )
        .await?;
    Ok(Json(comment_json(
        &rest,
        &format!("{owner}/{repo}"),
        number,
        &data["node"],
    ))
    .into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PullFilters {
    state: Option<String>,
    head: Option<String>,
    base: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    per_page: Option<u64>,
    page: Option<u64>,
    after: Option<String>,
}

async fn pulls(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Query(filters): Query<PullFilters>,
) -> Outcome<Response> {
    let paging = Paging {
        per_page: filters.per_page,
        page: filters.page,
        after: filters.after.clone(),
    };
    reject_page(&paging)?;
    let states = match filters.state.as_deref().unwrap_or("open") {
        "all" => json!(["OPEN", "CLOSED", "MERGED"]),
        "closed" => json!(["CLOSED", "MERGED"]),
        _ => json!(["OPEN"]),
    };
    // GitHub's `head` filter is `owner:branch`; the owner selects a fork.
    let (head_owner, head) = match filters.head.as_deref().map(|head| head.split_once(':')) {
        Some(Some((owner, branch))) => (Some(owner), Some(branch)),
        Some(None) => (None, filters.head.as_deref()),
        None => (None, None),
    };
    let order = json!({
        "field": match filters.sort.as_deref() { Some("updated") => "UPDATED_AT", _ => "CREATED_AT" },
        "direction": if filters.direction.as_deref() == Some("asc") { "ASC" } else { "DESC" },
    });
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                LABEL,
                REPO,
                PULL,
                "query($owner: String!, $name: String!, $first: Int!, $after: String, $states: [PullRequestState!], $head: String, $base: String, $order: IssueOrder) { repository(owner: $owner, name: $name) { pullRequests(first: $first, after: $after, states: $states, headRefName: $head, baseRefName: $base, orderBy: $order) { nodes { ...pull } pageInfo { hasNextPage endCursor } } } }",
            ]),
            json!({
                "owner": owner, "name": repo, "first": paging.first()?, "after": paging.after,
                "states": states, "head": head, "base": filters.base, "order": order,
            }),
        )
        .await?;
    let full = format!("{owner}/{repo}");
    let connection = &data["repository"]["pullRequests"];
    let items: Vec<Value> = connection["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pull| head_owner.is_none_or(|owner| pull["headRepositoryOwner"]["login"] == owner))
        .map(|pull| pull_json(&rest, &full, pull))
        .collect();
    let next = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .filter(|more| *more)
        .and_then(|_| connection["pageInfo"]["endCursor"].as_str());
    Ok(rest.page(Value::Array(items), next))
}

async fn pull_detail(rest: &Rest, owner: &str, repo: &str, number: u64) -> Outcome<Value> {
    let data = rest
        .graphql(
            &document(&[
                ACTOR,
                LABEL,
                REPO,
                MILESTONE,
                PULL,
                PULL_DETAIL,
                "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { ...pull ...pullDetail } } }",
            ]),
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    let pull = data["repository"]["pullRequest"].clone();
    if pull.is_null() {
        return Err(failure(StatusCode::NOT_FOUND, "Not Found"));
    }
    Ok(pull)
}

/// A pull request, or its unified diff for the `diff` and `patch` media types.
async fn pull(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Outcome<Response> {
    if rest.accept.contains("diff") || rest.accept.contains("patch") {
        let repository = objects::Repository::find(&rest.context, &owner, &repo)
            .await
            .map_err(|error| field_error(&error))?;
        let pull = objects::PullRequest::find(&rest.context, &repository, number)
            .await
            .map_err(|error| field_error(&error))?
            .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Not Found"))?;
        let diff = pull
            .unified_diff(&rest.context)
            .await
            .map_err(|error| field_error(&error))?;
        let media = if rest.accept.contains("patch") {
            "application/vnd.github.v3.patch"
        } else {
            "application/vnd.github.v3.diff"
        };
        return Ok(([(header::CONTENT_TYPE, media)], diff).into_response());
    }
    let pull = pull_detail(&rest, &owner, &repo, number).await?;
    Ok(Json(pull_json(&rest, &format!("{owner}/{repo}"), &pull)).into_response())
}

/// GitHub's REST status for a resolver error.
fn field_error(error: &graphql::FieldError) -> Failure {
    failure(
        match error.kind() {
            Some("NOT_FOUND") => StatusCode::NOT_FOUND,
            Some("FORBIDDEN") => StatusCode::FORBIDDEN,
            Some("UNPROCESSABLE" | "EXCESSIVE_PAGE_SIZE") => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_GATEWAY,
        },
        error.message(),
    )
}

#[derive(Deserialize)]
struct NewPull {
    title: String,
    head: String,
    base: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
}

async fn create_pull(
    rest: Rest,
    Path((owner, repo)): Path<(String, String)>,
    Json(input): Json<NewPull>,
) -> Outcome<Response> {
    let id = rest
        .graphql(
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }",
            json!({"owner": owner, "name": repo}),
        )
        .await?;
    let data = rest
        .graphql(
            "mutation($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { number } } }",
            json!({"input": {
                "repositoryId": id["repository"]["id"],
                "title": input.title,
                "body": input.body,
                "headRefName": input.head,
                "baseRefName": input.base,
                "draft": input.draft,
            }}),
        )
        .await?;
    let number = data["createPullRequest"]["pullRequest"]["number"]
        .as_u64()
        .ok_or_else(|| {
            failure(
                StatusCode::BAD_GATEWAY,
                "The created pull request is not readable",
            )
        })?;
    let pull = pull_detail(&rest, &owner, &repo, number).await?;
    Ok((
        StatusCode::CREATED,
        Json(pull_json(&rest, &format!("{owner}/{repo}"), &pull)),
    )
        .into_response())
}

async fn update_pull(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(input): Json<IssueInput>,
) -> Outcome<Response> {
    apply_issue_update(&rest, &owner, &repo, number, input).await?;
    let pull = pull_detail(&rest, &owner, &repo, number).await?;
    Ok(Json(pull_json(&rest, &format!("{owner}/{repo}"), &pull)).into_response())
}

async fn pull_files(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Outcome<Response> {
    let data = rest
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { files(first: 100) { nodes { path additions deletions changeType } } } } }",
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    let files: Vec<Value> = data["repository"]["pullRequest"]["files"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| {
            let additions = file["additions"].as_u64().unwrap_or(0);
            let deletions = file["deletions"].as_u64().unwrap_or(0);
            json!({
                "filename": file["path"],
                "status": match file["changeType"].as_str() {
                    Some("ADDED") => "added",
                    Some("DELETED") => "removed",
                    Some("RENAMED") => "renamed",
                    Some("COPIED") => "copied",
                    _ => "modified",
                },
                "additions": additions,
                "deletions": deletions,
                "changes": additions + deletions,
            })
        })
        .collect();
    Ok(Json(files).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MergeInput {
    commit_title: Option<String>,
    commit_message: Option<String>,
    sha: Option<String>,
    merge_method: Option<String>,
}

async fn merge_pull(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    body: Option<Json<MergeInput>>,
) -> Outcome<Response> {
    let input = body.map(|Json(input)| input).unwrap_or_default();
    let id = rest
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } } }",
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    let method = match input.merge_method.as_deref() {
        Some("squash") => "SQUASH",
        Some("rebase") => "REBASE",
        _ => "MERGE",
    };
    let data = rest
        .graphql(
            "mutation($input: MergePullRequestInput!) { mergePullRequest(input: $input) { pullRequest { merged mergeCommit { oid } } } }",
            json!({"input": {
                "pullRequestId": id["repository"]["pullRequest"]["id"],
                "mergeMethod": method,
                "commitHeadline": input.commit_title,
                "commitBody": input.commit_message,
                "expectedHeadOid": input.sha,
            }}),
        )
        .await?;
    let merged = &data["mergePullRequest"]["pullRequest"];
    if merged["merged"] != true {
        // A queued or pending merge has not published a commit yet.
        return Err(failure(
            StatusCode::METHOD_NOT_ALLOWED,
            "Pull Request is not mergeable",
        ));
    }
    Ok(Json(json!({
        "sha": merged["mergeCommit"]["oid"],
        "merged": true,
        "message": "Pull Request successfully merged",
    }))
    .into_response())
}

/// GitHub answers 204 for merged pull requests and 404 otherwise.
async fn merged(
    rest: Rest,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Outcome<Response> {
    let data = rest
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { merged } } }",
            json!({"owner": owner, "name": repo, "number": number}),
        )
        .await?;
    Ok(if data["repository"]["pullRequest"]["merged"] == true {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(StatusCode::NOT_FOUND, "Not Found")
    })
}
