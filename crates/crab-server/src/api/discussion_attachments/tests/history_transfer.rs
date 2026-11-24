use super::*;
use crate::cells::repository::collaboration::attachments::transferred_id;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_restores_history_and_only_copies_unredacted_revision_attachments() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let previous = Uuid::now_v7();
    let removed = Uuid::now_v7();
    let historical_bytes = b"Evidence from the retained edit";
    let upload_view = async |id, name, bytes: &[u8]| -> Value {
        upload(&h, &alice, id, name, "text/plain", bytes)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    let historical = upload_view(previous, "evidence.txt", historical_bytes).await;
    let discarded = upload_view(removed, "discarded.txt", b"File from a removed revision").await;
    let original = format!(
        "Original issue with [evidence]({})",
        historical["url"].as_str().unwrap()
    );
    let secret = format!(
        "Removed text with [file]({})",
        discarded["url"].as_str().unwrap()
    );
    let list = format!("{ROOT}/issues");
    let write = async |method, address: &str, data| -> Value {
        h.write(method, address, &alice, data)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    write(
        Method::POST,
        &list,
        json!({"request_id":Uuid::now_v7(),"title":"Existing issue","body":""}),
    )
    .await;
    let first = write(
        Method::POST,
        &list,
        json!({"request_id":Uuid::now_v7(),"title":"Move with edit history","body":original}),
    )
    .await;
    let number = first["number"].as_u64().unwrap();
    let origin = format!("{list}/{number}");
    let second = write(
        Method::PATCH,
        &origin,
        json!({"version":first["version"],"body":secret}),
    )
    .await;
    let current = write(
        Method::PATCH,
        &origin,
        json!({"version":second["version"],"body":"Current issue body"}),
    )
    .await;
    let history = format!("{ROOT}/edit-history/issues/{number}");
    let revisions = h.json(&history, &alice).await;
    let recent = revisions["items"][0]["number"].as_u64().unwrap();
    write(
        Method::DELETE,
        &format!("{history}?revision={recent}"),
        json!({}),
    )
    .await;
    let redacted_source = h
        .json(&format!("{history}?revision={recent}"), &alice)
        .await;
    let first_source_edit = &revisions["items"][1];

    let comment = write(
        Method::POST,
        &format!("{origin}/comments"),
        json!({"request_id":Uuid::now_v7(),"body":"Original comment"}),
    )
    .await;
    let comment_number = comment["number"].as_u64().unwrap();
    write(
        Method::PATCH,
        &format!("{origin}/comments/{comment_number}"),
        json!({"version":comment["version"],"body":"Current comment"}),
    )
    .await;

    h.add_repository("library").await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let request_id = Uuid::now_v7();
    let moved = h
        .write(
            Method::POST,
            &format!("{origin}/transfer"),
            &alice,
            json!({"request_id":request_id, "version":current["version"], "destination":repo.id}),
        )
        .await;
    assert_eq!(moved.status(), StatusCode::ACCEPTED);
    let new_number = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let old = h.json(&origin, &alice).await;
            if old["transfer"]["state"] == "ready" {
                break old["transfer"]["number"].as_u64().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(new_number, number);
    let destination = "/api/repos/team/library";
    let new_issue = h
        .json(&format!("{destination}/issues/{new_number}"), &alice)
        .await;
    assert_eq!(new_issue["body"], "Current issue body");
    assert_eq!(new_issue["has_edit_history"], true);
    let new_history = format!("{destination}/edit-history/issues/{new_number}");
    let new_revisions = h.json(&new_history, &alice).await;
    let newest = &new_revisions["items"][0];
    let oldest = &new_revisions["items"][1];
    assert_ne!(oldest["number"], first_source_edit["number"]);
    assert_eq!(oldest["editor"], first_source_edit["editor"]);
    assert_eq!(oldest["edited_at"], first_source_edit["edited_at"]);
    assert_eq!(
        newest["redaction"],
        redacted_source["revision"]["redaction"]
    );
    let redacted = h
        .json(
            &format!("{new_history}?revision={}", newest["number"]),
            &alice,
        )
        .await;
    assert!(redacted["before"].is_null() && redacted["after"].is_null());

    let old_version = format!("{new_history}?revision={}", oldest["number"]);
    let visible = h.json(&old_version, &alice).await;
    let new_file = format!(
        "/api/discussion-attachments/{}/{}",
        repo.id,
        transferred_id(request_id, previous)
    );
    assert_eq!(
        visible["before"]["body"],
        format!("Original issue with [evidence]({new_file})")
    );
    assert!(visible["after"].is_null());
    assert_eq!(
        download(&h, &alice, &new_file)
            .await
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        historical_bytes,
    );
    let removed_path = super::super::object_path(&repo, transferred_id(request_id, removed));
    assert!(matches!(
        repo.store.get_with_etag_bounded(&removed_path, 1024).await,
        Err(crab_storage::StorageError::NotFound { .. })
    ));

    let comment_history = format!("{new_history}/comments/{comment_number}");
    let comment_revisions = h.json(&comment_history, &alice).await;
    let detail = h
        .json(
            &format!(
                "{comment_history}?revision={}",
                comment_revisions["items"][0]["number"]
            ),
            &alice,
        )
        .await;
    assert_eq!(detail["before"]["body"], "Original comment");
    assert_eq!(detail["after"]["body"], "Current comment");

    let session = h.json("/api/session", &alice).await;
    h.http
        .delete(format!("{}{old_version}", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-crab-repository-id", repo.id.to_string())
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(h.json(&old_version, &alice).await["before"].is_null());
    h.close().await;
}
