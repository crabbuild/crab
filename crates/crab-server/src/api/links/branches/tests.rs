use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

const ISSUE: &str = "/api/repos/team/private/issues/1";
const EDIT: &str = "/api/repos/team/private/issues/1/development-branches";
const LINKS: &str = "/api/repos/team/private/issues/1/links?include_branches=true";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_branch_creation_recovers_after_its_head_moves() {
    use crate::{
        app,
        auth::Principal,
        cells::repository::collaboration::links::branches::{
            Branch,
            creation::{Decision, Request, Reserve, Status},
        },
        cells::repository::issues::transfer::{Capture, CaptureInput, CaptureOutcome},
        git::receive::{self, IssueBranchPublication},
        repository::development_branches,
    };
    use std::sync::Arc;

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    let (target_id, remote) = seed(&h, &alice, checkout.path()).await;
    let target = h.server.repositories.by_id(target_id).unwrap();
    let source = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let commit = git::success(checkout.path(), &["rev-parse", "main"]).await;
    let occupied = json!({"request_id":Uuid::now_v7(),"repository":target.id,"name":"main","source_oid":commit});
    assert_eq!(
        h.write(Method::POST, EDIT, &alice, occupied).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(h.json(LINKS, &alice).await["branches"]["items"], json!([]));

    let principal = Principal::local();
    let request = Request {
        id: Uuid::now_v7(),
        issue: 1,
        branch: Branch {
            repository: target.id,
            reference: "refs/heads/from-issue".into(),
        },
        commit,
        actor: app::account(&principal).unwrap(),
        target_epoch: target.ownership_epoch,
    };
    let cell = app::repository_cell(&h.server, &source).unwrap();
    let reserved = cell
        .client
        .command::<Reserve>(
            cell.target,
            app::mutation_identity().unwrap(),
            request.clone(),
        )
        .await
        .unwrap()
        .output;
    assert!(matches!(reserved, Decision::Saved(Status::Pending)));
    let version = h.json(ISSUE, &alice).await["version"].as_u64().unwrap();
    assert!(matches!(
        cell.client
            .query::<Capture>(
                cell.target,
                None,
                CaptureInput::Start {
                    number: 1,
                    expected_version: version
                },
            )
            .await
            .unwrap()
            .output,
        CaptureOutcome::LinkedWork
    ));

    // Leave the issue-side request pending as if the create response was lost
    // after the Git publication worker returned.
    receive::create_issue_branch(
        Arc::clone(&h.server),
        principal.clone(),
        &target,
        IssueBranchPublication {
            source: Arc::clone(&source),
            request: request.clone(),
        },
    )
    .await
    .unwrap();
    git::success(
        checkout.path(),
        &["push", remote.as_str(), "HEAD:refs/heads/from-issue"],
    )
    .await;
    assert_ne!(
        target
            .ref_oid(&request.branch.reference)
            .await
            .unwrap()
            .as_deref(),
        Some(request.commit.as_str())
    );
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            development_branches::recover(&h.server, &source)
                .await
                .unwrap();
            if h.json(LINKS, &alice).await["branches"]["items"][0]["reference"]
                == "refs/heads/from-issue"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        development_branches::submit(Arc::clone(&h.server), principal, source, target, request)
            .await
            .unwrap(),
        Status::Created
    );
    h.close().await;
}

async fn seed(h: &Harness, alice: &str, checkout: &std::path::Path) -> (Uuid, Url) {
    seed_repository(h, alice, checkout, "library").await
}

