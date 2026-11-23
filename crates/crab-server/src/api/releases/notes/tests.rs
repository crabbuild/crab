use reqwest::{Method, header};
use url::Url;
use uuid::Uuid;

use super::*;
use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

const REPO: &str = "/api/repos/team/private";

async fn write(h: &Harness, actor: &str, path: &str, input: Value) -> Value {
    let response = h
        .write(Method::POST, &format!("{REPO}/{path}"), actor, input)
        .await;
    let status = response.status();
    let output: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status}: {output}");
    output
}

async fn merge(h: &Harness, actor: &str, base: &str, head: &str, title: &str) -> String {
    let pull = write(
        h,
        actor,
        "pulls",
        json!({
            "request_id": Uuid::now_v7(),
            "title": title,
            "body": "",
            "base_ref": format!("refs/heads/{base}"),
            "head_ref": format!("refs/heads/{head}"),
        }),
    )
    .await;
    let output = write(
        h,
        actor,
        &format!("pulls/{}/merge", pull["number"]),
        json!({
            "request_id": Uuid::now_v7(),
            "version": pull["version"],
            "base_oid": pull["base_oid"],
            "head_oid": pull["head_oid"],
            "method": "merge_commit",
        }),
    )
    .await;
    output["merge"]["commit_oid"].as_str().unwrap().to_owned()
}

async fn topic(path: &std::path::Path, remote: &str, base: &str, name: &str) {
    git::success(path, &["checkout", "-b", name, base]).await;
    std::fs::write(path.join(format!("{name}.txt")), "topic\n").unwrap();
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", name]).await;
    git::success(path, &["push", remote, name]).await;
}

async fn history(h: &Harness, alice: &str) -> (String, String) {
    h.write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        alice,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git::push_review_branches(path, remote.as_str()).await;
    git::success(path, &["push", remote.as_str(), "main:maintenance"]).await;

    let previous = merge(h, alice, "main", "feature", "Already released").await;
    git::success(path, &["fetch", remote.as_str(), "main"]).await;
    git::success(
        path,
        &[
            "tag",
            "-a",
            "release/v1",
            "-m",
            "Previous release",
            "FETCH_HEAD",
        ],
    )
    .await;
    git::success(path, &["push", remote.as_str(), "refs/tags/release/v1"]).await;
    write(
        h,
        alice,
        "releases",
        json!({
            "request_id":Uuid::now_v7(), "tag_name":"release/v1", "target_oid":previous,
            "title":"Previous release", "body":"", "prerelease":false, "draft":false,
        }),
    )
    .await;

    topic(path, remote.as_str(), "FETCH_HEAD", "current").await;
    let target = merge(h, alice, "main", "current", "Ship [the] new feature").await;
    topic(path, remote.as_str(), "main", "maintenance-fix").await;
    let maintenance = merge(
        h,
        alice,
        "maintenance",
        "maintenance-fix",
        "Maintenance only",
    )
    .await;
    write(
        h,
        alice,
        "releases",
        json!({
            "request_id":Uuid::now_v7(), "tag_name":"maintenance/v2", "target_oid":maintenance,
            "title":"Maintenance release", "body":"", "prerelease":false, "draft":false,
        }),
    )
    .await;
    (target, previous)
}

#[tokio::test(flavor = "multi_thread")]
async fn draft_notes_are_scoped_to_the_selected_commit_range_and_current_writer() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (target, previous) = history(&h, &alice).await;
    let route = format!("{REPO}/releases/generate-notes?target_oid={target}");
    let notes = h.json(&route, &alice).await;
    assert_eq!(notes["target_oid"], target);
    assert_eq!(notes["previous_tag"], "release/v1");
    assert_eq!(notes["previous_oid"], previous);
    let body = notes["body"].as_str().unwrap();
    assert!(
        body.contains(r"Ship \[the\] new feature by Alice in [#2]"),
        "{body}"
    );
    assert!(body.contains("## Contributors\n\n- Alice\n"), "{body}");
    assert!(body.contains(&format!("/commits/{target}?base={previous}&")));
    assert!(!body.contains("Already released") && !body.contains("Maintenance only"));
    assert_eq!(
        h.json(&format!("{route}&previous_tag=release%2Fv1"), &alice)
            .await,
        notes
    );

    let all = h.json(&format!("{route}&previous_tag="), &alice).await;
    assert!(all["previous_tag"].is_null());
    assert!(all["body"].as_str().unwrap().contains("Already released"));

    let unrelated = h
        .http
        .get(format!("{}{route}&previous_tag=maintenance%2Fv2", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(unrelated.status(), StatusCode::BAD_REQUEST);

    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let forbidden = h
        .http
        .get(format!("{}{route}", h.origin))
        .header(header::COOKIE, reader)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_previews_inserts_and_publishes_generated_release_notes() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (target, _) = history(&h, &alice).await;
    let browser = Browser::new("generated-release-notes");
    browser
        .run(&["open", &format!("{}/team/private/releases/new", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    click("button", "Continue to sign in").await;
    browser.run(&["wait", "#release-notes"]).await;
    browser.run(&["fill", "#release-tag", "release/v2"]).await;
    browser
        .run(&["fill", "#release-title", "Current release"])
        .await;
    browser
        .run(&["fill", "#release-notes", "Written by the release manager."])
        .await;
    click("button", "Generate release notes").await;
    browser
        .run(&["wait", "--text", "Preview notes since release/v1"])
        .await;
    click("button", "Insert generated notes").await;
    click("button", "Publish release").await;
    browser
        .run(&["wait", "--text", "Written by the release manager."])
        .await;
    click("link", "Full changelog").await;
    browser.run(&["wait", "--text", "Commits since"]).await;
    browser.run(&["wait", "--text", "current"]).await;
    browser.run(&["back"]).await;
    click("link", "#2").await;
    browser.run(&["wait", "--url", "**/pull/2*"]).await;

    let record = h
        .json(&format!("{REPO}/releases/tag/release%2Fv2"), &alice)
        .await;
    assert_eq!(record["target_oid"], target);
    let body = record["body"].as_str().unwrap();
    assert!(body.starts_with("Written by the release manager.\n\n"));
    assert!(body.contains("new feature by Alice"));
    assert!(!body.contains("Already released") && !body.contains("Maintenance only"));
    drop(browser);
    h.close().await;
}
