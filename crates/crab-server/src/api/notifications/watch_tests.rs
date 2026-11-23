use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember, app,
    cells::{
        Address,
        directory::{
            IdentityKey,
            notifications::{Access, Deliver, Delivery, Notice, Reason, Recipient},
        },
        repository::collaboration::{mentions::Pending, watches},
    },
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
    let session = h.json("/api/session", &bob).await;
    let identity = IdentityKey {
        issuer: session["user"]["issuer"].as_str().unwrap().into(),
        subject: session["user"]["subject"].as_str().unwrap().into(),
    };
    let bob_key = watches::identity_key(&identity).unwrap();
    // Existing preferences can outlive a grant. Place a full page of higher
    // qualified keys ahead of Bob without granting those accounts access.
    let candidates = (0..)
        .map(|offset| IdentityKey {
            issuer: identity.issuer.clone(),
            subject: format!("revoked-{offset}"),
        })
        .filter(|actor| watches::identity_key(actor).unwrap() > bob_key)
        .take(32);
    for actor in candidates {
        cell.client
            .command::<watches::Set>(
                &cell.target,
                app::mutation_identity().unwrap(),
                watches::Change {
                    actor,
                    desired: watches::Preference {
                        version: 0,
                        mode: watches::Mode::Issues,
                        releases: false,
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
async fn discussion_preferences_override_repository_watch_and_mentions_restore_following() {
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
        json!({"mode":"follow","version":1})
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
    assert_eq!(notices.len(), 2);
    let first_thread = &followed["items"][0];
    let closed = notices
        .iter()
        .find(|item| item["title"] == "Followed discussion")
        .unwrap();
    assert_eq!(closed["id"], first_thread["id"]);
    assert_ne!(closed["updated"], first_thread["updated"]);

    choose("unsubscribed", 2).await.error_for_status().unwrap();
    comment(&first, "A direct question for @bob-watch.").await;
    h.deliver_notifications("team", "private").await;
    let mention = h.json("/api/notifications?reason=mention", &bob).await;
    let [item] = mention["items"].as_array().unwrap().as_slice() else {
        panic!("the subscribed issue moved into the mention filter");
    };
    assert_eq!(item["id"], first_thread["id"]);
    assert!(item["href"].as_str().unwrap().ends_with("#issuecomment-3"));
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"follow","version":4})
    );
    let follows = h
        .json(
            "/api/notifications/subscriptions?reason=mention&sort=recent",
            &bob,
        )
        .await;
    assert_eq!(follows["items"][0]["origin"]["reason"], "mention");
    assert_eq!(follows["items"][0]["title"], "Followed discussion");
    choose("inherit", 4).await.error_for_status().unwrap();
    comment(&first, "Back to the repository setting").await;
    h.deliver_notifications("team", "private").await;
    let all = h.json("/api/notifications", &bob).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 2);
    assert_eq!(all["items"][0]["id"], first_thread["id"]);
    assert!(
        all["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-4")
    );
    h.close().await;
}

#[tokio::test]
async fn participation_follows_before_delivery_and_fences_a_newer_choice() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let preference = format!("{ROOT}/issues/1/subscription");
    let comment_path = format!("{ROOT}/issues/1/comments");
    let comment = |cookie: &str, body: &str| {
        let cookie = cookie.to_owned();
        let body = json!({"request_id":Uuid::now_v7(),"body":body});
        let h = &h;
        let path = &comment_path;
        async move {
            h.write(Method::POST, path, &cookie, body)
                .await
                .error_for_status()
                .unwrap();
        }
    };
    issue(&h, &alice, "Followed by participating", None).await;
    assert_eq!(
        h.json(&preference, &alice).await,
        json!({"mode":"follow","version":1})
    );
    comment(&bob, "First reply").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"follow","version":1})
    );
    comment(&alice, "An answer before the worker runs").await;
    h.deliver_notifications("team", "private").await;
    let alice_notices = h.json(WATCHED, &alice).await;
    let bob_notices = h.json(WATCHED, &bob).await;
    assert_eq!(alice_notices["items"].as_array().unwrap().len(), 1);
    assert_eq!(bob_notices["items"].as_array().unwrap().len(), 1);
    assert!(
        bob_notices["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-2")
    );

    h.write(
        Method::PUT,
        &preference,
        &bob,
        json!({"mode":"unsubscribed","version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    comment(&alice, "No notice during the unsubscribe").await;
    comment(&bob, "Participating again follows future replies").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"follow","version":3})
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &preference,
            &bob,
            json!({"mode":"unsubscribed","version":1})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    comment(&alice, "A reply after following again").await;
    h.deliver_notifications("team", "private").await;
    let replayed = h.json(WATCHED, &bob).await;
    assert_eq!(replayed["items"].as_array().unwrap().len(), 1);
    assert_eq!(replayed["items"][0]["id"], bob_notices["items"][0]["id"]);
    assert!(
        replayed["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-5")
    );
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(WATCHED, &bob).await, replayed);
    h.close().await;
}

