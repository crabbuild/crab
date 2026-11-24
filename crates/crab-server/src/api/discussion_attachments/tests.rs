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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_transfer_publishes_verified_attachments_with_destination_access() {
    use std::collections::BTreeMap;

    use sha2::{Digest, Sha256};

    use super::object_path;
    use crate::{
        app,
        cells::{
            directory::IdentityKey,
            repository::{
                collaboration::attachments::transferred_id,
                issues::transfer::{
                    Capture, CaptureInput, CaptureOutcome, Stage, StageInput, StageOutcome,
                    publication::{
                        Prepare, PrepareInput, PrepareOutcome, Publish, PublishOutcome, Retire,
                        RetireOutcome,
                    },
                },
            },
        },
    };

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let bytes = b"Attached release notes";
    let uploaded: Value = upload(&h, &alice, id, "notes.txt", "text/plain", bytes)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let old = uploaded["url"].as_str().unwrap();
    let body = format!(
        "[See files](<{old}> \"literal {old}\") and [use the reference][files].\n\n\
         `[code]({old})` [external](https://example.com{old})\n\n\
         [files]: {old} \"keep this title\""
    );
    let issue: Value = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Move files with issue","body":body}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let issue_number = issue["number"].as_u64().unwrap();
    let comment = format!("Shared attachment: [notes]({old}).");
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/{issue_number}/comments"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":comment}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.add_repository("library").await;
    let repositories = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
    };
    let source = repositories("private");
    let target = repositories("library");
    let origin_cell = app::repository_cell(&h.server, &source).unwrap();
    let target_cell = app::repository_cell(&h.server, &target).unwrap();
    let request = Uuid::now_v7();
    let actor = IdentityKey {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
    };
    let moved_id = transferred_id(request, id);
    let new = format!("/api/discussion-attachments/{}/{moved_id}", target.id);
    let mut capture = CaptureInput::Start {
        number: issue_number,
        expected_version: issue["version"].as_u64().unwrap(),
    };
    let mut first = true;
    let prepared = loop {
        let output = origin_cell
            .client
            .query::<Capture>(origin_cell.target, None, capture)
            .await
            .unwrap()
            .output;
        let CaptureOutcome::Ready { frame } = output else {
            panic!("{output:?}");
        };
        let observed = frame.source;
        let mut input = StageInput {
            request,
            destination: target.id,
            actor: actor.clone(),
            frame: *frame,
            copied: BTreeMap::new(),
        };
        if input.frame.records.uploads().next().is_some() {
            let replay = serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
            assert_eq!(
                target_cell
                    .client
                    .command::<Stage>(
                        target_cell.target,
                        app::mutation_identity().unwrap(),
                        replay
                    )
                    .await
                    .unwrap()
                    .output,
                StageOutcome::AwaitingCopies
            );
        }
        if first {
            assert_eq!(
                target_cell
                    .client
                    .command::<Prepare>(
                        target_cell.target,
                        app::mutation_identity().unwrap(),
                        PrepareInput {
                            request,
                            source: observed,
                            actor: actor.clone(),
                            commitment: [0; 32],
                            ownership_epoch: 1,
                        },
                    )
                    .await
                    .unwrap()
                    .output,
                PrepareOutcome::Incomplete
            );
        }
        for upload in input.frame.records.uploads() {
            let (data, _) = source
                .store
                .get_with_etag_bounded(&object_path(&source, upload.id), upload.size)
                .await
                .unwrap();
            assert_eq!(data.len() as u64, upload.size);
            assert_eq!(format!("{:x}", Sha256::digest(&data)), upload.digest);
            let created = target
                .store
                .put_if_absent_verified(&object_path(&target, moved_id), data)
                .await
                .unwrap();
            assert_eq!(created, first);
            input.copied.insert(moved_id, upload.digest.clone());
        }
        let progress = target_cell
            .client
            .command::<Stage>(target_cell.target, app::mutation_identity().unwrap(), input)
            .await
            .unwrap()
            .output;
        let StageOutcome::Saved { progress } = progress else {
            panic!("{progress:?}");
        };
        let Some(cursor) = progress.next else {
            break PrepareInput {
                request,
                source: observed,
                actor: actor.clone(),
                commitment: progress.commitment,
                ownership_epoch: 1,
            };
        };
        first = false;
        capture = CaptureInput::Continue {
            source: observed,
            cursor,
        };
    };
    assert_eq!(
        download(&h, &alice, &new).await.status(),
        StatusCode::NOT_FOUND
    );
    let reserved = target_cell
        .client
        .command::<Prepare>(
            target_cell.target,
            app::mutation_identity().unwrap(),
            prepared,
        )
        .await
        .unwrap()
        .output;
    let PrepareOutcome::Reserved { reservation } = reserved else {
        panic!("{reserved:?}");
    };
    let retired = origin_cell
        .client
        .command::<Retire>(
            origin_cell.target,
            app::mutation_identity().unwrap(),
            reservation,
        )
        .await
        .unwrap()
        .output;
    let RetireOutcome::Committed { decision } = retired else {
        panic!("{retired:?}");
    };
    let destination = decision.destination();
    for _ in 0..2 {
        assert_eq!(
            target_cell
                .client
                .command::<Publish>(
                    target_cell.target,
                    app::mutation_identity().unwrap(),
                    decision.clone(),
                )
                .await
                .unwrap()
                .output,
            PublishOutcome::Published { destination }
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let catalog = h.server.catalog().unwrap();
    let mut members = catalog.membership("team", "private").await.unwrap().members;
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(
        download(&h, &bob, old).await.status(),
        StatusCode::NOT_FOUND
    );
    let downloaded = download(&h, &bob, &new).await.error_for_status().unwrap();
    assert_eq!(downloaded.bytes().await.unwrap().as_ref(), bytes);
    let path = format!("/api/repos/team/library/issues/{}", destination.number);
    assert_eq!(
        h.json(&path, &bob).await["body"],
        body.replace(&format!("<{old}>"), &format!("<{new}>"))
            .replace(&format!("[files]: {old}"), &format!("[files]: {new}"))
    );
    assert_eq!(
        h.json(&format!("{path}/comments"), &bob).await["items"][0]["body"],
        comment.replace(old, &new)
    );
    h.close().await;
}
