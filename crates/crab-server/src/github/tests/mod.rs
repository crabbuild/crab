//! GitHub API compatibility through the real HTTP stack. GraphQL documents
//! are GitHub CLI's own, so a passing test means gh sends what Crab answers.

use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

use crate::test_support::{git, http::Harness};

#[cfg(unix)]
mod gh;

async fn token(h: &Harness, cookie: &str, scopes: &[&str]) -> String {
    let response = h
        .write(
            Method::POST,
            "/api/personal-access-tokens",
            cookie,
            json!({"name": "GitHub CLI", "scopes": scopes, "expires_in_days": 7}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.unwrap();
    assert!(created["expires_at"].as_u64().unwrap() > created["created_at"].as_u64().unwrap());
    created["token"].as_str().unwrap().to_owned()
}

async fn graphql(h: &Harness, token: &str, query: &str, variables: Value) -> Value {
    h.http
        .post(format!("{}/api/graphql", h.origin))
        .header(header::AUTHORIZATION, format!("token {token}"))
        .json(&json!({"query": query, "variables": variables}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn data(h: &Harness, token: &str, query: &str, variables: Value) -> Value {
    let response = graphql(h, token, query, variables).await;
    assert!(response.get("errors").is_none(), "{response:#}");
    response["data"].clone()
}

async fn rest(
    h: &Harness,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = h
        .http
        .request(method, format!("{}/api/v3{path}", h.origin))
        .header(header::AUTHORIZATION, format!("token {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    request.send().await.unwrap()
}

/// A harness under a nonempty storage root, which production requires and
/// browser-created repositories need for their coordination keys.
async fn rooted() -> Harness {
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "github").await;
    h.initialize_directory().await;
    h
}

async fn alice(h: &Harness) -> (String, String) {
    crate::test_support::server::init_test_tracing();
    let cookie = h.login().await;
    h.choose_username(&cookie, "alice").await;
    let token = token(h, &cookie, &["repo", "read:org", "gist"]).await;
    (cookie, token)
}

#[tokio::test]
async fn personal_tokens_authenticate_github_clients_and_nothing_else() {
    let h = Harness::new(false).await;
    let (cookie, token) = alice(&h).await;

    // gh's login check: the REST root must answer and report the scopes.
    let root = rest(&h, &token, Method::GET, "/", None).await;
    assert_eq!(root.status(), StatusCode::OK);
    assert_eq!(root.headers()["x-oauth-scopes"], "repo, read:org, gist");
    assert_eq!(root.headers()["x-github-enterprise-version"], "3.17.0");
    let meta: Value = rest(&h, &token, Method::GET, "/meta", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(meta["installed_version"], "3.17.0");
    let viewer = data(&h, &token, "query UserCurrent{viewer{login}}", json!({})).await;
    assert_eq!(viewer["viewer"]["login"], "alice");
    let user: Value = rest(&h, &token, Method::GET, "/user", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (user["login"].as_str(), user["type"].as_str()),
        (Some("alice"), Some("User"))
    );

    // Invalid tokens are rejected outright, and cookies never authenticate
    // the GitHub API, which therefore needs no CSRF proof.
    let forged = rest(&h, &format!("{token}0"), Method::GET, "/user", None).await;
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        forged.json::<Value>().await.unwrap()["message"],
        "Bad credentials"
    );
    let cookie_only = h
        .http
        .get(format!("{}/api/v3/user", h.origin))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(cookie_only.status(), StatusCode::UNAUTHORIZED);
    let unauthenticated_graphql = h
        .http
        .post(format!("{}/api/graphql", h.origin))
        .header(header::COOKIE, &cookie)
        .json(&json!({"query": "{ viewer { login } }"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated_graphql.status(), StatusCode::UNAUTHORIZED);

    // A token reaches neither the browser API nor credential management.
    for path in ["/api/personal-access-tokens", "/api/repos"] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(header::AUTHORIZATION, format!("token {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    let listed = h.json("/api/personal-access-tokens", &cookie).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["scopes"], json!(["repo", "read:org", "gist"]));
    assert!(listed[0].get("token").is_none());
    let id = listed[0]["id"].as_str().unwrap().to_owned();
    let revoked = h
        .write(
            Method::DELETE,
            &format!("/api/personal-access-tokens/{id}"),
            &cookie,
            json!({}),
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    let after = rest(&h, &token, Method::GET, "/user", None).await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    h.close().await;
}

#[tokio::test]
async fn token_creation_rejects_unknown_scopes_and_excess_lifetimes() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    for body in [
        json!({"name": "bad scope", "scopes": ["admin:enterprise"], "expires_in_days": 7}),
        json!({"name": "too long", "scopes": ["repo"], "expires_in_days": 367}),
        json!({"name": "", "scopes": ["repo"]}),
    ] {
        let response = h
            .write(
                Method::POST,
                "/api/personal-access-tokens",
                &cookie,
                body.clone(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    h.close().await;
}

#[tokio::test]
async fn feature_detection_matches_what_crab_implements() {
    let h = Harness::new(false).await;
    let (_, token) = alice(&h).await;
    let fields = |data: &Value, name: &str| -> Vec<String> {
        data[name]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let repository = data(
        &h,
        &token,
        "query Repository_fields{Repository: __type(name: \"Repository\"){fields(includeDeprecated: true){name}}}",
        json!({}),
    )
    .await;
    let repository = fields(&repository, "Repository");
    assert!(repository.contains(&"visibility".to_owned()));
    // Without the field, gh reads pull request templates from the checkout.
    assert!(!repository.contains(&"pullRequestTemplates".to_owned()));
    let pull = data(
        &h,
        &token,
        "query PullRequest_fields{PullRequest: __type(name: \"PullRequest\"){fields(includeDeprecated: true){name}} StatusCheckRollupContextConnection: __type(name: \"StatusCheckRollupContextConnection\"){fields(includeDeprecated: true){name}}}",
        json!({}),
    )
    .await;
    assert!(!fields(&pull, "PullRequest").contains(&"isInMergeQueue".to_owned()));
    assert!(
        fields(&pull, "StatusCheckRollupContextConnection").contains(&"checkRunCount".to_owned())
    );
    // gh ignores exactly this error when an instance has no Projects.
    let projects = graphql(
        &h,
        &token,
        "query IssueProjectItems($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { projectItems(first: 100) { totalCount } } } }",
        json!({"owner": "team", "name": "private", "number": 1}),
    )
    .await;
    assert_eq!(
        projects["errors"][0]["message"],
        "Field 'projectItems' doesn't exist on type 'Issue'"
    );
    h.close().await;
}

const REPOSITORY_INFO: &str = r#"
fragment repo on Repository {
    id
    name
    owner { login }
    hasIssuesEnabled
    description
    hasWikiEnabled
    viewerPermission
    defaultBranchRef {
        name
    }
}

query RepositoryInfo($owner: String!, $name: String!) {
    repository(owner: $owner, name: $name) {
        ...repo
        parent {
            ...repo
        }
        mergeCommitAllowed
        rebaseMergeAllowed
        squashMergeAllowed
    }
}"#;

const ISSUE_LIST: &str = r#"fragment issue on Issue {number,title,url,state,updatedAt,labels(first:100){nodes{id,name,description,color},totalCount}}
query IssueList($owner: String!, $repo: String!, $limit: Int, $endCursor: String, $states: [IssueState!] = OPEN, $assignee: String, $author: String, $mention: String) {
    repository(owner: $owner, name: $repo) {
        hasIssuesEnabled
        issues(first: $limit, after: $endCursor, orderBy: {field: CREATED_AT, direction: DESC}, states: $states, filterBy: {assignee: $assignee, createdBy: $author, mentioned: $mention}) {
            totalCount
            nodes {
                ...issue
            }
            pageInfo {
                hasNextPage
                endCursor
            }
        }
    }
}"#;

const ISSUE_BY_NUMBER: &str = r#"
query IssueByNumber($owner: String!, $repo: String!, $number: Int!) {
    repository(owner: $owner, name: $repo) {
        hasIssuesEnabled
        issue: issueOrPullRequest(number: $number) {
            __typename
            ...on Issue{number,url,state,createdAt,title,body,author{login,...on User{id,name}},milestone{number,title,description,dueOn},assignees(first:100){nodes{id,login,name,databaseId},totalCount},labels(first:100){nodes{id,name,description,color},totalCount},reactionGroups{content,users{totalCount}},comments(last: 1){nodes{author{login,...on User{id,name}},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}}},totalCount},stateReason,id}
            ...on PullRequest{number,url,state,createdAt,title,body,author{login,...on User{id,name}},milestone{number,title,description,dueOn},assignees(first:100){nodes{id,login,name,databaseId},totalCount},labels(first:100){nodes{id,name,description,color},totalCount},reactionGroups{content,users{totalCount}},id}
        }
    }
}"#;

#[tokio::test]
async fn gh_issue_documents_drive_the_native_issue_lifecycle() {
    let h = Harness::new(false).await;
    let (_, token) = alice(&h).await;
    let repository = data(
        &h,
        &token,
        REPOSITORY_INFO,
        json!({"owner": "team", "name": "private"}),
    )
    .await;
    let repository = &repository["repository"];
    assert_eq!(
        (
            repository["owner"]["login"].as_str(),
            repository["hasIssuesEnabled"].as_bool(),
            repository["viewerPermission"].as_str(),
            repository["parent"].is_null(),
        ),
        (Some("team"), Some(true), Some("ADMIN"), true)
    );
    let created = data(
        &h,
        &token,
        "mutation IssueCreate($input: CreateIssueInput!) { createIssue(input: $input) { issue { id url } } }",
        json!({"input": {"repositoryId": repository["id"], "title": "Compiler crash", "body": "Steps to reproduce"}}),
    )
    .await;
    let issue_id = created["createIssue"]["issue"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        created["createIssue"]["issue"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/team/private/issues/1")
    );

    let list = data(
        &h,
        &token,
        ISSUE_LIST,
        json!({"owner": "team", "repo": "private", "limit": 30}),
    )
    .await;
    let issues = &list["repository"]["issues"];
    assert_eq!(
        (
            issues["totalCount"].as_u64(),
            issues["nodes"][0]["title"].as_str()
        ),
        (Some(1), Some("Compiler crash"))
    );
    assert_eq!(issues["pageInfo"]["hasNextPage"], false);

    let commented = data(
        &h,
        &token,
        "mutation CommentCreate($input:AddCommentInput!){addComment(input: $input){commentEdge{node{url}}}}",
        json!({"input": {"body": "Reproduced on main", "subjectId": issue_id}}),
    )
    .await;
    assert!(
        commented["addComment"]["commentEdge"]["node"]["url"]
            .as_str()
            .unwrap()
            .contains("#issuecomment-")
    );

    let viewed = data(
        &h,
        &token,
        ISSUE_BY_NUMBER,
        json!({"owner": "team", "repo": "private", "number": 1}),
    )
    .await;
    let issue = &viewed["repository"]["issue"];
    assert_eq!(
        (
            issue["__typename"].as_str(),
            issue["body"].as_str(),
            issue["author"]["login"].as_str(),
            issue["comments"]["totalCount"].as_u64(),
            issue["comments"]["nodes"][0]["body"].as_str(),
            issue["stateReason"].is_null(),
        ),
        (
            Some("Issue"),
            Some("Steps to reproduce"),
            Some("alice"),
            Some(1),
            Some("Reproduced on main"),
            true
        )
    );

    let closed = data(
        &h,
        &token,
        "mutation IssueClose($input:CloseIssueInput!){closeIssue(input: $input){issue{id}}}",
        json!({"input": {"issueId": issue_id, "stateReason": "NOT_PLANNED"}}),
    )
    .await;
    assert_eq!(closed["closeIssue"]["issue"]["id"], issue_id.as_str());
    let state = data(
        &h,
        &token,
        ISSUE_BY_NUMBER,
        json!({"owner": "team", "repo": "private", "number": 1}),
    )
    .await;
    assert_eq!(
        (
            state["repository"]["issue"]["state"].as_str(),
            state["repository"]["issue"]["stateReason"].as_str()
        ),
        (Some("CLOSED"), Some("NOT_PLANNED"))
    );
    data(
        &h,
        &token,
        "mutation IssueReopen($input:ReopenIssueInput!){reopenIssue(input: $input){issue{id}}}",
        json!({"input": {"issueId": issue_id}}),
    )
    .await;

    // REST shares the same resolvers and reports the reopened issue.
    let issue: Value = rest(
        &h,
        &token,
        Method::GET,
        "/repos/team/private/issues/1",
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        (
            issue["state"].as_str(),
            issue["comments"].as_u64(),
            issue["user"]["login"].as_str()
        ),
        (Some("open"), Some(1), Some("alice"))
    );
    let listed: Value = rest(
        &h,
        &token,
        Method::GET,
        "/repos/team/private/issues?state=all",
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    h.close().await;
}

const PULL_REQUEST_LIST: &str = r#"
query PullRequestList($owner: String!, $repo: String!, $limit: Int!, $endCursor: String, $baseBranch: String, $headBranch: String, $state: [PullRequestState!] = OPEN) {
    repository(owner: $owner, name: $repo) {
        pullRequests(states: $state, baseRefName: $baseBranch, headRefName: $headBranch, first: $limit, after: $endCursor, orderBy: {field: CREATED_AT, direction: DESC}) {
            totalCount
            nodes {
                number,title,state,url,headRefName,headRepositoryOwner{id,login,...on User{name}},isCrossRepository,isDraft,createdAt
            }
            pageInfo {
                hasNextPage
                endCursor
            }
        }
    }
}"#;

#[tokio::test]
async fn tokens_push_over_github_style_urls_and_drive_pull_requests() {
    let h = rooted().await;
    let (_, token) = alice(&h).await;
    let created = data(
        &h,
        &token,
        "mutation RepositoryCreate($input: CreateRepositoryInput!) { createRepository(input: $input) { repository { id name owner { login } url } } }",
        json!({"input": {"name": "tools", "visibility": "PRIVATE", "hasWikiEnabled": true, "hasIssuesEnabled": true}}),
    )
    .await;
    let repository = &created["createRepository"]["repository"];
    assert_eq!(repository["owner"]["login"], "alice");

    // gh's credential helper answers with the login, not Crab's `crab`.
    let mut remote = url::Url::parse(&format!("{}/alice/tools.git", h.origin)).unwrap();
    remote.set_username("alice").unwrap();
    remote.set_password(Some(&token)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "# Tools\n").unwrap();
    git::success(path, &["add", "README.md"]).await;
    git::success(path, &["commit", "-m", "Initial commit"]).await;
    let pushed = git::git(path, &["push", remote.as_str(), "main"]).await;
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr).replace(&token, "[token]")
    );
    git::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "# Tools\n\nNow with docs.\n").unwrap();
    git::success(path, &["commit", "-am", "Document tools"]).await;
    let pushed = git::git(path, &["push", remote.as_str(), "feature"]).await;
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr).replace(&token, "[token]")
    );
    let clone = tempfile::tempdir().unwrap();
    let cloned = git::git(clone.path(), &["clone", remote.as_str(), "."]).await;
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr).replace(&token, "[token]")
    );

    let pull = data(
        &h,
        &token,
        "mutation PullRequestCreate($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { id url } } }",
        json!({"input": {"repositoryId": repository["id"], "baseRefName": "main", "headRefName": "feature", "title": "Document tools", "body": "Adds a line", "draft": false, "maintainerCanModify": true}}),
    )
    .await;
    let pull_id = pull["createPullRequest"]["pullRequest"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        pull["createPullRequest"]["pullRequest"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/alice/tools/pull/1")
    );

    let list = data(
        &h,
        &token,
        PULL_REQUEST_LIST,
        json!({"owner": "alice", "repo": "tools", "limit": 30}),
    )
    .await;
    let first = &list["repository"]["pullRequests"]["nodes"][0];
    assert_eq!(
        (
            first["headRefName"].as_str(),
            first["headRepositoryOwner"]["login"].as_str(),
            first["isCrossRepository"].as_bool(),
            first["isDraft"].as_bool(),
        ),
        (Some("feature"), Some("alice"), Some(false), Some(false))
    );
    let detail = data(
        &h,
        &token,
        "query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $pr_number) {id,number,state,title,additions,deletions,changedFiles,commits{totalCount},baseRefName,headRefName,headRefOid,mergeStateStatus,reviewDecision,files(first:100){nodes{path,additions,deletions,changeType}},statusCheckRollup: commits(last: 1){nodes{commit{oid,statusCheckRollup{state}}}}} } }",
        json!({"owner": "alice", "repo": "tools", "pr_number": 1}),
    )
    .await;
    let detail = &detail["repository"]["pullRequest"];
    assert_eq!(
        (
            detail["additions"].as_u64(),
            detail["deletions"].as_u64(),
            detail["changedFiles"].as_u64(),
            detail["commits"]["totalCount"].as_u64(),
            detail["files"]["nodes"][0]["path"].as_str(),
            detail["mergeStateStatus"].as_str(),
            detail["statusCheckRollup"]["nodes"][0]["commit"]["statusCheckRollup"].is_null(),
        ),
        (
            Some(2),
            Some(0),
            Some(1),
            Some(1),
            Some("README.md"),
            Some("CLEAN"),
            true
        )
    );

    let diff = h
        .http
        .get(format!("{}/api/v3/repos/alice/tools/pulls/1", h.origin))
        .header(header::AUTHORIZATION, format!("token {token}"))
        .header(header::ACCEPT, "application/vnd.github.v3.diff")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        diff.starts_with("diff --git a/README.md b/README.md\n"),
        "{diff}"
    );
    assert!(diff.contains("+Now with docs.\n"), "{diff}");

    data(
        &h,
        &token,
        "mutation PullRequestMerge($input:MergePullRequestInput!){mergePullRequest(input: $input){clientMutationId}}",
        json!({"input": {"pullRequestId": pull_id, "mergeMethod": "MERGE", "expectedHeadOid": detail["headRefOid"]}}),
    )
    .await;
    let merged: Value = rest(&h, &token, Method::GET, "/repos/alice/tools/pulls/1", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (merged["state"].as_str(), merged["merged"].as_bool()),
        (Some("closed"), Some(true))
    );
    let deleted = rest(
        &h,
        &token,
        Method::DELETE,
        "/repos/alice/tools/git/refs/heads/feature",
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let branches: Value = rest(&h, &token, Method::GET, "/repos/alice/tools/branches", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        branches
            .as_array()
            .unwrap()
            .iter()
            .map(|branch| branch["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["main"]
    );
    h.close().await;
}

#[tokio::test]
async fn repository_attributes_reach_graphql_and_rest() {
    // Attributes live in the catalog, which needs a rooted harness.
    let h = rooted().await;
    let id = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap()
        .id;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(id)
        .await
        .unwrap();
    let (cookie, token) = alice(&h).await;
    h.write(
        Method::PUT,
        &format!("/api/repositories/{id}/attributes"),
        &cookie,
        json!({"homepage": "https://crab.build/docs", "license": "MIT", "is_template": true, "version": 0}),
    )
    .await
    .error_for_status()
    .unwrap();
    let graphql = data(
        &h,
        &token,
        r#"{ repository(owner: "team", name: "private") { homepageUrl isTemplate } }"#,
        json!({}),
    )
    .await;
    let rest: Value = rest(&h, &token, Method::GET, "/repos/team/private", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            graphql["repository"].clone(),
            rest["homepage"].clone(),
            rest["is_template"].clone()
        ),
        (
            json!({"homepageUrl": "https://crab.build/docs", "isTemplate": true}),
            json!("https://crab.build/docs"),
            json!(true)
        )
    );
    h.close().await;
}

#[tokio::test]
async fn scopes_roles_and_account_state_bound_every_token() {
    let h = Harness::new(false).await;
    let (cookie, full) = alice(&h).await;
    let public_only = token(&h, &cookie, &["read:org"]).await;
    // Without `repo`, a token reads public data only.
    let hidden = graphql(
        &h,
        &public_only,
        REPOSITORY_INFO,
        json!({"owner": "team", "name": "private"}),
    )
    .await;
    assert_eq!(hidden["errors"][0]["type"], "NOT_FOUND");
    assert_eq!(
        rest(&h, &public_only, Method::GET, "/repos/team/private", None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.choose_username(&bob, "bob").await;
    let reader = token(&h, &bob, &["repo"]).await;
    let repository = data(
        &h,
        &reader,
        REPOSITORY_INFO,
        json!({"owner": "team", "name": "private"}),
    )
    .await;
    assert_eq!(repository["repository"]["viewerPermission"], "READ");
    let label = rest(
        &h,
        &reader,
        Method::POST,
        "/repos/team/private/labels",
        Some(json!({"name": "triage", "color": "ffcc00"})),
    )
    .await;
    assert_eq!(label.status(), StatusCode::FORBIDDEN);

    // Suspension revokes tokens for good, like the sessions issued before it.
    let auth = h.server.auth.as_ref().unwrap();
    let state = auth.account_state("alice-id").await.unwrap();
    auth.set_account_suspended("alice-id", state.revision, true)
        .await
        .unwrap();
    assert_eq!(
        rest(&h, &full, Method::GET, "/user", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let state = auth.account_state("alice-id").await.unwrap();
    auth.set_account_suspended("alice-id", state.revision, false)
        .await
        .unwrap();
    assert_eq!(
        rest(&h, &full, Method::GET, "/user", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    h.close().await;
}
