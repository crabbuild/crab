use crate::test_support::http::Harness;
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private";

async fn seed(h: &Harness, cookie: &str) -> Value {
    h.write(
        Method::POST,
        &format!("{ROOT}/labels"),
        cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"name":"bug","color":"1d76db"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/milestones"),
        cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Release","description":"","due_on":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    json!({"label_ids":[1],"assignee_ids":[h.assignee_id(cookie,"Bob").await],"milestone":1})
}
fn input(metadata: Value) -> Value {
    json!({"request_id":uuid::Uuid::now_v7(),"title":"Plan the release","body":"One atomic creation","metadata":metadata})
}

#[tokio::test]
async fn initial_issue_metadata_is_atomic_and_replay_does_not_restore_old_assignments() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let request = input(seed(&h, &cookie).await);
    let response = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &cookie,
            request.clone(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["version"], 1);
    assert_eq!(created["labels"][0]["id"], 1);
    assert_eq!(
        created["assignees"][0]["id"],
        request["metadata"]["assignee_ids"][0]
    );
    let path = format!("{ROOT}/issues/1");
    let assignment = h.json(&format!("{path}/milestone"), &cookie).await;
    assert_eq!(assignment["version"], 1);
    assert_eq!(assignment["milestone"]["open_count"], 1);
    h.write(
        Method::PATCH,
        &path,
        &cookie,
        json!({"version":1,"label_ids":[],"assignee_ids":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{path}/milestone"),
        &cookie,
        json!({"version":1,"milestone":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    let events = h.json(&format!("{path}/activity"), &cookie).await;
    let replay = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &cookie,
            request.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
    let saved: Value = replay.json().await.unwrap();
    assert_eq!(saved["labels"], json!([]));
    assert_eq!(
        h.json(&format!("{path}/milestone"), &cookie).await["milestone"],
        Value::Null
    );
    assert_eq!(h.json(&format!("{path}/activity"), &cookie).await, events);
    let mut changed = request;
    changed["metadata"]["label_ids"] = json!([]);
    assert_eq!(
        h.write(Method::POST, &format!("{ROOT}/issues"), &cookie, changed)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn invalid_initial_metadata_does_not_allocate_an_issue_or_change_progress() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let metadata = seed(&h, &cookie).await;
    for (field, value) in [
        ("label_ids", json!([999])),
        ("assignee_ids", json!([uuid::Uuid::now_v7()])),
        ("milestone", json!(999)),
    ] {
        let mut bad = metadata.clone();
        bad[field] = value;
        assert_eq!(
            h.write(Method::POST, &format!("{ROOT}/issues"), &cookie, input(bad))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        h.json(&format!("{ROOT}/milestones/1"), &cookie).await["milestone"]["open_count"],
        0
    );
    let created: Value = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &cookie,
            input(metadata),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["number"], 1);
    h.close().await;
}

#[tokio::test]
async fn rejected_initial_metadata_cannot_execute_after_its_target_appears() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let request = input(json!({"milestone":1}));
    let response = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &cookie,
            request.clone(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("x-crab-write-outcome").unwrap(),
        "rejected"
    );
    h.write(Method::POST,&format!("{ROOT}/milestones"),&cookie,json!({"request_id":uuid::Uuid::now_v7(),"title":"Now available","description":"","due_on":null})).await.error_for_status().unwrap();
    let replay = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &cookie,
            request.clone(),
        )
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        replay.headers().get("x-crab-write-outcome").unwrap(),
        "rejected"
    );
    let mut fresh = request;
    fresh["request_id"] = json!(uuid::Uuid::now_v7());
    let created: Value = h
        .write(Method::POST, &format!("{ROOT}/issues"), &cookie, fresh)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["number"], 1);
    h.close().await;
}
