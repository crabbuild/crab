use super::*;
use crate::{
    cells::directory::notifications::{Access, Deliver, Delivery, MAX_HISTORY, Notice, Recipient},
    test_support::http::Harness,
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

const INBOX: &str = "/api/notifications";
const SAVED: &str = "/api/notifications?view=saved";
const DONE: &str = "/api/notifications?view=done";
const UNREAD: &str = "/api/notifications/unread";

#[tokio::test]
async fn saved_threads_survive_history_eviction_and_done_tracks_the_seen_activity() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in ["Pinned conversation", "Another issue"] {
        h.write(
            Method::POST,
            "/api/repos/team/private/issues",
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
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
    let actor: Uuid = serde_json::from_value(profile["id"].clone()).unwrap();
    let repo = h.server.repositories.values()[0].clone();
    let deliver = |notice: Notice, access, reason| {
        let server = &h.server;
        async move {
            server
                .cells()
                .unwrap()
                .command::<Deliver>(
                    Address::Directory,
                    app::mutation_identity().unwrap(),
                    Delivery {
                        notice,
                        recipients: vec![Recipient {
                            actor,
                            access,
                            reason,
                        }],
                    },
                )
                .await
                .unwrap();
        }
    };
    let first = Notice {
        id: Uuid::now_v7(),
        repository: repo.id,
        target: Target::Issue { number: 1 },
        created_at_ms: 1,
        sender: "Alice".into(),
    };
    let second = Notice {
        id: Uuid::now_v7(),
        target: Target::Issue { number: 2 },
        created_at_ms: 2,
        ..first.clone()
    };
    deliver(first.clone(), Access::Direct, Reason::Mention).await;
    deliver(second.clone(), Access::Direct, Reason::Mention).await;
    let path = format!("{INBOX}/{}/state", first.thread_id());
    let second_path = format!("{INBOX}/{}/state", second.thread_id());
    let write = |expected: Position, action: Value| json!({"repository_id":repo.id,"expected":expected,"action":action});
    let save = write(first.position(), json!({"save":true}));
    for _ in 0..2 {
        let result: Value = h
            .write(Method::PUT, &path, &bob, save.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result, json!({"read":false,"saved":true,"done":false}));
    }
    let result = h
        .write(
            Method::PUT,
            &path,
            &alice,
            write(first.position(), json!({"done":true})),
        )
        .await;
    assert_eq!(result.status(), StatusCode::CONFLICT);
    h.write(
        Method::PUT,
        &second_path,
        &bob,
        write(second.position(), json!({"done":true})),
    )
    .await
    .error_for_status()
    .unwrap();

    // Fill the rolling history and cross an entire private archive page.
    // Synthetic slots have no source title and cannot become public cursors.
    for number in 3..=(MAX_HISTORY as u64 + 3) {
        let notice = Notice {
            id: Uuid::now_v7(),
            target: Target::Issue { number },
            created_at_ms: number,
            ..first.clone()
        };
        deliver(notice.clone(), Access::Direct, Reason::Mention).await;
        if number <= 55 {
            let path = format!("{INBOX}/{}/state", notice.thread_id());
            h.write(
                Method::PUT,
                &path,
                &bob,
                write(notice.position(), json!({"done":true})),
            )
            .await
            .error_for_status()
            .unwrap();
        }
    }
    let inbox = h.json(INBOX, &bob).await;
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["items"][0]["id"], json!(first.thread_id()));
    assert_eq!(h.json(SAVED, &bob).await, inbox);
    let done = h.json(DONE, &bob).await;
    assert_eq!(done["items"].as_array().unwrap().len(), 1);
    assert_eq!(done["items"][0]["id"], json!(second.thread_id()));

    let result: Value = h
        .write(
            Method::PUT,
            &second_path,
            &bob,
            write(second.position(), json!({"done":false})),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result, json!({"read":true,"saved":false,"done":false}));
    assert_eq!(
        h.json(INBOX, &bob).await["items"][0]["id"],
        json!(second.thread_id())
    );
    assert_eq!(h.json(DONE, &bob).await["items"], json!([]));

    let complete = write(first.position(), json!({"done":true}));
    h.write(Method::PUT, &path, &bob, complete.clone())
        .await
        .error_for_status()
        .unwrap();
    deliver(first.clone(), Access::Group, Reason::Mention).await;
    let saved = h.json(SAVED, &bob).await;
    assert_eq!(saved["items"][0]["done"], true);
    assert_eq!(saved["items"][0]["read"], true);
    assert_eq!(h.json(DONE, &bob).await, saved);
    assert_eq!(
        h.json(INBOX, &bob).await["items"].as_array().unwrap().len(),
        1
    );

    let latest = Notice {
        id: Uuid::now_v7(),
        created_at_ms: MAX_HISTORY as u64 + 4,
        target: Target::IssueComment {
            issue: 1,
            number: 7,
        },
        ..first.clone()
    };
    deliver(latest.clone(), Access::Group, Reason::Subscribed).await;
    let saved = h.json(SAVED, &bob).await;
    let row = &saved["items"][0];
    assert_eq!(row["updated"], json!(latest.position()));
    assert_eq!(row["saved"], true);
    assert_eq!(row["reason"], "mention");
    assert_eq!(row["read"], false);
    assert_eq!(row["done"], false);
    assert!(row["href"].as_str().unwrap().ends_with("#issuecomment-7"));
    assert_eq!(h.json(DONE, &bob).await["items"], json!([]));
    assert_eq!(
        h.write(Method::PUT, &path, &bob, complete).await.status(),
        StatusCode::CONFLICT
    );

    for (query, expected) in [(SAVED, first.thread_id()), (INBOX, first.thread_id())] {
        let separator = if query.contains('?') { '&' } else { '?' };
        let limited = format!("{query}{separator}limit=1");
        let page = h.json(&limited, &bob).await;
        assert_eq!(page["items"][0]["id"], json!(expected));
        if query == INBOX {
            let next = page["next"].as_str().unwrap();
            let next = h.json(&format!("{limited}&before={next}"), &bob).await;
            assert_eq!(next["items"][0]["id"], json!(second.thread_id()));
            assert!(next["next"].is_null());
        }
    }
    h.write(
        Method::PUT,
        &path,
        &bob,
        write(latest.position(), json!({"done":true})),
    )
    .await
    .error_for_status()
    .unwrap();
    let mut members = repo.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    for query in [INBOX, SAVED, DONE] {
        assert_eq!(h.json(query, &bob).await, json!({"items":[],"next":null}));
    }
    assert_eq!(h.json(UNREAD, &bob).await, json!({"unread":false}));
    assert_eq!(
        h.write(
            Method::PUT,
            &path,
            &bob,
            write(latest.position(), json!({"save":false})),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_saves_completes_and_restores_an_inbox_conversation() {
    use crate::test_support::http::browser::Browser;

    let h = Harness::new(false).await;
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

    let title = "Review notification triage";
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":title,"body":"@bob, please review."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;

    let browser = Browser::new("inbox-triage");
    browser
        .run(&["open", &format!("{}/-/notifications", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    click("button", "Continue to sign in").await;
    browser.run(&["wait", "--text", title]).await;
    click("button", "Save").await;
    browser.run(&["wait", "--text", "Unsave"]).await;
    let saved = h.json(SAVED, &bob).await;
    assert_eq!(saved["items"][0]["title"], title);
    click("button", "Mark as done").await;
    browser
        .run(&["wait", "--text", "No unread notifications."])
        .await;
    assert_eq!(h.json(UNREAD, &bob).await, json!({"unread":false}));
    assert_eq!(
        h.json(DONE, &bob).await["items"][0]["id"],
        saved["items"][0]["id"]
    );

    click("link", "Done").await;
    browser.run(&["reload"]).await;
    click("button", "Move to inbox").await;
    browser
        .run(&["wait", "--text", "No completed notifications."])
        .await;
    click("link", "All").await;
    browser.run(&["wait", "--text", title]).await;
    assert_eq!(h.json(INBOX, &bob).await["items"][0]["saved"], true);
    assert_eq!(h.json(DONE, &bob).await["items"], json!([]));
    click("button", "Unsubscribe").await;
    browser
        .run(&["wait", "--text", "No notifications yet."])
        .await;
    assert_eq!(h.json(SAVED, &bob).await["items"][0]["title"], title);
    h.write(
        Method::POST,
        "/api/repos/team/private/issues/1/comments",
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"@bob, re-opening the conversation."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;

    let second_title = "Review a second notification";
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":second_title,"body":"@bob, one more."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    browser.run(&["reload"]).await;
    browser.run(&["wait", "--text", second_title]).await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Select all loaded notifications",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "select",
            "select[aria-label='Bulk notification action']",
            "done",
        ])
        .await;
    click("button", "Apply to selected").await;
    browser
        .run(&["wait", "--text", "No notifications yet."])
        .await;
    assert_eq!(
        h.json(DONE, &bob).await["items"].as_array().unwrap().len(),
        2
    );
    drop(browser);
    h.close().await;
}