#[tokio::test]
async fn a_reply_waits_for_the_mention_that_subscribes_its_recipient() {
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
    // Occupy the first follow page with earlier unresolved handles.
    for number in 0..16 {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":format!("Earlier issue {number}"),"body":"@unregistered"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Mentioned on this issue","body":"Please check this @bob."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/17/comments"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"A reply before the mention worker."}),
    )
    .await
    .error_for_status()
    .unwrap();

    let preference = format!("{ROOT}/issues/17/subscription");
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"inherit","version":0})
    );
    assert_eq!(h.json(WATCHED, &bob).await["items"], json!([]));

    // Directory can commit delivery before the worker finishes enrollment.
    // Replay that partial outcome and keep the user's existing read state.
    let repo = &h.server.repositories.values()[0];
    let cell = app::repository_cell(&h.server, repo).unwrap();
    let events = cell
        .client
        .query::<Pending>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let source = events
        .discussions
        .iter()
        .find(|event| event.follow.is_some())
        .unwrap();
    let notice = Notice {
        id: source.id,
        repository: repo.id,
        target: source.target.into(),
        created_at_ms: source.created_at_ms,
        sender: source.actor.name.clone(),
    };
    cell.client
        .command::<Deliver>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            Delivery {
                email: false,
                notice: notice.clone(),
                recipients: vec![Recipient {
                    actor: serde_json::from_value(profile["id"].clone()).unwrap(),
                    reason: Reason::Mention,
                    access: Access::Direct,
                }],
            },
        )
        .await
        .unwrap();
    h.write(
        Method::PUT,
        &format!("/api/notifications/{}/state", notice.thread_id()),
        &bob,
        json!({"repository_id":repo.id,"expected":notice.position(),"action":{"read":true}}),
    )
    .await
    .error_for_status()
    .unwrap();

    assert_eq!(
        h.json("/api/notifications", &bob).await["items"][0]["read"],
        true
    );

    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"follow","version":1})
    );
    let inbox = h.json("/api/notifications", &bob).await;
    let items = inbox["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reason"], "mention");
    assert_eq!(items[0]["id"], json!(notice.thread_id()));
    assert!(
        items[0]["href"]
            .as_str()
            .unwrap()
            .ends_with("#issuecomment-1")
    );
    assert_eq!(items[0]["read"], false);
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json("/api/notifications", &bob).await, inbox);
    h.close().await;
}

#[tokio::test]
async fn a_newer_unsubscribe_survives_assignment_delivery_and_reassignment_follows() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    issue(
        &h,
        &alice,
        "Assigned to this discussion",
        Some(bob_id.clone()),
    )
    .await;

    let preference = format!("{ROOT}/issues/1/subscription");
    h.write(
        Method::PUT,
        &preference,
        &bob,
        json!({"mode":"unsubscribed","version":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"unsubscribed","version":1})
    );
    let assigned = "/api/notifications?reason=assigned";
    assert_eq!(
        h.json(assigned, &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let path = format!("{ROOT}/issues/1");
    for (version, assignees) in [(1, json!([])), (2, json!([bob_id]))] {
        h.write(
            Method::PATCH,
            &path,
            &alice,
            json!({"version":version,"assignee_ids":assignees}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::POST,
        &format!("{path}/comments"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"Reply after reassigning."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json(&preference, &bob).await,
        json!({"mode":"follow","version":2})
    );
    let follows = h
        .json(
            "/api/notifications/subscriptions?reason=assignment&sort=oldest",
            &bob,
        )
        .await;
    assert_eq!(follows["items"][0]["origin"]["reason"], "assignment");
    assert_eq!(follows["items"][0]["title"], "Assigned to this discussion");
    let assigned = h.json(assigned, &bob).await;
    let [item] = assigned["items"].as_array().unwrap().as_slice() else {
        panic!("reassignment keeps one thread and its reason");
    };
    assert!(item["href"].as_str().unwrap().ends_with("#issuecomment-1"));
    h.close().await;
}
