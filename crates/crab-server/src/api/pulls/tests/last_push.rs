use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const REPO: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";

#[tokio::test(flavor = "multi_thread")]
async fn last_push_approval_requires_a_current_independent_writer() {
    exercise(None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_shows_the_independent_last_push_approval() {
    exercise(Some(Browser::new("independent-push-review"))).await;
}

async fn exercise(browser: Option<Browser>) {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.provider
        .claims
        .lock()
        .await
        .insert("sub".into(), json!("carol-id"));
    h.provider
        .claims
        .lock()
        .await
        .insert("preferred_username".into(), json!("Carol"));
    let carol = h.login().await;
    h.provider.claims.lock().await.clear();
    *h.provider.mode.lock().await = String::new();

    let members = format!("{REPO}/members");
    let roster = |version: Value, carol: &str| {
        json!({
            "expected_revision":version, "groups": [],
            "members":[
                {"subject":"alice-id","name":"Alice","access":"admin"},
                {"subject":"bob-id","name":"Bob","access":"write"},
                {"subject":"carol-id","name":"Carol","access":carol}
            ]
        })
    };
    h.write(
        Method::PUT,
        &members,
        &alice,
        roster(h.json(&members, &alice).await["revision"].clone(), "write"),
    )
    .await
    .error_for_status()
    .unwrap();
    let remote = async |cookie: &str| {
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                cookie,
                json!({"owner":"team","repository":"private","access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
        url.set_username("crab").unwrap();
        url.set_password(token["token"].as_str()).unwrap();
        url
    };
    let alice_remote = remote(&alice).await;
    let bob_remote = remote(&bob).await;
    let work = tempfile::tempdir().unwrap();
    let path = work.path();
    let head = git::push_review_branches(path, alice_remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Independently review the latest push","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","require_last_push_approval":true}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let review = async |cookie: &str, oid: &str| {
        let input =
            json!({"request_id":Uuid::now_v7(),"state":"approved","body":"","commit_oid":oid});
        h.write(
            Method::POST,
            &format!("{PULL}/reviews"),
            cookie,
            input.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
        input
    };
    review(&carol, &head).await;
    assert_eq!(
        h.json(PULL, &alice).await["merge_requirements"]["last_push_approval"],
        "approved"
    );

    // Restoring the same commit is a distinct ref publication. A review of
    // that SHA from its previous incarnation cannot approve Bob's push.
    std::fs::write(path.join("README.md"), "Second version\n").unwrap();
    git(&["commit", "-am", "Second version"]).await;
    git(&["push", bob_remote.as_str(), "feature"]).await;
    git(&["push", bob_remote.as_str(), "--delete", "feature"]).await;
    let restored = format!("{head}:refs/heads/feature");
    git(&["push", bob_remote.as_str(), &restored]).await;
    review(&bob, &head).await;
    let blocked = h.json(PULL, &alice).await;
    assert_eq!(blocked["head_oid"], head);
    assert_eq!(
        blocked["merge_requirements"]["last_push_approval"],
        "pending"
    );
    assert_eq!(blocked["can_merge"], false);
    let merge = |pull: &Value| {
        json!({
            "request_id":Uuid::now_v7(),"version":pull["version"],"method":"merge_commit",
            "base_oid":pull["base_oid"],"head_oid":pull["head_oid"],"message":"Merge independent review"
        })
    };
    let endpoint = format!("{PULL}/merge");
    assert_eq!(
        h.write(Method::POST, &endpoint, &alice, merge(&blocked))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    if let Some(browser) = &browser {
        browser
            .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
            .await;
        browser.sign_in().await;
        browser
            .run(&["wait", "--text", "The latest push needs approval"])
            .await;
    }
    let carol_review = review(&carol, &head).await;
    let approved = h.json(PULL, &alice).await;
    assert_eq!(approved["can_merge"], true);
    if let Some(browser) = &browser {
        browser
            .run(&[
                "wait",
                "--text",
                "The latest push has an independent approval.",
            ])
            .await;
    }

    // Revoking Write takes effect without changing the recorded Git or PR
    // version, and replaying an existing review cannot restore eligibility.
    h.write(
        Method::PUT,
        &members,
        &alice,
        roster(h.json(&members, &alice).await["revision"].clone(), "read"),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{PULL}/reviews"),
        &carol,
        carol_review,
    )
    .await
    .error_for_status()
    .unwrap();
    let denied = h.json(PULL, &alice).await;
    assert_eq!(denied["version"], approved["version"]);
    assert_eq!(
        denied["merge_requirements"]["last_push_approval"],
        "pending"
    );
    assert_eq!(
        h.write(Method::POST, &endpoint, &alice, merge(&approved))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(
        Method::PUT,
        &members,
        &alice,
        roster(h.json(&members, &alice).await["revision"].clone(), "write"),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST, &endpoint, &alice, merge(&approved))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(h.json(PULL, &alice).await["state"], "merged");
    drop(browser);
    h.close().await;
}