async fn seed_repository(
    h: &Harness,
    alice: &str,
    checkout: &std::path::Path,
    name: &str,
) -> (Uuid, Url) {
    if name != "private" {
        h.add_repository(name).await;
    }
    let target = h
        .server
        .repositories
        .get(&("team".into(), name.into()))
        .unwrap();
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{}/git/team/{name}.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    git::push_review_branches(checkout, remote.as_str()).await;
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        alice,
        json!({"request_id":Uuid::now_v7(),"title":"Plan the release","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    (target.id, remote)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_promotes_an_issue_branch_to_its_pull_and_closes_the_issue_on_merge() {
    use crate::{
        app,
        auth::Principal,
        cells::repository::collaboration::links::branches::{
            Branch,
            creation::{Decision, Request, Reserve, Status},
        },
        git::receive::{self, IssueBranchPublication},
    };
    use std::sync::Arc;

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    let (repo, remote) = seed_repository(&h, &alice, checkout.path(), "private").await;
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Issue with a pending branch result","body":""
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let source = h.server.repositories.by_id(repo).unwrap();
    let principal = Principal::local();
    let request = Request {
        id: Uuid::now_v7(),
        issue: 2,
        branch: Branch {
            repository: repo,
            reference: "refs/heads/rollout".into(),
        },
        commit: git::success(checkout.path(), &["rev-parse", "main"]).await,
        actor: app::account(&principal).unwrap(),
        target_epoch: source.ownership_epoch,
    };
    let cell = app::repository_cell(&h.server, &source).unwrap();
    assert!(matches!(
        cell.client
            .command::<Reserve>(
                cell.target,
                app::mutation_identity().unwrap(),
                request.clone()
            )
            .await
            .unwrap()
            .output,
        Decision::Saved(Status::Pending)
    ));
    receive::create_issue_branch(
        Arc::clone(&h.server),
        principal,
        &source,
        IssueBranchPublication {
            source: Arc::clone(&source),
            request,
        },
    )
    .await
    .unwrap();
    git::success(
        checkout.path(),
        &["push", remote.as_str(), "HEAD:refs/heads/rollout"],
    )
    .await;
    h.write(
        Method::PUT,
        EDIT,
        &alice,
        json!({
            "branch":{"repository":repo,"reference":"refs/heads/rollout"},"version":0,"active":true
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let browser = Browser::new("branch-pr");
    let compare = format!("{}/team/private/compare/main...rollout", h.origin);
    browser.run(&["open", &compare]).await;
    browser.sign_in().await;
    browser.run(&["wait", "#discussion-title"]).await;
    browser
        .run(&["fill", "#discussion-title", "Ship the planned feature"])
        .await;
    let click = |label| {
        let browser = &browser;
        async move {
            browser
                .run(&[
                    "find", "role", "button", "click", "--name", label, "--exact",
                ])
                .await
        }
    };
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => !button.disabled && button.textContent.trim() === 'Create pull request')"]).await;
    click("Create pull request").await;
    browser
        .run(&["wait", "--url", "**/team/private/pull/1*"])
        .await;
    browser.run(&["wait", "--text", "From linked branch"]).await;
    let issue = h.json(LINKS, &alice).await;
    assert_eq!(issue["branches"]["items"], json!([]));
    assert_eq!(issue["items"][0]["from_branch"], true);
    assert_eq!(issue["items"][0]["closes_on_merge"], true);
    assert_eq!(
        h.json("/api/repos/team/private/issues/2/links", &alice)
            .await["items"],
        json!([])
    );

    let mut members = source.config.members.clone();
    members
        .iter_mut()
        .find(|member| member.subject == "bob-id")
        .unwrap()
        .access = crate::RepositoryAccess::Write;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let head = git::success(checkout.path(), &["rev-parse", "HEAD"]).await;
    h.write(
        Method::POST,
        "/api/repos/team/private/pulls/1/reviews",
        &bob,
        json!({
            "request_id":Uuid::now_v7(),"state":"approved","body":"","commit_oid":head,
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    h.http
        .post(format!(
            "{}/api/repos/team/private/statuses/{head}",
            h.origin
        ))
        .basic_auth("crab", remote.password())
        .json(&json!({"request_id":Uuid::now_v7(),"context":"ci/test","state":"success"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pull = format!("{}/team/private/pull/1", h.origin);
    browser.run(&["open", &pull]).await;
    browser.run(&["wait", "--text", "Merge pull request"]).await;
    click("Merge pull request").await;
    click("Confirm merge").await;
    browser
        .run(&["wait", "--text", "Pull request merged"])
        .await;
    assert_eq!(h.json(ISSUE, &alice).await["state"], "closed");
    assert_eq!(
        h.json("/api/repos/team/private/issues/2", &alice).await["state"],
        "closed"
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn development_branches_follow_the_issue_and_recheck_private_target_grants() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    let (target, remote) = seed(&h, &alice, checkout.path()).await;
    let input = json!({"branch":{"repository":target,"reference":"refs/heads/feature"},"version":0,"active":true});
    let mut missing = input.clone();
    missing["branch"]["reference"] = json!("refs/heads/missing");
    assert_eq!(
        h.write(Method::PUT, EDIT, &alice, missing).await.status(),
        StatusCode::NOT_FOUND
    );
    for _ in 0..2 {
        let result: Value = h
            .write(Method::PUT, EDIT, &alice, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["version"], 1);
    }
    assert_eq!(
        h.json(LINKS, &alice).await["branches"]["items"],
        json!([{
            "repository":{"id":target,"owner":"team","name":"library"},
            "reference":"refs/heads/feature","exists":true,
        }])
    );
    let mut stale = input.clone();
    stale["branch"]["reference"] = json!("refs/heads/main");
    assert_eq!(
        h.write(Method::PUT, EDIT, &alice, stale).await.status(),
        StatusCode::CONFLICT
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(LINKS, &bob).await["branches"]["items"][0]["exists"],
        true
    );
    let mut members = h
        .server
        .repositories
        .by_id(target)
        .unwrap()
        .config
        .members
        .clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "library", members, true)
        .await
        .unwrap();
    assert_eq!(h.json(LINKS, &bob).await["branches"]["items"], json!([]));
    assert_eq!(
        h.write(Method::PUT, EDIT, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut source_members = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap()
        .config
        .members
        .clone();
    source_members
        .iter_mut()
        .find(|member| member.subject == "bob-id")
        .unwrap()
        .access = crate::RepositoryAccess::Write;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", source_members, true)
        .await
        .unwrap();
    assert_eq!(
        h.write(Method::PUT, EDIT, &bob, input).await.status(),
        StatusCode::NOT_FOUND
    );

    h.add_repository("destination").await;
    let destination = h
        .server
        .repositories
        .get(&("team".into(), "destination".into()))
        .unwrap();
    let version = h.json(ISSUE, &alice).await["version"].clone();
    h.write(
        Method::POST,
        &format!("{ISSUE}/transfer"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"destination":destination.id,"version":version}),
    )
    .await
    .error_for_status()
    .unwrap();
    let location = h.json(ISSUE, &alice).await;
    assert_eq!(location["transfer"]["state"], "ready");
    let moved = "/api/repos/team/destination/issues/1/links?include_branches=true";
    assert_eq!(
        h.json(moved, &alice).await["branches"]["items"][0]["reference"],
        "refs/heads/feature"
    );
    assert_eq!(h.json(moved, &bob).await["branches"]["items"], json!([]));
    git::success(
        checkout.path(),
        &["push", remote.as_str(), "--delete", "feature"],
    )
    .await;
    assert_eq!(
        h.json(moved, &alice).await["branches"]["items"][0]["exists"],
        false
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_links_a_branch_from_another_repository_and_unlinks_it_after_reload() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    let (target, _) = seed(&h, &alice, checkout.path()).await;
    let browser = Browser::new("link-development-branch");
    let issue_url = format!("{}/team/private/issues/1", h.origin);
    browser.run(&["open", &issue_url]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Linked branches"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Link branch",
            "--exact",
        ])
        .await;
    let option = format!(
        r##"document.querySelector("#branch-repository option[value='{target}']") !== null"##
    );
    browser.run(&["wait", "--fn", &option]).await;
    browser
        .run(&["select", "#branch-repository", &target.to_string()])
        .await;
    browser.run(&["wait", "#development-branch"]).await;
    browser
        .run(&["select", "#development-branch", "refs/heads/feature"])
        .await;
    browser
        .run(&["click", "[role=dialog] button[type=submit]"])
        .await;
    let linked = "team/library:feature";
    browser.run(&["wait", "--text", linked]).await;
    browser.run(&["open", &issue_url]).await;
    browser.run(&["wait", "--text", linked]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Unlink team/library:feature",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No linked branches."])
        .await;
    assert_eq!(h.json(LINKS, &alice).await["branches"]["items"], json!([]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_creates_a_branch_from_the_issue_in_the_selected_repository() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let checkout = tempfile::tempdir().unwrap();
    let (target, _) = seed(&h, &alice, checkout.path()).await;
    let commit = git::success(checkout.path(), &["rev-parse", "main"]).await;
    let browser = Browser::new("issue-branch");
    let issue = format!("{}/team/private/issues/1", h.origin);
    browser.run(&["open", &issue]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Create a branch"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create a branch",
            "--exact",
        ])
        .await;
    let option = format!(
        r##"document.querySelector("#branch-repository option[value='{target}']") !== null"##
    );
    browser.run(&["wait", "--fn", &option]).await;
    browser
        .run(&["select", "#branch-repository", &target.to_string()])
        .await;
    browser
        .run(&["wait", "[role=dialog] button[type=submit]:enabled"])
        .await;
    browser
        .run(&["click", "[role=dialog] button[type=submit]"])
        .await;
    browser
        .run(&["wait", "--text", "team/library:issue-1"])
        .await;
    browser.run(&["open", &issue]).await;
    browser
        .run(&["wait", "--text", "team/library:issue-1"])
        .await;
    let repository = h.server.repositories.by_id(target).unwrap();
    assert_eq!(
        repository.ref_oid("refs/heads/issue-1").await.unwrap(),
        Some(commit)
    );
    h.close().await;
}
