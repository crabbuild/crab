use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private/milestones";
const ISSUE: &str = "/api/repos/team/private/issues";

fn creation(title: &str) -> Value {
    json!({"request_id":uuid::Uuid::now_v7(),"title":title,"description":"Release planning","due_on":"2028-02-29"})
}

#[tokio::test]
async fn milestone_crud_preserves_replay_identity_and_rejects_stale_edits() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let mut invalid = creation("Invalid date");
    invalid["due_on"] = json!("2027-02-29");
    assert_eq!(
        h.write(Method::POST, ROOT, &cookie, invalid).await.status(),
        StatusCode::BAD_REQUEST
    );
    let request = creation("Version one");
    let response = h.write(Method::POST, ROOT, &cookie, request.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["number"], 1);
    assert_eq!(created["author"], "Alice");
    let replay: Value = h
        .write(Method::POST, ROOT, &cookie, request)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(replay, created);
    let conflict = h
        .write(Method::POST, ROOT, &cookie, creation("VERSION ONE"))
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let path = format!("{ROOT}/1");
    let edit = json!({"version":1,"title":"Ready to ship","description":"Updated","due_on":null,"state":"closed"});
    let response = h.write(Method::PATCH, &path, &cookie, edit.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let saved: Value = response.json().await.unwrap();
    assert_eq!(saved["state"], "closed");
    assert_eq!(
        h.write(Method::PATCH, &path, &cookie, edit).await.status(),
        StatusCode::CONFLICT
    );
    let list = h.json(&format!("{ROOT}?state=closed"), &cookie).await;
    assert_eq!(list["items"][0]["title"], "Ready to ship");
    let reopen = json!({"version":2,"title":"Ready to ship","description":"Updated","due_on":null,"state":"open"});
    assert_eq!(
        h.write(Method::PATCH, &path, &cookie, reopen)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        h.json(&path, &cookie).await["milestone"]["closed_at"],
        Value::Null
    );
    h.close().await;
}

#[tokio::test]
async fn milestone_assignment_and_issue_progress_survive_http_retries() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    h.write(Method::POST, ROOT, &cookie, creation("Release"))
        .await
        .error_for_status()
        .unwrap();
    let issue = json!({"request_id":uuid::Uuid::now_v7(),"title":"Ship it","body":"One step"});
    h.write(Method::POST, ISSUE, &cookie, issue)
        .await
        .error_for_status()
        .unwrap();
    let path = format!("{ISSUE}/1/milestone");
    let selection = json!({"version":0,"milestone":1});
    for _ in 0..2 {
        let response = h
            .write(Method::PUT, &path, &cookie, selection.clone())
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let selected: Value = response.json().await.unwrap();
        assert_eq!(selected["milestone"]["open_count"], 1);
        assert_eq!(selected["version"], 1);
    }
    let items = h.json(&format!("{ROOT}/1/items"), &cookie).await;
    assert_eq!(
        items["items"][0]["subject"],
        json!({"kind":"issue","number":1})
    );
    h.write(
        Method::PATCH,
        &format!("{ISSUE}/1"),
        &cookie,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let value = h.json(&format!("{ROOT}/1"), &cookie).await;
    assert_eq!(value["milestone"]["open_count"], 0);
    assert_eq!(value["milestone"]["closed_count"], 1);
    h.write(
        Method::PUT,
        &path,
        &cookie,
        json!({"version":1,"milestone":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{ROOT}/1/items"), &cookie).await["items"],
        json!([])
    );
    h.close().await;
}

#[tokio::test]
async fn milestone_writes_require_membership_csrf_and_an_unarchived_repository() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let response = h
        .http
        .post(format!("{}{ROOT}", h.origin))
        .header(header::COOKIE, &alice)
        .json(&creation("Unprotected"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = h
        .http
        .put(format!("{}{ISSUE}/1/milestone", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .json(&json!({"version":0,"milestone":null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(ROOT, &bob).await["can_manage"], false);
    assert_eq!(
        h.write(Method::POST, ROOT, &bob, creation("Read only"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let repo = h.server.repositories.values()[0].id;
    let cell = h.server.cells().unwrap().repository(repo).unwrap();
    cell.client
        .command::<crate::cells::repository::ReplaceRepositoryLifecycle>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            crate::cells::repository::ReplaceRepositoryLifecycleInput {
                expected_version: 0,
                archived: true,
            },
        )
        .await
        .unwrap();
    let response = h
        .write(Method::POST, ROOT, &alice, creation("Archived"))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.json(ROOT, &alice).await["items"], json!([]));
    h.close().await;
}

#[tokio::test]
async fn milestone_request_boundaries_reject_invalid_pagination_and_targets() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    for query in ["limit=0", "limit=51", "before=0", "before=9007199254740992"] {
        let response = h
            .http
            .get(format!("{}{ROOT}?{query}", h.origin))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    h.write(Method::POST, ROOT, &cookie, creation("Valid"))
        .await
        .error_for_status()
        .unwrap();
    for query in ["limit=0", "before=0"] {
        let response = h
            .http
            .get(format!("{}{ROOT}/1/items?{query}", h.origin))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    h.write(
        Method::POST,
        ISSUE,
        &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Target","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    let response = h
        .write(
            Method::PUT,
            &format!("{ISSUE}/1/milestone"),
            &cookie,
            json!({"version":0,"milestone":0}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
