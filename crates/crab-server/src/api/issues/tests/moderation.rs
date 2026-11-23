use super::{Harness, ROOT, StatusCode, Value, input, json, nonce, write};
use crate::test_support::http::browser::Browser;
use reqwest::Method;

const REPOSITORY: &str = "/api/repos/team/private";
const ISSUE: &str = "/api/repos/team/private/issues/1";
const COMMENT: &str = "/api/repos/team/private/issues/1/comments/1";
const LABELS: &str = "/api/repos/team/private/labels";
const MILESTONES: &str = "/api/repos/team/private/milestones";

async fn grant(h: &Harness, alice: &str, access: &str) {
    let path = format!("{REPOSITORY}/members");
    let current = h.json(&path, alice).await;
    let members = json!([
        {"subject":"alice-id","name":"Alice","access":"admin"},
        {"subject":"bob-id","name":"Bob","access":access},
    ]);
    h.write(
        Method::PUT,
        &path,
        alice,
        json!({
            "expected_revision":current["revision"],"members":members,"groups":[]
        }),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn discussion() -> (Harness, String, String) {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    assert_eq!(
        write(&h, &alice, "POST", ROOT, input(1)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        write(
            &h,
            &alice,
            "POST",
            &format!("{ISSUE}/comments"),
            json!({"request_id":nonce(2),"body":"Original comment"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (h, alice, bob)
}

#[tokio::test(flavor = "multi_thread")]
async fn moderator_edits_issue_content_and_metadata_without_code_or_label_deletion_access() {
    let (h, alice, bob) = discussion().await;
    let issue_change =
        json!({"version":1,"title":"Corrected issue","body":"Updated issue details"});
    let comment_change = json!({"version":1,"body":"Normalized comment"});
    let label = json!({"request_id":nonce(3),"name":"needs-review","color":"ffee00"});
    let milestone = json!({"request_id":nonce(4),"title":"Release planning"});

    grant(&h, &alice, "triage").await;
    for (method, path, body) in [
        ("PATCH", ISSUE, &issue_change),
        ("PATCH", COMMENT, &comment_change),
        ("POST", LABELS, &label),
        ("POST", MILESTONES, &milestone),
    ] {
        assert_eq!(
            write(&h, &bob, method, path, body.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(h.json(ISSUE, &bob).await["can_edit"], false);

    grant(&h, &alice, "moderate").await;
    assert_eq!(h.json(ISSUE, &bob).await["can_edit"], true);
    let timeline = format!("{ISSUE}/conversation");
    let conversation = h.json(&timeline, &bob).await;
    let entries = conversation["items"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["kind"] == "comment" && entry["value"]["can_edit"] == true)
    );
    for (path, body) in [(ISSUE, issue_change), (COMMENT, comment_change)] {
        let (status, changed) = write(&h, &bob, "PATCH", path, body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(changed["author"]["name"], "Alice");
        assert_eq!(changed["version"], 2);
        assert_eq!(
            write(&h, &bob, "PATCH", path, body).await.0,
            StatusCode::CONFLICT
        );
    }
    let activity = h.json(&format!("{ISSUE}/activity"), &bob).await;
    let edited: Vec<_> = activity["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["action"]["kind"] == "updated")
        .collect();
    assert_eq!(edited.len(), 2);
    assert!(edited.iter().all(|event| event["actor"]["name"] == "Bob"));
    assert_eq!(
        h.json(&format!("{ROOT}?q=in:comments%20Normalized"), &bob)
            .await["items"][0]["number"],
        1
    );

    let (status, created) = write(&h, &bob, "POST", LABELS, label).await;
    assert_eq!(status, StatusCode::CREATED);
    let label_path = format!("{LABELS}/{}", created["id"]);
    assert_eq!(
        write(
            &h,
            &bob,
            "PATCH",
            &label_path,
            json!({"version":1,"name":"reviewed","color":"228811"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        write(&h, &bob, "DELETE", &label_path, json!({"version":2}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let catalog = h.json(LABELS, &bob).await;
    assert_eq!(catalog["can_manage"], true);
    assert_eq!(catalog["can_delete"], false);

    let (status, created) = write(&h, &bob, "POST", MILESTONES, milestone).await;
    assert_eq!(status, StatusCode::CREATED);
    let milestone_path = format!("{MILESTONES}/{}", created["number"]);
    assert_eq!(
        write(
            &h,
            &bob,
            "PATCH",
            &milestone_path,
            json!({"version":1,"title":"Release ready","description":"","due_on":null})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(h.json(&milestone_path, &bob).await["can_manage"], true);
    let denied = h
        .write(
            Method::POST,
            "/api/git-token",
            &bob,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await;
    assert!(denied.status().is_client_error());

    grant(&h, &alice, "triage").await;
    assert_eq!(h.json(ISSUE, &bob).await["can_edit"], false);
    assert_eq!(
        write(
            &h,
            &bob,
            "PATCH",
            COMMENT,
            json!({"version":2,"body":"After revocation"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn native_browser_moderator_edits_another_authors_issue_and_comment() {
    let (h, alice, bob) = discussion().await;
    grant(&h, &alice, "moderate").await;
    let browser = Browser::new("repository-moderator");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    click("Edit issue").await;
    browser
        .run(&["fill", "#edit-issue-title", "Moderated in the browser"])
        .await;
    click("Save changes").await;
    browser
        .run(&["wait", "--text", "Moderated in the browser"])
        .await;
    click("Edit comment").await;
    browser
        .run(&[
            "find",
            "label",
            "Edit comment",
            "fill",
            "Corrected browser comment",
            "--exact",
        ])
        .await;
    click("Save comment").await;
    browser
        .run(&["wait", "--text", "Corrected browser comment"])
        .await;
    let issue = h.json(ISSUE, &bob).await;
    assert_eq!(issue["title"], "Moderated in the browser");
    assert_eq!(issue["author"]["name"], "Alice");
    let comment: Value = h.json(COMMENT, &bob).await;
    assert_eq!(comment["body"], "Corrected browser comment");
    assert_eq!(comment["author"]["name"], "Alice");
    browser.run(&["close"]).await;
    h.close().await;
}
