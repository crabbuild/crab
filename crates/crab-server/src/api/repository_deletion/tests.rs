use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

use super::*;
use crate::test_support::http::{Harness, organizations};

#[tokio::test(flavor = "multi_thread")]
async fn deletion_policy_and_restore_preserve_repository_identity() {
    let h = Harness::new(false).await;
    let (organization, _, member) = organizations::bootstrap(&h).await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    for (method, route, cookie, body) in [
        (
            Method::PUT,
            format!("{root}/invitations/{member}"),
            &alice,
            json!({"version":0,"role":"member"}),
        ),
        (
            Method::POST,
            format!("{root}/invitation"),
            &bob,
            json!({"version":1,"accept":true}),
        ),
        (
            Method::PUT,
            format!("{root}/repository-policy"),
            &alice,
            json!({"version":0,"member_creation":true,"baseline":null}),
        ),
    ] {
        h.write(method, &route, cookie, body)
            .await
            .error_for_status()
            .unwrap();
    }
    let created: Value = h
        .write(
            Method::POST,
            &format!("{root}/repositories/creation"),
            &bob,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    let owner = created["owner"].as_str().unwrap();
    let refs = format!("/api/repos/{owner}/library/refs");
    let listing = format!("/api/repos?organization={organization}");
    let deletion = format!("/api/repositories/{id}/deletion");
    let deleted = format!("{root}/deleted-repositories");
    let restore = format!("{deleted}/{id}");
    let request = Uuid::now_v7();
    let submit =
        json!({"request_id":request,"version":0,"confirmation":format!("{owner}/library")});

    assert_eq!(h.json(&deletion, &alice).await["can_delete"], true);
    assert_eq!(h.json(&deletion, &bob).await["can_delete"], false);
    assert_eq!(
        h.write(Method::DELETE, &deletion, &bob, submit.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    h.write(
        Method::PUT,
        &format!("{root}/repository-policy"),
        &alice,
        json!({"version":1,"member_creation":true,"member_deletion":true,"baseline":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&deletion, &bob).await["can_delete"], true);
    let mut mistyped = submit.clone();
    mistyped["confirmation"] = json!("library");
    assert_eq!(
        h.write(Method::DELETE, &deletion, &bob, mistyped)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    for _ in 0..2 {
        h.write(Method::DELETE, &deletion, &bob, submit.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(h.json(&listing, &alice).await["repositories"], json!([]));
    for (path, cookie) in [(&refs, &alice), (&deleted, &bob)] {
        assert_eq!(
            h.http
                .get(format!("{}{path}", h.origin))
                .header(header::COOKIE, cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let rows = h.json(&deleted, &alice).await;
    assert_eq!(rows["items"][0]["id"], id);
    assert_eq!(rows["items"][0]["deletion_id"], json!(request));
    assert_eq!(rows["items"][0]["can_restore"], true);
    assert_eq!(
        h.write(Method::PUT, &restore, &bob, json!({"deletion_id":request}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    h.write(
        Method::PUT,
        &restore,
        &alice,
        json!({"deletion_id":request}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&deleted, &alice).await["items"], json!([]));
    assert_eq!(h.json(&listing, &alice).await["repositories"][0]["id"], id);
    assert_eq!(
        h.json(&refs, &alice).await["unborn_head"],
        "refs/heads/main"
    );
    assert_eq!(
        h.write(Method::DELETE, &deletion, &bob, submit)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}
