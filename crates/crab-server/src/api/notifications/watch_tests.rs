use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember, app,
    cells::repository::collaboration::watches,
    test_support::{git, http::Harness},
};

const ROOT: &str = "/api/repos/team/private";
const PREFERENCE: &str = "/api/repos/team/private/subscription";
const WATCHED: &str = "/api/notifications?reason=subscribed";

async fn set(h: &Harness, cookie: &str, mode: &str, version: u64) -> Value {
    h.write(
        Method::PUT,
        PREFERENCE,
        cookie,
        json!({"mode":mode,"version":version}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn issue(h: &Harness, cookie: &str, title: &str, assignee: Option<Value>) {
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        cookie,
        json!({"request_id":Uuid::now_v7(),"title":title,"body":"",
            "metadata":{"assignee_ids":assignee.into_iter().collect::<Vec<_>>()}}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test]
async fn watch_preferences_apply_to_future_activity_and_current_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(PREFERENCE, &bob).await,
        json!({"mode":"direct","version":0})
    );
    set(&h, &alice, "issues", 0).await;
    issue(&h, &alice, "Before watching", None).await;
    let watching = set(&h, &bob, "issues", 0).await;
    assert_eq!(watching, json!({"mode":"issues","version":1}));
    assert_eq!(set(&h, &bob, "issues", 0).await, watching);
    assert_eq!(
        h.write(
            Method::PUT,
            PREFERENCE,
            &bob,
            json!({"mode":"direct","version":0})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await["items"], json!([]));

    issue(&h, &alice, "Watched issue", None).await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), remote.as_str()).await;
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"A pull request","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    let watched = h.json(WATCHED, &bob).await;
    assert_eq!(watched["items"].as_array().unwrap().len(), 1);
    assert_eq!(watched["items"][0]["title"], "Watched issue");
    assert_eq!(h.json(WATCHED, &alice).await["items"], json!([]));

    set(&h, &bob, "issues_and_pull_requests", 1).await;
    // Issue and pull request 1 have independent preferences.
    h.write(
        Method::PUT,
        &format!("{ROOT}/issues/1/subscription"),
        &bob,
        json!({"mode":"unsubscribed","version":0}),
    )
    .await
    .error_for_status()
    .unwrap();

    h.write(
        Method::POST,
        &format!("{ROOT}/pulls/1/comments"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"Follow-up on the change."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    let both = h.json(WATCHED, &bob).await;
    assert_eq!(both["items"].as_array().unwrap().len(), 2);
    assert!(
        both["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-1")
    );

    set(&h, &bob, "muted", 2).await;
    issue(&h, &alice, "Assignment while muted", Some(bob_id)).await;
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json("/api/notifications", &bob).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );
    set(&h, &bob, "direct", 3).await;
    assert_eq!(h.json("/api/notifications", &bob).await, both);
    issue(&h, &alice, "After unwatching", None).await;
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await, both);

    set(&h, &bob, "issues", 4).await;
    issue(&h, &alice, "Before revocation", None).await;
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "private",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await["items"], json!([]));
    let blocked = h
        .http
        .get(format!("{}{PREFERENCE}", h.origin))
        .header("cookie", &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn watch_fanout_advances_over_ineligible_pages() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    set(&h, &bob, "issues", 0).await;
    let repo = h.server.repositories.values()[0].clone();
    let cell = app::repository_cell(&h.server, &repo).unwrap();
    // Existing preferences can outlive a repository grant. Place a full page
    // of higher UUIDs ahead of Bob without granting those accounts access.
    for offset in 0..32 {
        cell.client
            .command::<watches::Set>(
                &cell.target,
                app::mutation_identity().unwrap(),
                watches::Change {
                    actor: Uuid::from_u128(u128::MAX - offset),
                    desired: watches::Preference {
                        version: 0,
                        mode: watches::Mode::Issues,
                    },
                },
            )
            .await
            .unwrap();
    }
    issue(&h, &alice, "A paged notification", None).await;
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await["items"], json!([]));
    h.deliver_notifications("team", "private").await;
    let delivered = h.json(WATCHED, &bob).await;
    assert_eq!(delivered["items"].as_array().unwrap().len(), 1);
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await, delivered);
    h.close().await;
}

#[tokio::test]
async fn discussion_preferences_override_repository_watch_without_suppressing_mentions() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let first = format!("{ROOT}/issues/1");
    let second = format!("{ROOT}/issues/2");
    let preference = format!("{first}/subscription");
    issue(&h, &alice, "Followed discussion", None).await;
    issue(&h, &alice, "Sibling discussion", None).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob-watch","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"inherit","version":0})
    );

    let choose = |mode, version| {
        h.write(
            Method::PUT,
            &preference,
            &bob,
            json!({"mode":mode,"version":version}),
        )
    };
    let comment = |path: &str, body: &str| {
        let path = format!("{path}/comments");
        let body = json!({"request_id":Uuid::now_v7(),"body":body});
        let h = &h;
        let alice = &alice;
        async move {
            h.write(Method::POST, &path, alice, body)
                .await
                .error_for_status()
                .unwrap();
        }
    };

    for _ in 0..2 {
        let saved: Value = choose("follow", 0)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved, json!({"mode":"follow","version":1}));
    }
    assert_eq!(
        choose("state_changes", 0).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json(&preference, &alice).await,
        json!({"mode":"inherit","version":0})
    );
    comment(&first, "This update should arrive").await;
    comment(&second, "The other thread is not followed").await;
    h.deliver_notifications("team", "private").await;
    let followed = h.json(WATCHED, &bob).await;
    assert_eq!(followed["items"].as_array().unwrap().len(), 1);
    assert!(
        followed["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-1")
    );

    set(&h, &bob, "issues", 0).await;
    choose("state_changes", 1).await.error_for_status().unwrap();
    comment(&first, "This comment should be filtered").await;
    comment(&second, "The repository watch covers this comment").await;
    let version = h.json(&first, &alice).await["version"].clone();
    h.write(
        Method::POST,
        &format!("{first}/state"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"version":version,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    let watched = h.json(WATCHED, &bob).await;
    let notices = watched["items"].as_array().unwrap();
    assert_eq!(notices.len(), 3);
    assert_eq!(
        notices
            .iter()
            .filter(|item| item["title"] == "Followed discussion")
            .count(),
        2
    );

    choose("unsubscribed", 2).await.error_for_status().unwrap();
    comment(&first, "A direct question for @bob-watch.").await;
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await, watched);
    assert_eq!(
        h.json("/api/notifications?reason=mention", &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    choose("inherit", 3).await.error_for_status().unwrap();
    comment(&first, "Back to the repository setting").await;
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json(WATCHED, &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    h.close().await;
}
