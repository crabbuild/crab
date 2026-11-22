use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::{git, http::Harness},
};

#[tokio::test]
async fn inbox_delivers_a_review_request_and_mention_to_the_current_member() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
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
    h.write(
        Method::PUT,
        "/api/repos/team/private/pulls/1/review-requests",
        &alice,
        json!({"version":1,"change":{"kind":"request","actor_id":bob_id}}),
    )
    .await
    .error_for_status()
    .unwrap();

    let page = h.json("/api/inbox?limit=1", &bob).await;
    let review = &page["items"][0];
    assert_eq!(review["reason"], "review_requested");
    assert_eq!(review["title"], "Please review");
    assert_eq!(review["target"], json!({"kind":"pull","number":1}));
    let next = format!("/api/inbox?cursor={}", page["next"].as_str().unwrap());
    let mentions = h.json(&next, &bob).await;
    assert_eq!(mentions["next"], Value::Null);
    assert_eq!(mentions["items"].as_array().unwrap().len(), 1);
    assert_eq!(mentions["items"][0]["reason"], "mention");
    assert_eq!(
        mentions["items"][0]["target"],
        json!({"kind":"issue","number":1})
    );
    assert_eq!(h.json("/api/inbox", &alice).await["items"], json!([]));
    assert_eq!(
        h.http
            .get(format!("{}{next}", h.origin))
            .header("cookie", &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    for private in [&h.provider.issuer, "bob-id", "alice-id"] {
        assert!(!page.to_string().contains(private));
    }

    let id = review["repository"]["id"].as_str().unwrap();
    let mark = format!("/api/inbox/{id}/{}", review["event"]);
    let read = json!({"at":review["at"],"read":true});
    assert_eq!(
        h.write(Method::PATCH, &mark, &alice, read.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.http
            .patch(format!("{}{mark}", h.origin))
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
        h.write(Method::PATCH, &mark, &bob, read.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(h.json("/api/inbox", &bob).await["items"], mentions["items"]);
    let all = format!("/api/inbox?unread=false&repository={id}");
    assert_eq!(h.json(&all, &bob).await["items"][0]["read"], true);
    h.write(
        Method::PATCH,
        &mark,
        &bob,
        json!({"at":review["at"],"read":false}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json("/api/inbox", &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
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
    assert_eq!(h.json("/api/inbox", &bob).await["items"], json!([]));
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
        h.write(Method::PATCH, &mark, &bob, read).await.status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
