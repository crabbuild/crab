use crate::{
    app,
    cells::{
        Address,
        directory::notifications::{Access, Deliver, Delivery, Notice, Reason, Recipient},
        repository::collaboration::Target,
    },
    test_support::http::Harness,
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const ROOT: &str = "/api/repos/team/private";
const INBOX: &str = "/api/notifications";
const UNSUBSCRIBE: &str = "/api/notifications/unsubscribe";
const FIRST_PREFERENCE: &str = "/api/repos/team/private/issues/1/subscription";

fn selection(row: &Value) -> Value {
    json!({
        "id":row["id"],
        "repository_id":row["repository"]["id"],
        "subject":row["subject"],
        "expected":row["updated"]
    })
}

#[tokio::test]
async fn inbox_unsubscribe_preserves_bookmarks_and_only_reopens_for_new_direct_activity() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let actor = serde_json::from_value::<Uuid>(profile["id"].clone()).unwrap();

    for title in ["Saved issue", "Another issue"] {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":"Hi @bob."}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.deliver_notifications("team", "private").await;
    let page = h.json(INBOX, &bob).await;
    let rows = page["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let first = rows
        .iter()
        .find(|row| row["subject"]["number"] == 1)
        .unwrap();
    let second = rows
        .iter()
        .find(|row| row["subject"]["number"] == 2)
        .unwrap();
    let mark = format!("{INBOX}/{}/state", first["id"].as_str().unwrap());
    h.write(
        Method::PUT, &mark, &bob,
        json!({"repository_id":first["repository"]["id"],"expected":first["updated"],"action":{"save":true}}),
    ).await.error_for_status().unwrap();
    let reply = async |issue: u32, body: &str| {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues/{issue}/comments"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"body":body}),
        )
        .await
        .error_for_status()
        .unwrap();
        h.deliver_notifications("team", "private").await;
    };
    reply(2, "A newer update for the other issue").await;

    let choice = json!({"items":[selection(first),selection(second)]});
    assert_eq!(
        h.http
            .post(format!("{}{UNSUBSCRIBE}", h.origin))
            .header("cookie", &bob)
            .header("origin", &h.origin)
            .json(&choice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let outcome: Value = h
        .write(Method::POST, UNSUBSCRIBE, &bob, choice.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        outcome,
        json!({"items":[
            {"id":first["id"],"status":"unsubscribed"},
            {"id":second["id"],"status":"changed"}
        ]})
    );
    assert_eq!(h.json(FIRST_PREFERENCE, &bob).await["mode"], "unsubscribed");
    assert_eq!(h.json(INBOX, &bob).await["items"][0]["id"], second["id"]);
    assert_eq!(
        h.json("/api/notifications?view=saved", &bob).await["items"][0]["id"],
        first["id"]
    );
    let second = h.json(INBOX, &bob).await["items"][0].clone();
    h.write(
        Method::POST,
        UNSUBSCRIBE,
        &bob,
        json!({"items":[selection(&second)]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );
    reply(1, "An ordinary comment after unsubscribing.").await;

    let inbox = h.server.cells().unwrap();
    let repo = h.server.repositories.values()[0].clone();
    // Simulate a watch page already in flight when the preference changed.
    // Its carried display reason remains mention, but it cannot reopen.
    inbox
        .command::<Deliver>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            Delivery {
                notice: Notice {
                    id: Uuid::now_v7(),
                    repository: repo.id,
                    target: Target::IssueComment {
                        issue: 1,
                        number: 1,
                    },
                    created_at_ms: first["updated"]["created_at_ms"].as_u64().unwrap(),
                    sender: "Alice".into(),
                },
                recipients: vec![Recipient {
                    actor,
                    access: Access::Direct,
                    reason: Reason::Subscribed,
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );

    h.write(
        Method::PATCH,
        &format!("{ROOT}/issues/1"),
        &alice,
        json!({"version":1,"assignee_ids":[actor]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(FIRST_PREFERENCE, &bob).await["mode"], "unsubscribed");
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));
    reply(1, "Another direct mention, @bob.").await;
    let reopened = h.json(INBOX, &bob).await;
    assert_eq!(reopened["items"].as_array().unwrap().len(), 1);
    assert_eq!(reopened["items"][0]["id"], first["id"]);
    assert_eq!(reopened["items"][0]["read"], false);
    assert_eq!(reopened["items"][0]["saved"], true);
    assert_eq!(h.json(FIRST_PREFERENCE, &bob).await["mode"], "follow");

    let replay: Value = h
        .write(Method::POST, UNSUBSCRIBE, &bob, choice)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["items"][0]["status"], "unsubscribed");
    assert_eq!(h.json(INBOX, &bob).await, reopened);
    assert_eq!(h.json(FIRST_PREFERENCE, &bob).await["mode"], "follow");
    let mut members = repo.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    let revoked: Value = h
        .write(
            Method::POST,
            UNSUBSCRIBE,
            &bob,
            json!({"items":[selection(&reopened["items"][0])]}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(revoked["items"][0]["status"], "unavailable");
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));
    h.close().await;
}
