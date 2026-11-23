use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app,
    cells::repository::{CreatePull, CreatePullInput, CreatePullOutcome, RepositoryAuthor},
    test_support::http::{Harness, browser::Browser},
};

const REPO: &str = "/api/repos/team/private";

async fn issue(h: &Harness, alice: &str, title: &str) {
    h.write(
        Method::POST,
        &format!("{REPO}/issues"),
        alice,
        json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn post(h: &Harness, who: &str, target: &str, body: &str, id: Uuid) {
    h.write(
        Method::POST,
        &format!("{REPO}/{target}/comments"),
        who,
        json!({"request_id":id,"body":body}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn events(h: &Harness, who: &str, target: &str, kind: &str) -> Vec<Value> {
    let page = h.json(&format!("{REPO}/{target}/activity"), who).await;
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["action"]["kind"] == kind)
        .cloned()
        .collect()
}

async fn undo(h: &Harness, who: &str, target: &str, event: u64) -> reqwest::Response {
    h.write(
        Method::PUT,
        &format!("{REPO}/duplicates/{target}/undo"),
        who,
        json!({"event":event}),
    )
    .await
}

async fn grant(h: &Harness, alice: &str, access: &str) {
    let members = format!("{REPO}/members");
    let revision = h.json(&members, alice).await["revision"].clone();
    h.write(
        Method::PUT,
        &members,
        alice,
        json!({"expected_revision":revision,"groups":[],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":access}
        ]}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_writers_mark_duplicates_and_stale_undo_cannot_remove_a_new_mark() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in ["Reported regression", "Canonical bug", "More recent report"] {
        issue(&h, &alice, title).await;
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let request = Uuid::now_v7();
    post(&h, &bob, "issues/1", "Duplicate of #2", request).await;
    assert!(
        events(&h, &bob, "issues/1", "duplicate_marked")
            .await
            .is_empty()
    );
    grant(&h, &alice, "write").await;
    post(&h, &bob, "issues/1", "Duplicate of #2", request).await;
    assert!(
        events(&h, &bob, "issues/1", "duplicate_marked")
            .await
            .is_empty()
    );

    for comment in ["Duplicate of #1", "Duplicate of #88", "`Duplicate of #2`"] {
        post(&h, &bob, "issues/1", comment, Uuid::now_v7()).await;
    }
    let request = Uuid::now_v7();
    for id in [request, request, Uuid::now_v7()] {
        post(&h, &bob, "issues/1", "Duplicate of #2", id).await;
    }
    post(&h, &bob, "issues/2", "Duplicate of #1", Uuid::now_v7()).await;
    let marks = events(&h, &bob, "issues/1", "duplicate_marked").await;
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0]["action"]["title"], "Canonical bug");
    assert_eq!(marks[0]["action"]["subject_kind"], "issue");
    let id = marks[0]["number"].as_u64().unwrap();
    let canonical = events(&h, &alice, "issues/2", "duplicate_marked").await;
    assert_eq!(canonical.len(), 1);
    assert_eq!(canonical[0]["number"], id);
    let path = format!("{REPO}/issues/1/conversation");
    assert_eq!(h.json(&path, &alice).await["undo_duplicate_event"], id);
    grant(&h, &alice, "read").await;
    assert!(h.json(&path, &bob).await["undo_duplicate_event"].is_null());
    assert_eq!(
        undo(&h, &bob, "issue/1", id).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        h.json(&format!("{REPO}/issues/2/conversation"), &alice)
            .await["undo_duplicate_event"]
            .is_null()
    );
    for _ in 0..2 {
        undo(&h, &alice, "issue/1", id)
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(
        events(&h, &alice, "issues/1", "duplicate_unmarked")
            .await
            .len(),
        1
    );
    grant(&h, &alice, "write").await;
    post(&h, &bob, "issues/1", "Duplicate of #2", request).await;
    assert!(h.json(&path, &alice).await["undo_duplicate_event"].is_null());
    post(&h, &alice, "issues/1", "Duplicate of #3", Uuid::now_v7()).await;
    assert_eq!(
        undo(&h, &alice, "issue/1", id).await.status(),
        StatusCode::CONFLICT
    );
    let latest = events(&h, &alice, "issues/1", "duplicate_marked").await;
    assert_eq!(latest[0]["action"]["number"], 3);
    assert_eq!(
        h.json(&path, &alice).await["undo_duplicate_event"],
        latest[0]["number"]
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn combined_pull_comments_mark_only_pulls_and_roll_back_with_rejected_state() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    issue(&h, &alice, "Issue with the same number").await;
    issue(&h, &alice, "Wrong kind").await;
    let repo = h.server.repositories.values()[0].id;
    let cell = h.server.cells().unwrap().repository(repo).unwrap();
    for (branch, title, commit) in [
        ("first", "Duplicate change", "b"),
        ("second", "Canonical change", "c"),
    ] {
        let result = cell
            .client
            .command::<CreatePull>(
                &cell.target,
                app::mutation_identity().unwrap(),
                CreatePullInput {
                    code_owner_requests: Default::default(),
                    metadata: Default::default(),
                    draft: false,
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: RepositoryAuthor {
                        issuer: h.provider.issuer.clone(),
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                    },
                    title: title.into(),
                    body: String::new(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: format!("refs/heads/{branch}"),
                    head_repository: None,
                    base_oid: "a".repeat(40),
                    head_oid: commit.repeat(40),
                },
            )
            .await
            .unwrap();
        assert!(matches!(result.output, CreatePullOutcome::Created(_)));
    }
    let path = format!("{REPO}/pulls/1/state");
    let create = |version| json!({"request_id":Uuid::now_v7(),"version":version,"state":"closed","body":"Duplicate of #2"});
    assert_eq!(
        h.write(Method::POST, &path, &alice, create(5))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(
        events(&h, &alice, "pulls/1", "duplicate_marked")
            .await
            .is_empty()
    );
    h.write(Method::POST, &path, &alice, create(1))
        .await
        .error_for_status()
        .unwrap();
    let marked = events(&h, &alice, "pulls/1", "duplicate_marked").await;
    assert_eq!(marked[0]["action"]["title"], "Canonical change");
    assert_eq!(marked[0]["action"]["subject_kind"], "pull");
    undo(&h, &alice, "pull/1", marked[0]["number"].as_u64().unwrap())
        .await
        .error_for_status()
        .unwrap();
    post(&h, &alice, "pulls/1", "Duplicate of #2", Uuid::now_v7()).await;
    assert_eq!(
        events(&h, &alice, "pulls/1", "duplicate_marked")
            .await
            .len(),
        2
    );
    assert!(
        events(&h, &alice, "issues/2", "duplicate_marked")
            .await
            .is_empty()
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn embedded_browser_marks_and_undoes_a_duplicate_issue() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    issue(&h, &alice, "Reported regression").await;
    issue(&h, &alice, "Canonical bug").await;
    let browser = Browser::new("duplicate-issue");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "#new-comment"]).await;
    browser
        .run(&["fill", "#new-comment", "Duplicate of #2"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Comment", "--exact",
        ])
        .await;
    let remove = "[aria-label='Undo duplicate mark']";
    browser.run(&["wait", remove]).await;
    assert!(
        browser
            .run(&["snapshot"])
            .await
            .contains("#2 Canonical bug")
    );
    browser.run(&["click", remove]).await;
    browser
        .run(&["wait", "--text", "removed the duplicate link to"])
        .await;
    assert_eq!(
        events(&h, &alice, "issues/1", "duplicate_unmarked")
            .await
            .len(),
        1
    );
    assert!(
        h.json(&format!("{REPO}/issues/1/conversation"), &alice)
            .await["undo_duplicate_event"]
            .is_null()
    );
    browser.run(&["close"]).await;
    h.close().await;
}
