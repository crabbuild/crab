use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

const ROOT: &str = "/api/repos/team/private";
const INITIAL: &str = "Original comment text";
const CURRENT: &str = "Updated comment text";

async fn grant(h: &Harness, owner: &str, access: &str) {
    let path = format!("{ROOT}/members");
    let current = h.json(&path, owner).await;
    h.write(
        Method::PUT,
        &path,
        owner,
        json!({"expected_revision":current["revision"],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":access}
        ],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn issue() -> (Harness, String, String) {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Track edits","body":"Issue description"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (h, alice, bob)
}

async fn comment(h: &Harness, author: &str, body: &str) {
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/1/comments"),
        author,
        json!({"request_id":Uuid::now_v7(),"body":body}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn patch(h: &Harness, cookie: &str, address: &str, input: Value) -> Value {
    h.write(Method::PATCH, &format!("{ROOT}/{address}"), cookie, input)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn history(address: &str) -> String {
    format!("{ROOT}/edit-history/{address}")
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_history_preserves_attribution_and_redaction_hides_adjacent_comparison_text() {
    let (h, alice, bob) = issue().await;
    let address = "issues/1/comments/1";
    let path = history(address);
    comment(&h, &alice, INITIAL).await;
    assert_eq!(h.json(&path, &bob).await["items"], json!([]));
    grant(&h, &alice, "moderate").await;

    for (version, body) in [(1, "Intermediate version"), (2, CURRENT), (3, CURRENT)] {
        let edited = patch(&h, &bob, address, json!({"version":version,"body":body})).await;
        assert_eq!(edited["has_edit_history"], true);
    }
    let page = h.json(&path, &bob).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["editor"]["name"], "Bob");
    assert!(!page.to_string().contains("bob-id"));
    let latest = format!("{path}?revision={}", items[0]["number"]);
    let earlier = format!("{path}?revision={}", items[1]["number"]);
    let detail = h.json(&latest, &bob).await;
    assert_eq!(detail["before"]["body"], "Intermediate version");
    assert_eq!(detail["after"]["body"], CURRENT);
    assert_eq!(detail["can_redact"], false);
    let first = h.json(&earlier, &bob).await;
    assert_eq!(first["before"]["body"], INITIAL);
    assert_eq!(first["after"]["body"], "Intermediate version");

    assert_eq!(
        h.write(Method::DELETE, &latest, &bob, json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    grant(&h, &alice, "write").await;
    for _ in 0..2 {
        h.write(Method::DELETE, &latest, &bob, json!({}))
            .await
            .error_for_status()
            .unwrap();
    }
    let redacted = h.json(&latest, &alice).await;
    assert!(redacted["before"].is_null());
    assert!(redacted["after"].is_null());
    assert_eq!(redacted["revision"]["redaction"]["author"]["name"], "Bob");
    assert!(!redacted.to_string().contains("Intermediate version"));
    assert!(h.json(&earlier, &alice).await["after"].is_null());
    let current = h.json(&format!("{ROOT}/{address}"), &bob).await;
    assert_eq!(current["body"], CURRENT);
    assert_eq!(current["author"]["name"], "Alice");

    patch(
        &h,
        &bob,
        "issues/1",
        json!({"version":1,"body":"Revised issue description"}),
    )
    .await;
    let root = history("issues/1");
    let revision = h.json(&root, &alice).await["items"][0]["number"]
        .as_u64()
        .unwrap();
    assert_eq!(
        h.json(&format!("{root}?revision={revision}"), &alice).await["before"]["body"],
        "Issue description"
    );
    h.close().await;
}

async fn create_pull(h: &Harness, author: &str) -> (Value, String) {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            author,
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
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    let pull: Value = h.write(
        Method::POST, &format!("{ROOT}/pulls"), author,
        json!({"request_id":Uuid::now_v7(),"title":"Track review edits","body":"Original pull description",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    (pull, head)
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_history_records_description_reviews_comments_inline_suggestions_and_replies() {
    let (h, alice, bob) = issue().await;
    let (pull, head) = create_pull(&h, &alice).await;
    for (route, input) in [
        (
            "pulls/1/comments",
            json!({"request_id":Uuid::now_v7(),"body":"PR comment"}),
        ),
        (
            "pulls/1/reviews",
            json!({"request_id":Uuid::now_v7(),"body":"Review body","state":"commented","commit_oid":head}),
        ),
        (
            "pulls/1/threads",
            json!({
                "request_id":Uuid::now_v7(),"body":"Inline comment","side":"new","start_line":2,"end_line":2,
                "path_hex":"524541444d452e6d64","base_oid":pull["base_oid"],"head_oid":head,
                "suggested_text":"First replacement"
            }),
        ),
        (
            "pulls/1/threads/1/replies",
            json!({"request_id":Uuid::now_v7(),"body":"Inline reply"}),
        ),
    ] {
        h.write(Method::POST, &format!("{ROOT}/{route}"), &bob, input)
            .await
            .error_for_status()
            .unwrap();
    }
    let pull_version = h.json(&format!("{ROOT}/pulls/1"), &alice).await["version"]
        .as_u64()
        .unwrap();
    for (address, member, version, before, after) in [
        (
            "pulls/1",
            &alice,
            pull_version,
            "Original pull description",
            "Revised pull description",
        ),
        (
            "pulls/1/comments/1",
            &bob,
            1,
            "PR comment",
            "Edited PR comment",
        ),
        (
            "pulls/1/reviews/1",
            &bob,
            1,
            "Review body",
            "Edited review body",
        ),
        (
            "pulls/1/threads/1/replies/1",
            &bob,
            1,
            "Inline reply",
            "Edited inline reply",
        ),
    ] {
        let edited = patch(&h, member, address, json!({"version":version,"body":after})).await;
        assert_eq!(edited["has_edit_history"], true, "{address}");
        let path = history(address);
        let revision = h.json(&path, &bob).await["items"][0]["number"]
            .as_u64()
            .unwrap();
        let detail = h.json(&format!("{path}?revision={revision}"), &bob).await;
        assert_eq!(detail["before"]["body"], before, "{address}");
        assert_eq!(detail["after"]["body"], after, "{address}");
    }
    let thread = "pulls/1/threads/1";
    let edited = patch(
        &h,
        &bob,
        thread,
        json!({"version":1,"suggested_text":"Second replacement"}),
    )
    .await;
    assert_eq!(edited["has_edit_history"], true);
    let path = history(thread);
    let revision = h.json(&path, &alice).await["items"][0]["number"]
        .as_u64()
        .unwrap();
    let detail = h.json(&format!("{path}?revision={revision}"), &alice).await;
    assert_eq!(detail["before"]["body"], "Inline comment");
    assert_eq!(detail["before"]["suggestion"], "First replacement");
    assert_eq!(detail["after"]["suggestion"], "Second replacement");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_preserves_original_and_bounds_large_escaped_revisions() {
    use crate::cells::repository::{
        CommentKey, RepositoryAuthor, UpdateComment, UpdateCommentInput,
    };
    let (h, alice, bob) = issue().await;
    let initial = "\\".repeat(65_536);
    // HTTP JSON has a smaller envelope than a fully escaped maximum-size body.
    // Seed the canonical native comment writer directly so the storage limit is exercised.
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    let author = RepositoryAuthor {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
        name: "Alice".into(),
    };
    cell.client
        .command::<crate::cells::repository::CreateComment>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            crate::cells::repository::CreateCommentInput {
                can_mark_duplicate: false,
                can_post_locked: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                issue: 1,
                author: author.clone(),
                body: initial.clone(),
            },
        )
        .await
        .unwrap();
    for version in 1..=102 {
        cell.client
            .command::<UpdateComment>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                UpdateCommentInput {
                    key: CommentKey {
                        issue: 1,
                        number: 1,
                    },
                    actor: author.clone(),
                    can_moderate: true,
                    version,
                    body: format!("Edit {version}"),
                },
            )
            .await
            .unwrap();
    }
    let path = history("issues/1/comments/1");
    let mut numbers = Vec::new();
    let mut page = h.json(&path, &bob).await;
    loop {
        numbers.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["number"].as_u64().unwrap()),
        );
        let Some(next) = page["next"].as_u64() else {
            break;
        };
        assert!(numbers.len() < 100);
        page = h.json(&format!("{path}?before={next}"), &alice).await;
    }
    assert_eq!(numbers.len(), 100);
    assert!(numbers.windows(2).all(|pair| pair[0] > pair[1]));
    let original = numbers.last().unwrap();
    let detail = h.json(&format!("{path}?revision={original}"), &bob).await;
    assert_eq!(detail["before"]["body"], initial);
    assert!(detail["after"].is_null());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn embedded_browser_compares_and_removes_an_authored_comment_revision() {
    let (h, alice, bob) = issue().await;
    grant(&h, &alice, "read").await;
    comment(&h, &bob, INITIAL).await;
    let browser = Browser::new("edit-history");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    browser.sign_in().await;
    let click = async |name: &str| {
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["wait", "--text", INITIAL]).await;
    click("Edit comment").await;
    browser.run(&["fill", "#edit-comment-1", CURRENT]).await;
    click("Save comment").await;
    browser.run(&["wait", "--text", CURRENT]).await;
    click("View edit history").await;
    browser.run(&["wait", "--text", "Before this edit"]).await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(snapshot.contains(INITIAL));
    assert!(snapshot.contains(CURRENT));
    click("Remove revision").await;
    click("Confirm removal").await;
    browser
        .run(&["wait", "--text", "Revision content removed"])
        .await;
    assert!(!browser.run(&["snapshot"]).await.contains(INITIAL));
    let history = history("issues/1/comments/1");
    assert_eq!(
        h.json(&history, &bob).await["items"][0]["redaction"]["author"]["name"],
        "Bob"
    );
    browser.run(&["close"]).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn embedded_browser_edits_pull_content_and_submitted_review_without_losing_newer_fields() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (pull, head) = create_pull(&h, &alice).await;
    let review_path = format!("{ROOT}/pulls/1/reviews/1");
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls/1/reviews"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"First review summary","state":"commented","commit_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();

    let browser = Browser::new("pull-edits");
    let page = format!("{}/team/private/pull/1", h.origin);
    browser.run(&["open", &page]).await;
    browser.sign_in().await;
    browser
        .run(&["wait", "--text", "Original pull description"])
        .await;
    let click = async |name: &str| {
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    click("Edit pull request").await;
    browser
        .run(&["fill", "#edit-pull-description-title", "Updated pull title"])
        .await;
    let latest = patch(
        &h,
        &alice,
        "pulls/1",
        json!({"version":pull["version"],"body":"Description from another tab"}),
    )
    .await;
    click("Save changes").await;
    browser
        .run(&["wait", "--text", "Newer content was saved"])
        .await;
    browser
        .run(&["wait", "--text", "Continue with my draft"])
        .await;
    click("Continue with my draft").await;
    click("Save changes").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser.run(&["wait", "--text", "Updated pull title"]).await;
    browser
        .run(&["wait", "--text", "Description from another tab"])
        .await;
    let saved = h.json(&format!("{ROOT}/pulls/1"), &alice).await;
    assert_eq!(saved["title"], "Updated pull title");
    assert_eq!(saved["body"], latest["body"]);
    let root_history = h.json(&history("pulls/1"), &alice).await;
    assert_eq!(root_history["items"].as_array().unwrap().len(), 1);

    click("Edit pull request").await;
    browser
        .run(&["fill", "#edit-pull-description", "Revised pull description"])
        .await;
    click("Save changes").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser
        .run(&["wait", "--text", "Revised pull description"])
        .await;
    let saved = h.json(&format!("{ROOT}/pulls/1"), &alice).await;
    assert_eq!(saved["title"], "Updated pull title");
    assert_eq!(saved["body"], "Revised pull description");
    assert_eq!(saved["has_edit_history"], true);

    browser
        .run(&["wait", "--text", "First review summary"])
        .await;
    click("Edit review summary").await;
    browser
        .run(&["fill", "#edit-review-1", "Revised review summary"])
        .await;
    click("Save changes").await;
    browser
        .run(&["wait", "--text", "Revised review summary"])
        .await;
    let review = h.json(&review_path, &alice).await;
    assert_eq!(review["body"], "Revised review summary");
    assert_eq!(review["state"], "commented");
    assert_eq!(review["commit_oid"], head);
    assert_eq!(review["has_edit_history"], true);
    browser
        .run(&["open", &format!("{page}#pullrequestreview-1")])
        .await;
    browser
        .run(&["wait", "--text", "Revised review summary"])
        .await;
    assert!(
        browser
            .run(&["snapshot"])
            .await
            .contains("Review #1 permalink")
    );
    browser.run(&["close"]).await;
    h.close().await;
}
