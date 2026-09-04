use super::*;

const ROOT: &str = "/api/repos/team/private/issues";
fn nonce(id: usize) -> String {
    format!("00000000-0000-4000-8000-{id:012}")
}
fn input(id: usize) -> Value {
    json!({"request_id":nonce(id),"title":format!("Discuss change {id}"),"body":"Details with **Markdown**."})
}

async fn write(
    h: &Harness,
    cookie: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .request(method.parse().unwrap(), format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn issues_comments_edits_and_replays_preserve_durable_state() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let (status, issue) = write(&h, &cookie, "POST", ROOT, input(1)).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = issue["number"].as_u64().unwrap();
    let path = format!("{ROOT}/{id}");
    assert_eq!(
        write(&h, &cookie, "POST", ROOT, input(1)).await.1["number"],
        id
    );
    let (status, closed) = write(
        &h,
        &cookie,
        "PATCH",
        &path,
        json!({"version":1,"state":"closed","title":"Reviewed change"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(closed["state"], "closed");
    assert_eq!(
        write(&h, &cookie, "POST", ROOT, input(1)).await.1["state"],
        "closed"
    );
    assert_eq!(
        write(
            &h,
            &cookie,
            "PATCH",
            &path,
            json!({"version":1,"body":"stale edit"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let comments = format!("{path}/comments");
    let comment = json!({"request_id":nonce(2),"body":"Looks good.\n\n- [x] Reviewed"});
    let (status, first) = write(&h, &cookie, "POST", &comments, comment.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        write(&h, &cookie, "POST", &comments, comment).await.1["number"],
        first["number"]
    );
    let comment_path = format!("{comments}/{}", first["number"]);
    assert_eq!(
        write(
            &h,
            &cookie,
            "PATCH",
            &comment_path,
            json!({"version":1,"body":"Reviewed and verified"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        h.json(&comment_path, &cookie).await["body"],
        "Reviewed and verified"
    );
    assert!(
        h.json(ROOT, &cookie).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.json(&format!("{ROOT}?state=closed"), &cookie).await["items"][0]["number"],
        id
    );
    assert_eq!(
        write(
            &h,
            &cookie,
            "PATCH",
            &path,
            json!({"version":2,"state":"open"})
        )
        .await
        .0,
        StatusCode::OK
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_creation_is_idempotent_and_sparse_pages_do_not_duplicate_issues() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let results = futures_util::future::join_all(
        (0..8).map(|i| write(&h, &cookie, "POST", ROOT, input(if i < 4 { 1 } else { i }))),
    )
    .await;
    assert!(
        results
            .iter()
            .all(|(status, _)| *status == StatusCode::CREATED)
    );
    let mut cursor = None;
    let mut numbers = Vec::new();
    loop {
        let path = format!(
            "{ROOT}?state=all&limit=2{}",
            cursor.map_or(String::new(), |n| format!("&before={n}"))
        );
        let page = h.json(&path, &cookie).await;
        numbers.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|issue| issue["number"].as_u64().unwrap()),
        );
        cursor = page["next"].as_u64();
        if cursor.is_none() {
            break;
        }
        assert!(numbers.len() <= 5);
    }
    let unique = numbers
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), 5);
    assert_eq!(numbers.len(), unique.len());
    assert!(numbers.windows(2).all(|pair| pair[0] > pair[1]));
    assert_eq!(
        write(
            &h,
            &cookie,
            "POST",
            ROOT,
            json!({"request_id":nonce(1),"title":"Different submission","body":""})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn membership_author_ownership_and_csrf_protect_all_mutations() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let issue = write(&h, &alice, "POST", ROOT, input(1)).await.1;
    let path = format!("{ROOT}/{}", issue["number"]);
    let response = h
        .http
        .post(format!("{}{ROOT}", h.origin))
        .header(header::COOKIE, &alice)
        .body(input(2).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(&path, &bob).await["can_edit"], false);
    assert_eq!(
        write(
            &h,
            &bob,
            "PATCH",
            &path,
            json!({"version":1,"state":"closed"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let comments = format!("{path}/comments");
    let comment = write(
        &h,
        &bob,
        "POST",
        &comments,
        json!({"request_id":nonce(3),"body":"Another team member can join the discussion"}),
    )
    .await
    .1;
    let comment_path = format!("{comments}/{}", comment["number"]);
    assert_eq!(
        write(
            &h,
            &alice,
            "PATCH",
            &comment_path,
            json!({"version":1,"body":"not my comment"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    for endpoint in [
        ROOT.to_owned(),
        path.clone(),
        comments.clone(),
        comment_path.clone(),
    ] {
        let response = h
            .http
            .get(format!("{}{endpoint}", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(
        write(
            &h,
            &outsider,
            "POST",
            &comments,
            json!({"request_id":nonce(4),"body":"denied"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn retry_repairs_interruption_after_reservation_without_allocating_another_issue() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let issue = write(&h, &cookie, "POST", ROOT, input(1)).await.1;
    let repo = h.server.repositories.values().next().unwrap();
    // Recreate the durable state left between reserving a number and publishing its issue.
    let path = repo.layout.repo_path(&format!(
        "app/v1/issues/{:016}/issue.json",
        issue["number"].as_u64().unwrap()
    ));
    repo.store.delete(&path).await.unwrap();
    assert_eq!(write(&h, &cookie, "POST", ROOT, input(1)).await.1, issue);
    assert_eq!(
        h.json(ROOT, &cookie).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn unknown_storage_schema_and_invalid_inputs_fail_without_creating_content() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    for value in [
        json!({"request_id":"../escape","title":"Title","body":""}),
        json!({"request_id":nonce(1),"title":" ","body":""}),
        json!({"request_id":nonce(1),"title":"Title","body":"x".repeat(65_537)}),
    ] {
        assert_eq!(
            write(&h, &cookie, "POST", ROOT, value).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let repo = h.server.repositories.values().next().unwrap();
    assert!(
        repo.store
            .list_prefix(&repo.layout.repo_path("app/v1/issues"))
            .await
            .unwrap()
            .is_empty()
    );
    repo.store
        .put_overwrite(
            &repo.layout.repo_path("app/v1/issues/sequence.json"),
            json!({"schema_version":99,"data":{"last":0}})
                .to_string()
                .into(),
        )
        .await
        .unwrap();
    assert_eq!(
        write(&h, &cookie, "POST", ROOT, input(1)).await.0,
        StatusCode::BAD_GATEWAY
    );
    h.close().await;
}
