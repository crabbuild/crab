use reqwest::Method;
use serde_json::{Value, json};

use crate::{
    cells::{
        directory::IdentityKey,
        repository::recent_pushes::{Branch, Intent, Record, RecordInput},
    },
    test_support::{
        git,
        http::{Harness, browser::Browser},
    },
};

use super::branch_restore::remote;

const ENDPOINT: &str = "/api/repos/team/private/pulls/recent-pushes";

#[tokio::test(flavor = "multi_thread")]
async fn recent_pushes_are_private_and_require_the_live_committed_publication() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let alice_remote = remote(&h, &alice).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let head = git::push_review_branches(path, alice_remote.as_str()).await;
    let actual = json!([{"name":"refs/heads/feature","oid":head}]);
    let recorded = h.json(ENDPOINT, &alice).await;
    assert_eq!(recorded["base_ref"], "refs/heads/main");
    assert_eq!(recorded["items"], actual);

    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let routed = crate::app::repository_cell(&h.server, &repo).unwrap();
    routed
        .client
        .command::<Record>(
            &routed.target,
            crate::app::mutation_identity().unwrap(),
            RecordInput {
                actor: IdentityKey {
                    issuer: h.server.auth.as_ref().unwrap().issuer().into(),
                    subject: "alice-id".into(),
                },
                intent: Intent {
                    plan: "a".repeat(64),
                    branches: vec![Branch {
                        name: "refs/heads/feature".into(),
                        oid: head.clone(),
                    }],
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(h.json(ENDPOINT, &alice).await["items"], actual);

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(ENDPOINT, &bob).await["items"], json!([]));
    let members = "/api/repos/team/private/members";
    let acl = h.json(members, &alice).await;
    h.write(
        Method::PUT,
        members,
        &alice,
        json!({"expected_revision":acl["revision"],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":"write"}
        ],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let bob_remote = remote(&h, &bob).await;
    git::success(path, &["push", bob_remote.as_str(), "--delete", "feature"]).await;
    assert_eq!(h.json(ENDPOINT, &alice).await["items"], json!([]));
    git::success(path, &["push", bob_remote.as_str(), "feature"]).await;
    assert_eq!(h.json(ENDPOINT, &alice).await["items"], json!([]));
    assert_eq!(h.json(ENDPOINT, &bob).await["items"], actual);
    let root = "/api/repos/team/private";
    let opened: Value = h
        .write(
            Method::POST,
            &format!("{root}/pulls"),
            &bob,
            json!({"request_id":uuid::Uuid::now_v7(),"title":"Review the branch","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h.json(ENDPOINT, &bob).await["items"], json!([]));
    h.write(
        Method::PUT,
        &format!("{root}/settings/branch-protections"),
        &alice,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let merge = h.write(Method::POST, &format!("{root}/pulls/1/merge"), &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"version":opened["version"],"method":"merge_commit",
            "base_oid":opened["base_oid"],"head_oid":head}))
        .await.error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(merge["state"], "merged");
    assert_eq!(
        repo.ref_oid("refs/heads/feature").await.unwrap().as_deref(),
        Some(head.as_str())
    );
    assert_eq!(h.json(ENDPOINT, &bob).await["items"], json!([]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_opens_a_pull_from_a_stock_git_push() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let url = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(checkout.path(), url.as_str()).await;
    let browser = Browser::new("recent-push");
    let root = format!("{}/team/private", h.origin);
    let label = "Compare & pull request for feature";
    let wait_for_link = async || {
        browser
            .run(&["wait", "[aria-label='Compare & pull request for feature']"])
            .await;
    };
    browser.run(&["open", &root]).await;
    browser.sign_in().await;
    wait_for_link().await;
    browser.run(&["open", &format!("{root}/pulls")]).await;
    wait_for_link().await;
    browser
        .run(&["find", "role", "link", "click", "--name", label, "--exact"])
        .await;
    browser
        .run(&["wait", "--url", "**/compare/main...feature*"])
        .await;
    browser.run(&["wait", "#discussion-title"]).await;
    browser
        .run(&["find", "label", "Title", "fill", "Review the recent push"])
        .await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent.trim() === 'Create pull request' && !button.disabled)"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create pull request",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--url", "**/pull/1"]).await;
    let pull: Value = h.json("/api/repos/team/private/pulls/1", &writer).await;
    assert_eq!(pull["head_oid"], head);
    assert_eq!(h.json(ENDPOINT, &writer).await["items"], json!([]));
    browser
        .run(&[
            "open",
            &format!("{root}/compare/main...feature?title=Another+review"),
        ])
        .await;
    browser
        .run(&["wait", "--text", "View pull request #1"])
        .await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent.trim() === 'Create pull request' && button.disabled)"]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "View pull request #1",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--url", "**/pull/1*"]).await;
    drop(browser);
    h.close().await;
}
