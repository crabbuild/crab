use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
use crate::test_support::{git, http::Harness};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private/pulls/1/review-requests";
async fn seed(h: &Harness) {
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    cell.client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                draft: false,
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Review requests".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                base_oid: "a".repeat(40),
                head_ref: "refs/heads/feature".into(),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
}
async fn change(h: &Harness, cookie: &str, version: u64, change: Value) -> reqwest::Response {
    h.write(
        Method::PUT,
        ROOT,
        cookie,
        json!({"version":version,"change":change}),
    )
    .await
}

#[tokio::test]
async fn qualified_members_can_be_requested_cancelled_and_fulfilled_by_http_review() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), url.as_str()).await;
    h.write(Method::POST,"/api/repos/team/private/pulls",&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Please review","body":"Ready for another member","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    let page = h.json(ROOT, &alice).await;
    assert_eq!(page["eligible"], json!([{"subject":"bob-id","name":"Bob"}]));
    let requested = change(&h, &alice, 1, json!({"kind":"request","subject":"bob-id"}))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(requested["state"]["kind"], "requested");
    let id = requested["id"].as_str().unwrap();
    assert!(!requested.to_string().contains("bob-id"));
    assert!(!requested.to_string().contains(&h.provider.issuer));
    change(&h, &alice, 2, json!({"kind":"cancel","id":id}))
        .await
        .error_for_status()
        .unwrap();
    change(&h, &alice, 3, json!({"kind":"request","subject":"bob-id"}))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"cancel","id":id}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let review = json!({"request_id":uuid::Uuid::now_v7(),"state":"approved","body":"Looks good"});
    let created = h
        .write(
            Method::POST,
            "/api/repos/team/private/pulls/1/reviews",
            &bob,
            review.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let page = h.json(ROOT, &alice).await;
    assert_eq!(page["version"], 5);
    assert_eq!(
        page["items"][0]["state"],
        json!({"kind":"fulfilled","review":created["number"]})
    );
    change(&h, &alice, 5, json!({"kind":"request","subject":"bob-id"}))
        .await
        .error_for_status()
        .unwrap();
    h.write(
        Method::POST,
        "/api/repos/team/private/pulls/1/reviews",
        &bob,
        review,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(ROOT, &alice).await["items"][0]["state"]["kind"],
        "requested"
    );
    h.close().await;
}

#[tokio::test]
async fn review_request_routes_enforce_membership_csrf_versions_and_archive() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h).await;
    let body = json!({"version":1,"change":{"kind":"request","subject":"bob-id"}});
    assert_eq!(
        h.http
            .put(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, &h.origin)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        change(&h, &alice, 1, json!({"kind":"request","subject":"unknown"}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        change(
            &h,
            &alice,
            1,
            json!({"kind":"request","subject":"alice-id"})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    for suffix in ["?limit=0", "?limit=51", "?before=invalid"] {
        assert_eq!(
            h.http
                .get(format!("{}{ROOT}{suffix}", h.origin))
                .header(header::COOKIE, &alice)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(ROOT, &bob).await["can_manage"], false);
    assert_eq!(
        change(&h, &bob, 1, json!({"kind":"request","subject":"bob-id"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    change(&h, &alice, 1, json!({"kind":"request","subject":"bob-id"}))
        .await
        .error_for_status()
        .unwrap();
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"request","subject":"bob-id"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.json(ROOT, &alice).await["items"][0]["state"]["kind"],
        "requested"
    );
    h.close().await;
}

#[tokio::test]
async fn renaming_or_revoking_a_member_preserves_the_request_identity_and_cancellation() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h).await;
    let created = change(&h, &alice, 1, json!({"kind":"request","subject":"bob-id"}))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let members_path = "/api/repos/team/private/members";
    let members = h.json(members_path, &alice).await;
    let renamed=h.write(Method::PUT,members_path,&alice,json!({"expected_revision":members["revision"],"members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Robert","access":"read"}]})).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    let page = h.json(ROOT, &alice).await;
    assert_eq!(page["items"][0]["name"], "Robert");
    assert_eq!(page["items"][0]["id"], created["id"]);
    h.write(Method::PUT,members_path,&alice,json!({"expected_revision":renamed["revision"],"members":[{"subject":"alice-id","name":"Alice","access":"admin"}]})).await.error_for_status().unwrap();
    let page = h.json(ROOT, &alice).await;
    assert_eq!(page["items"][0]["available"], false);
    assert_eq!(page["eligible"], json!([]));
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"request","subject":"bob-id"}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    change(&h, &alice, 2, json!({"kind":"cancel","id":created["id"]}))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(ROOT, &alice).await["items"][0]["state"]["kind"],
        "cancelled"
    );
    h.close().await;
}
