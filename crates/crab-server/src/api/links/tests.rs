use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::json;

const ROOT: &str = "/api/repos/team/private";
async fn seed(h: &Harness, cookie: &str) {
    h.write(Method::POST,&format!("{ROOT}/issues"),cookie,json!({"request_id":uuid::Uuid::now_v7(),"title":"Related issue","body":"Private issue body"})).await.error_for_status().unwrap();
    let repository = h.server.repositories.values()[0].id;
    let cell = h
        .server
        .repository_cells()
        .unwrap()
        .repository(repository)
        .unwrap();
    cell.client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Related pull".into(),
                body: "Private pull body".into(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn writers_link_and_unlink_both_sides_without_exposing_identity_claims() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    let pair = format!("{ROOT}/pulls/1/linked-issues/1");
    assert_eq!(
        h.json(&pair, &alice).await,
        json!({"active":false,"version":0})
    );
    let input = json!({"active":true,"version":0});
    for _ in 0..2 {
        let result: serde_json::Value = h
            .write(Method::PUT, &pair, &alice, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result, json!({"active":true,"version":1}));
    }
    let pull = h.json(&format!("{ROOT}/pulls/1/links"), &alice).await;
    let issue = h.json(&format!("{ROOT}/issues/1/links"), &alice).await;
    assert_eq!(pull["items"][0]["work"]["title"], "Related issue");
    assert_eq!(issue["items"][0]["work"]["title"], "Related pull");
    assert_eq!(pull["items"][0]["version"], 1);
    let activity = h.json(&format!("{ROOT}/issues/1/activity"), &alice).await;
    assert_eq!(activity["items"][0]["action"]["kind"], "issue_linked");
    for private in [
        "issuer",
        "alice-id",
        "Private issue body",
        "Private pull body",
    ] {
        assert!(
            !format!("{pull}{issue}{activity}").contains(private),
            "{private}"
        );
    }
    h.write(
        Method::PUT,
        &pair,
        &alice,
        json!({"active":false,"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1/links"), &alice).await["items"],
        json!([])
    );
    assert_eq!(
        h.json(&format!("{ROOT}/pulls/1/links"), &alice).await["items"],
        json!([])
    );
    assert_eq!(
        h.write(Method::PUT, &pair, &alice, input).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json(&pair, &alice).await,
        json!({"active":false,"version":2})
    );
    h.close().await;
}

#[tokio::test]
async fn link_routes_enforce_permissions_csrf_archive_and_input_bounds() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    let pair = format!("{ROOT}/pulls/1/linked-issues/1");
    let input = json!({"active":true,"version":0});
    assert_eq!(
        h.http
            .put(format!("{}{pair}", h.origin))
            .header(header::COOKIE, &alice)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1/links"), &bob).await["can_manage"],
        false
    );
    assert_eq!(
        h.write(Method::PUT, &pair, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}/issues/1/links", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    for query in [
        "limit=0",
        "limit=51",
        "before=0",
        "before=9007199254740992",
        "extra=1",
    ] {
        assert!(
            h.http
                .get(format!("{}{ROOT}/issues/1/links?{query}", h.origin))
                .header(header::COOKIE, &alice)
                .send()
                .await
                .unwrap()
                .status()
                .is_client_error(),
            "{query}"
        );
    }
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{ROOT}/pulls/1/linked-issues/999"),
            &alice,
            input.clone()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        h.write(
            Method::PUT,
            &pair,
            &alice,
            json!({"active":true,"version":9007199254740992u64})
        )
        .await
        .status()
        .is_client_error()
    );
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/archive"),
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::PUT, &pair, &alice, input).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.json(&pair, &alice).await,
        json!({"active":false,"version":0})
    );
    h.close().await;
}
