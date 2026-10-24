use crate::test_support::http::Harness;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn bulk_state_edits_use_versions_counts_replay_and_current_permissions() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let repository_id = h.server.repositories.values()[0].id;
    for title in ["First", "Second"] {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":title,"body":"Body"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let items = h
        .json(
            &format!("{ROOT}/discussions/selection?repository_id={repository_id}&items=issue:1:1,issue:2:1"),
            &alice,
        )
        .await["items"]
        .clone();
    let input = json!({"repository_id":repository_id,"request_id":uuid::Uuid::now_v7(),"items":items,"action":{"kind":"state","state":"closed","reason":"not_planned"}});
    let mut wrong = input.clone();
    wrong["repository_id"] = json!(uuid::Uuid::now_v7());
    let mismatch = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &alice,
            wrong,
        )
        .await;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    assert!(!mismatch.headers().contains_key("x-crab-write-outcome"));
    for _ in 0..2 {
        let result: Value = h
            .write(
                Method::POST,
                &format!("{ROOT}/discussions/bulk"),
                &alice,
                input.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["count"], 2);
    }
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/archive"),
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let recovered = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &alice,
            input.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(recovered["count"], 2);
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/archive"),
        &alice,
        json!({"expected_version":1,"archived":false,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let page = h
        .json(&format!("{ROOT}/issues?q=is:issue%20is:closed"), &alice)
        .await;
    assert_eq!(page["matching"], 2);
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1"), &alice).await["state_reason"],
        "not_planned"
    );
    let mut altered = input.clone();
    altered["action"]["state"] = json!("open");
    altered["action"].as_object_mut().unwrap().remove("reason");
    let conflict = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &alice,
            altered,
        )
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert!(!conflict.headers().contains_key("x-crab-write-outcome"));
    let mut stale = input.clone();
    stale["request_id"] = json!(uuid::Uuid::now_v7());
    let rejected = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &alice,
            stale,
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert_eq!(rejected.headers()["x-crab-write-outcome"], "rejected");
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let mut fresh = input.clone();
    fresh["request_id"] = json!(uuid::Uuid::now_v7());
    let denied = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &bob,
            fresh,
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(denied.headers()["x-crab-write-outcome"], "rejected");
    assert_eq!(
        h.json(&format!("{ROOT}/issues"), &bob).await["can_bulk_edit"],
        false
    );
    let before = h.json(&format!("{ROOT}/members"), &alice).await;
    let mut members = before["members"].clone();
    members[1]["access"] = json!("triage");
    h.write(
        Method::PUT,
        &format!("{ROOT}/members"),
        &alice,
        json!({"expected_revision":before["revision"],"members":members}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{ROOT}/issues"), &bob).await["can_bulk_edit"],
        true
    );
    let items = h
        .json(
            &format!("{ROOT}/discussions/selection?repository_id={repository_id}&items=issue:1:2,issue:2:2"),
            &bob,
        )
        .await["items"]
        .clone();
    h.write(Method::POST, &format!("{ROOT}/discussions/bulk"), &bob, json!({"repository_id":repository_id,"request_id":uuid::Uuid::now_v7(),"items":items,"action":{"kind":"state","state":"open"}})).await.error_for_status().unwrap();
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1"), &bob).await["state"],
        "open"
    );
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &bob,
            input.clone()
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let hidden = h
        .write(
            Method::POST,
            &format!("{ROOT}/discussions/bulk"),
            &outsider,
            input,
        )
        .await;
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert!(!hidden.headers().contains_key("x-crab-write-outcome"));
    h.close().await;
}

#[tokio::test]
async fn proven_rejection_remains_terminal_after_unarchive() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in ["First", "Second"] {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":title,"body":"Body"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let repository_id = h.server.repositories.values()[0].id;
    let items = h.json(&format!("{ROOT}/discussions/selection?repository_id={repository_id}&items=issue:1:1,issue:2:1"), &alice).await["items"].clone();
    let input = json!({"repository_id":repository_id,"request_id":uuid::Uuid::now_v7(),"items":items,"action":{"kind":"state","state":"closed"}});
    let path = format!("{ROOT}/discussions/bulk");
    for (version, archived) in [true, false].into_iter().enumerate() {
        h.write(
            Method::PUT,
            &format!("{ROOT}/settings/archive"),
            &alice,
            json!({"expected_version":version,"archived":archived,"repository":"team/private"}),
        )
        .await
        .error_for_status()
        .unwrap();
        let response = h.write(Method::POST, &path, &alice, input.clone()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-crab-write-outcome"], "rejected");
    }
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1"), &alice).await["state"],
        "open"
    );
    assert_eq!(
        h.json(&format!("{ROOT}/issues/2"), &alice).await["state"],
        "open"
    );
    let mut fresh = input;
    fresh["request_id"] = json!(uuid::Uuid::now_v7());
    h.write(Method::POST, &path, &alice, fresh)
        .await
        .error_for_status()
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn proven_rejection_remains_terminal_after_triage_grant() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Unchanged target","body":"Body"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let repository_id = h.server.repositories.values()[0].id;
    let items = h
        .json(
            &format!("{ROOT}/discussions/selection?repository_id={repository_id}&items=issue:1:1"),
            &alice,
        )
        .await["items"]
        .clone();
    let input = json!({"repository_id":repository_id,"request_id":uuid::Uuid::now_v7(),"items":items,"action":{"kind":"state","state":"closed"}});
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for promoted in [false, true] {
        if promoted {
            let current = h.json(&format!("{ROOT}/members"), &alice).await;
            let mut members = current["members"].clone();
            members[1]["access"] = json!("triage");
            h.write(
                Method::PUT,
                &format!("{ROOT}/members"),
                &alice,
                json!({"expected_revision":current["revision"],"members":members}),
            )
            .await
            .error_for_status()
            .unwrap();
        }
        let response = h
            .write(
                Method::POST,
                &format!("{ROOT}/discussions/bulk"),
                &bob,
                input.clone(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-crab-write-outcome"], "rejected");
    }
    assert_eq!(
        h.json(&format!("{ROOT}/issues/1"), &alice).await["state"],
        "open"
    );
    let mut fresh = input;
    fresh["request_id"] = json!(uuid::Uuid::now_v7());
    h.write(
        Method::POST,
        &format!("{ROOT}/discussions/bulk"),
        &bob,
        fresh,
    )
    .await
    .error_for_status()
    .unwrap();
    h.close().await;
}
