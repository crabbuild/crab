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

async fn seed(h: &Harness, alice: &str, checkout: &std::path::Path) -> (Uuid, Url) {
    h.add_repository("library").await;
    let target = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{}/git/team/library.git", h.origin)).unwrap();
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
