use axum::http::{StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Method, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::http::Harness;

const ROOT: &str = "/api/repos/team/private";

async fn upload(
    h: &Harness,
    cookie: &str,
    id: Uuid,
    name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Response {
    let session = h.json("/api/session", cookie).await;
    h.http
        .post(format!("{}{ROOT}/discussion-attachments", h.origin))
        .query(&[("id", id.to_string()), ("name", name.to_owned())])
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
        .header(header::CONTENT_TYPE, content_type)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
}

async fn download(h: &Harness, cookie: &str, path: &str) -> Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn raster_draft_is_visible_only_to_uploader_until_native_issue_create() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let png = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgCF0FAAFYAQDfZmccAAAAAElFTkSuQmCC")
        .unwrap();
    let id = Uuid::now_v7();
    let repository = &h.server.repositories.values()[0];
    let url = format!("/api/discussion-attachments/{}/{id}", repository.id);
    let uploaded: Value = upload(&h, &alice, id, "pixel.png", "text/plain", &png)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        uploaded,
        json!({"url":url,"name":"pixel.png","inline":true})
    );
    let draft = download(&h, &alice, &url).await;
    assert_eq!(draft.status(), StatusCode::OK);
    assert_eq!(draft.headers()[header::CONTENT_TYPE], "image/png");
    assert!(
        draft.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("inline")
    );
    assert_eq!(draft.headers()["x-content-type-options"], "nosniff");
    assert_eq!(draft.headers()[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(draft.bytes().await.unwrap().as_ref(), png.as_slice());
    assert_eq!(
        download(&h, &bob, &url).await.status(),
        StatusCode::NOT_FOUND
    );

    let issue_body = format!("Reproduction\n\n![pixel]({url})");
    let issues = format!("{ROOT}/issues");
    let claim = h
        .write(
            Method::POST,
            &issues,
            &bob,
            json!({"request_id":Uuid::now_v7(),"title":"Someone else's draft","body":issue_body}),
        )
        .await;
    assert_eq!(claim.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        download(&h, &bob, &url).await.status(),
        StatusCode::NOT_FOUND
    );
    let issue = h
        .write(
            Method::POST,
            &issues,
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Issue with reproduction","body":issue_body}),
        )
        .await;
    assert_eq!(issue.status(), StatusCode::CREATED);
    assert_eq!(issue.json::<Value>().await.unwrap()["body"], issue_body);
    let reader = download(&h, &bob, &url).await;
    assert_eq!(reader.status(), StatusCode::OK);
    assert_eq!(reader.bytes().await.unwrap().as_ref(), png.as_slice());

    let mut members = repository.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(
        download(&h, &bob, &url).await.status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn upload_retry_preserves_bytes_and_conflicted_comment_does_not_publish_draft() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = Uuid::now_v7();
    let html = b"<!doctype html><title>Attached notes</title>";
    let result: Value = upload(&h, &alice, id, "notes.html", "image/png", html)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = result["url"].as_str().unwrap();
    assert_eq!(result["inline"], false);
    assert_eq!(
        upload(&h, &alice, id, "notes.html", "text/html", html)
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        result
    );
    assert_eq!(
        upload(
            &h,
            &alice,
            id,
            "notes.html",
            "text/html",
            b"different bytes"
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );

    let issue: Value = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Review notes","body":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let path = format!("{ROOT}/issues/{}", issue["number"]);
    let body = format!("See [notes]({url}).");
    let conflicted = h
        .write(
            Method::POST,
            &format!("{path}/state"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":2,"state":"closed","body":body}),
        )
        .await;
    assert_eq!(conflicted.status(), StatusCode::CONFLICT);
    assert_eq!(
        download(&h, &bob, url).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.json(&format!("{path}/comments"), &alice).await["items"],
        json!([])
    );

    let posted = h
        .write(
            Method::POST,
            &format!("{path}/state"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":1,"state":"closed","body":body}),
        )
        .await;
    assert_eq!(posted.status(), StatusCode::OK);
    assert_eq!(
        h.json(&format!("{path}/comments"), &alice).await["items"][0]["body"],
        body
    );
    let published = download(&h, &bob, url).await;
    assert_eq!(published.status(), StatusCode::OK);
    assert_eq!(
        published.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert!(
        published.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(published.bytes().await.unwrap().as_ref(), html);
    h.close().await;
}
