use crate::test_support::http::Harness;
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const BULK: &str = "/api/notifications/bulk";
const INBOX: &str = "/api/notifications?unread=true";
const DONE: &str = "/api/notifications?view=done";

fn item(row: &Value) -> Value {
    json!({"id":row["id"],"repository_id":row["repository"]["id"],"expected":row["updated"]})
}

fn batch(items: Vec<Value>, action: Value) -> Value {
    json!({
        "request_id":Uuid::now_v7(),
        "issued_at_ms":crate::cells::unix_now_ms().unwrap(),
        "items":items,
        "action":action
    })
}

#[tokio::test]
async fn bulk_triage_is_atomic_across_repositories_and_replays_the_committed_intent() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();

    let session = h.json("/api/session", &alice).await;
    let csrf = session["csrf"].as_str().unwrap();
    let post_issue = async |name: &str, path: &str, body: Value| {
        let repo = h
            .server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap();
        h.http
            .post(format!("{}/api/repos/team/{name}/issues{path}", h.origin))
            .header("cookie", &alice)
            .header("origin", &h.origin)
            .header("x-csrf-token", csrf)
            .header("x-crab-repository-id", repo.id.to_string())
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        h.deliver_notifications("team", name).await;
    };
    for name in ["private", "library"] {
        post_issue(
            name,
            "",
            json!({"request_id":Uuid::now_v7(),"title":format!("{name} review"),"body":"Please check, @bob."}),
        ).await;
    }

    let page = h.json(INBOX, &bob).await;
    let rows = page["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let row = |name: &str| {
        rows.iter()
            .find(|row| row["repository"]["name"] == name)
            .unwrap()
    };
    let private = item(row("private"));
    let library = item(row("library"));
    let reply = async |name: &str, text: &str| {
        post_issue(
            name,
            "/1/comments",
            json!({"request_id":Uuid::now_v7(),"body":text}),
        )
        .await;
    };
    reply("library", "@bob, here's a newer update.").await;
    let stale = batch(vec![private.clone(), library.clone()], json!({"done":true}));
    assert_eq!(
        h.write(Method::POST, BULK, &bob, stale).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(h.json(DONE, &bob).await["items"], json!([]));
    assert_eq!(
        h.json(INBOX, &bob).await["items"].as_array().unwrap().len(),
        2
    );

    let duplicate = batch(vec![private.clone(), private.clone()], json!({"save":true}));
    assert_eq!(
        h.write(Method::POST, BULK, &bob, duplicate).await.status(),
        StatusCode::BAD_REQUEST
    );
    let fresh = h.json(INBOX, &bob).await;
    let library = item(
        fresh["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["id"] == row("library")["id"])
            .unwrap(),
    );
    let complete = batch(vec![private.clone(), library.clone()], json!({"done":true}));
    assert_eq!(
        h.http
            .post(format!("{}{BULK}", h.origin))
            .header("cookie", &bob)
            .header("origin", &h.origin)
            .json(&complete)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let result: Value = h
        .write(Method::POST, BULK, &bob, complete.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"items":[
            {"id":private["id"],"read":true,"saved":false,"done":true},
            {"id":library["id"],"read":true,"saved":false,"done":true}
        ]})
    );
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );

    reply("private", "@bob, follow-up after completion.").await;
    let replay: Value = h
        .write(Method::POST, BULK, &bob, complete.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, result);
    let mut different = complete;
    different["action"] = json!({"save":true});
    assert_eq!(
        h.write(Method::POST, BULK, &bob, different).await.status(),
        StatusCode::CONFLICT
    );
    let new_inbox = h.json(INBOX, &bob).await;
    assert_eq!(new_inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(new_inbox["items"][0]["id"], private["id"]);
    assert_ne!(new_inbox["items"][0]["updated"], private["expected"]);

    let repo = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let mut members = repo.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "library", members, true)
        .await
        .unwrap();
    let revoked = batch(
        vec![item(&new_inbox["items"][0]), library],
        json!({"done":true}),
    );
    assert_eq!(
        h.write(Method::POST, BULK, &bob, revoked).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.json(INBOX, &bob).await, new_inbox);
    h.close().await;
}
