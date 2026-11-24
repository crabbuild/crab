use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::{git, http::Harness},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_transfer_notices_follow_membership_in_both_repositories() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    let original: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    for (cookie, handle) in [(&bob, "bob"), (&outsider, "outside-reader")] {
        h.write(
            Method::POST,
            "/api/profile",
            cookie,
            json!({"request_id":Uuid::now_v7(),"handle":handle,"name":handle}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let catalog = h.server.catalog().unwrap();
    let admin = RepositoryMember {
        subject: "alice-id".into(),
        name: "Alice".into(),
        access: RepositoryAccess::Admin,
    };
    let destination = catalog
        .set_members(
            "team",
            "library",
            vec![
                admin.clone(),
                RepositoryMember {
                    subject: "bob-id".into(),
                    name: "Bob".into(),
                    access: RepositoryAccess::Read,
                },
                RepositoryMember {
                    subject: "outsider-id".into(),
                    name: "Outside reader".into(),
                    access: RepositoryAccess::Read,
                },
            ],
            true,
        )
        .await
        .unwrap()
        .id;
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Moved discussion","body":"Hello @bob, @outside-reader"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json("/api/notifications?reason=mention", &bob).await["items"][0]["title"],
        "Moved discussion"
    );
    let issue = "/api/repos/team/private/issues/1";
    let version = h.json(issue, &alice).await["version"].clone();
    let move_request =
        json!({"request_id":Uuid::now_v7(),"version":version,"destination":destination});
    let transfer = format!("{issue}/transfer");
    assert_eq!(
        h.write(Method::POST, &transfer, &alice, move_request.clone())
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if h.json(issue, &alice).await["transfer"]["state"] == "ready" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    h.deliver_notifications("team", "library").await;
    h.write(Method::POST, &transfer, &alice, move_request)
        .await
        .error_for_status()
        .unwrap();
    h.deliver_notifications("team", "library").await;
    let inbox = "/api/notifications?reason=issue_transferred";
    let page = h.json(inbox, &bob).await;
    let author = original["id"].as_str().unwrap();
    let author_inbox = format!("{inbox}&author={author}");
    assert_eq!(h.json(&author_inbox, &bob).await, page);
    assert_eq!(
        h.json(&format!("{inbox}&author=me"), &bob).await["items"],
        json!([])
    );
    let [notice] = page["items"].as_array().unwrap().as_slice() else {
        panic!("expected one issue transfer notice, got {page}");
    };
    assert_eq!(notice["title"], "Moved discussion");
    assert_eq!(
        notice["href"],
        format!("/team/library/issues/1?repository_id={destination}")
    );
    assert_eq!(
        notice["subject"],
        json!({"kind":"issue_transfer","number":1})
    );
    assert_eq!(h.json(inbox, &outsider).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await["unread"],
        true
    );

    let mark = format!(
        "/api/notifications/{}/state",
        notice["id"].as_str().unwrap()
    );
    let save =
        json!({"repository_id":destination,"expected":notice["updated"],"action":{"save":true}});
    assert_eq!(
        h.write(Method::PUT, &mark, &bob, save.clone())
            .await
            .status(),
        StatusCode::OK
    );
    for read in [true, false] {
        h.write(
            Method::PUT, &mark, &bob,
            json!({"repository_id":destination,"expected":notice["updated"],"action":{"read":read}}),
        ).await.error_for_status().unwrap();
        assert_eq!(
            h.json("/api/notifications/unread", &bob).await["unread"],
            !read
        );
    }
    catalog
        .set_members("team", "private", vec![admin], true)
        .await
        .unwrap();
    assert_eq!(h.json(inbox, &bob).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications?view=saved", &bob).await["items"],
        json!([])
    );
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await["unread"],
        false
    );
    assert_eq!(
        h.write(Method::PUT, &mark, &bob, save).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.json("/api/repos/team/library/issues/1", &bob).await["title"],
        "Moved discussion"
    );
}

#[tokio::test]
async fn inbox_delivers_a_review_request_and_mention_to_the_current_member() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let original: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let bob_id = h.assignee_id(&alice, "Bob").await;
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

    let issue = json!({"request_id":Uuid::now_v7(),"title":"Question","body":"Hi @Bob and @bob"});
    for _ in 0..2 {
        h.write(
            Method::POST,
            "/api/repos/team/private/issues",
            &alice,
            issue.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::PATCH,
        "/api/repos/team/private/issues/1",
        &alice,
        json!({"version":1,"body":"Thanks @bob"}),
    )
    .await
    .error_for_status()
    .unwrap();

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
    let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), url.as_str()).await;
    h.write(
        Method::POST, "/api/repos/team/private/pulls", &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Please review","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    ).await.error_for_status().unwrap();
    let requested: Value = h
        .write(
            Method::PUT,
            "/api/repos/team/private/pulls/1/review-requests",
            &alice,
            json!({"version":1,"change":{"kind":"request","actor_id":bob_id}}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    h.deliver_notifications("team", "private").await;
    let page = h.json("/api/notifications?limit=1&unread=true", &bob).await;
    let review = &page["items"][0];
    assert_eq!(review["reason"], "review_requested");
    assert_eq!(review["title"], "Please review");
    let author = original["id"].as_str().unwrap();
    let authored = h
        .json(
            &format!("/api/notifications?kind=pull&author={author}"),
            &bob,
        )
        .await;
    assert_eq!(authored["items"], json!([review]));
    assert_eq!(
        h.json("/api/notifications?kind=pull&author=me", &bob).await["items"],
        json!([])
    );
    assert!(
        review["href"]
            .as_str()
            .unwrap()
            .starts_with("/team/private/pull/1?repository_id=")
    );
    let next = format!(
        "/api/notifications?unread=true&before={}",
        page["next"].as_str().unwrap()
    );
    let remaining = h.json(&next, &bob).await;
    assert_eq!(remaining["next"], Value::Null);
    let [mention] = remaining["items"].as_array().unwrap().as_slice() else {
        panic!("the issue edit and original mention share a conversation");
    };
    assert_eq!(mention["reason"], "mention");
    assert!(
        mention["href"]
            .as_str()
            .unwrap()
            .starts_with("/team/private/issues/1?repository_id=")
    );
    assert_eq!(
        h.json("/api/notifications?unread=true", &alice).await["items"],
        json!([])
    );
    assert_eq!(h.json(&next, &alice).await["items"], json!([]));
    for private in [&h.provider.issuer, "bob-id", "alice-id"] {
        assert!(!page.to_string().contains(private));
    }

    let id = review["href"]
        .as_str()
        .unwrap()
        .split("repository_id=")
        .nth(1)
        .unwrap();
    let mark = format!(
        "/api/notifications/{}/state",
        review["id"].as_str().unwrap()
    );
    let read = json!({"repository_id":id,"expected":review["updated"],"action":{"read":true}});
    assert_eq!(
        h.write(Method::PUT, &mark, &alice, read.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json("/api/notifications?unread=true", &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.http
            .put(format!("{}{mark}", h.origin))
            .header("cookie", &bob)
            .header("origin", &h.origin)
            .json(&read)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        h.write(Method::PUT, &mark, &bob, read.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(
        h.json("/api/notifications?unread=true", &bob).await["items"],
        remaining["items"]
    );
    let all = format!("/api/notifications?unread=false&repository={id}");
    assert_eq!(h.json(&all, &bob).await["items"][0]["read"], true);
    h.write(
        Method::PUT,
        &mark,
        &bob,
        json!({"repository_id":id,"expected":review["updated"],"action":{"read":false}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json("/api/notifications?unread=true", &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let unsubscribe = json!({"items":[{
        "id":review["id"],"repository_id":id,"subject":review["subject"],"expected":review["updated"]
    }]});
    let result: Value = h
        .write(
            Method::POST,
            "/api/notifications/unsubscribe",
            &bob,
            unsubscribe,
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["items"][0]["status"], "unsubscribed");
    assert_eq!(
        h.json("/api/notifications?unread=true", &bob).await["items"],
        remaining["items"]
    );
    let requests_path = "/api/repos/team/private/pulls/1/review-requests";
    h.write(
        Method::PUT,
        requests_path,
        &alice,
        json!({"version":2,"change":{"kind":"cancel","id":requested["id"]}}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        requests_path,
        &alice,
        json!({"version":3,"change":{"kind":"request","actor_id":bob_id}}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    let reopened = h
        .json(
            "/api/notifications?reason=review_requested&unread=true",
            &bob,
        )
        .await;
    assert_eq!(reopened["items"][0]["id"], review["id"]);
    assert_ne!(reopened["items"][0]["updated"], review["updated"]);
    assert_eq!(
        h.json("/api/repos/team/private/pulls/1/subscription", &bob)
            .await["mode"],
        "follow"
    );

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
    assert_eq!(
        h.json("/api/notifications?unread=true", &bob).await["items"],
        json!([])
    );
    assert_eq!(h.json(&next, &bob).await["items"], json!([]));
    assert_eq!(
        h.http
            .get(format!("{}{all}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(Method::PUT, &mark, &bob, read).await.status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn assignment_notices_track_committed_additions_and_current_access() {
    const ROOT: &str = "/api/repos/team/private";
    const INBOX: &str = "/api/notifications?reason=assigned";
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let alice_id = h.assignee_id(&alice, "Alice").await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let issue = json!({
        "request_id":Uuid::now_v7(),"title":"Assigned issue","body":"",
        "metadata":{"assignee_ids":[alice_id,bob_id]}
    });
    for _ in 0..2 {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            issue.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
    }

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
        json!({"request_id":Uuid::now_v7(),"title":"Assigned change","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
            "metadata":{"assignee_ids":[bob_id]}}),
    )
    .await
    .error_for_status()
    .unwrap();

    // Retaining Bob must not notify again. Removing him and applying the
    // same bulk action to both discussion kinds must notify once per item.
    h.write(
        Method::PATCH,
        &format!("{ROOT}/issues/1"),
        &alice,
        json!({"version":1,"assignee_ids":[bob_id]}),
    )
    .await
    .error_for_status()
    .unwrap();
    for (resource, version) in [("issues", 2), ("pulls", 1)] {
        h.write(
            Method::PATCH,
            &format!("{ROOT}/{resource}/1"),
            &alice,
            json!({"version":version,"assignee_ids":[]}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let repository = h.server.repositories.values()[0].id;
    let items = h
        .json(
            &format!(
                "{ROOT}/discussions/selection?repository_id={repository}&items=issue:1:3,pull:1:2"
            ),
            &alice,
        )
        .await["items"]
        .clone();
    let bulk = json!({"repository_id":repository,"request_id":Uuid::now_v7(),"items":items,
        "action":{"kind":"assignees","add":[bob_id],"remove":[]}});
    for _ in 0..2 {
        let result: Value = h
            .write(
                Method::POST,
                &format!("{ROOT}/discussions/bulk"),
                &alice,
                bulk.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["count"], 2);
    }
    h.deliver_notifications("team", "private").await;
    h.deliver_notifications("team", "private").await;

    let page = h.json(INBOX, &bob).await;
    let mut hrefs: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["sender"], "Alice");
            assert_eq!(item["reason"], "assigned");
            item["href"].as_str().unwrap()
        })
        .collect();
    hrefs.sort_unstable();
    let issue = format!("/team/private/issues/1?repository_id={repository}");
    let pull = format!("/team/private/pull/1?repository_id={repository}");
    assert_eq!(hrefs, [&issue, &pull]);
    assert_eq!(h.json(INBOX, &alice).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications?reason=mention", &bob).await["items"],
        json!([])
    );

    let mut writer = h.server.repositories.values()[0].config.members[0].clone();
    writer.access = RepositoryAccess::Admin;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", vec![writer], true)
        .await
        .unwrap();
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));
    h.close().await;
}
