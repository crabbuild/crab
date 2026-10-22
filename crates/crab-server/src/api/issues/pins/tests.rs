use crate::{RepositoryAccess, test_support::http::Harness};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn pins_use_current_write_grants_limits_versions_and_recover_after_archive_or_downgrade() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for number in 1..=4 {
        h.write(Method::POST, &format!("{ROOT}/issues"), &alice, json!({"request_id":uuid::Uuid::now_v7(),"title":format!("Pinned candidate {number}"),"body":"Context"})).await.error_for_status().unwrap();
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let repo = h.server.repositories.values()[0].clone();
    let mut members = repo.config.members.clone();
    for access in [RepositoryAccess::Read, RepositoryAccess::Triage] {
        members[1].access = access;
        h.server
            .catalog()
            .unwrap()
            .set_members("team", "private", members.clone(), true)
            .await
            .unwrap();
        assert_eq!(
            h.json(&format!("{ROOT}/issues/pins"), &bob).await["can_manage"],
            false
        );
        let denied = h
            .write(
                Method::PUT,
                &format!("{ROOT}/issues/pins"),
                &bob,
                json!({"request_id":uuid::Uuid::now_v7(),"version":0,"numbers":[1]}),
            )
            .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(denied.headers()["x-crab-write-outcome"], "rejected");
    }
    let input = json!({"request_id":uuid::Uuid::now_v7(),"version":0,"numbers":[1,2,3]});
    for _ in 0..2 {
        let saved: Value = h
            .write(
                Method::PUT,
                &format!("{ROOT}/issues/pins"),
                &alice,
                input.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved["version"], 1);
    }
    let page = h.json(&format!("{ROOT}/issues/pins"), &bob).await;
    assert_eq!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["number"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(!page.to_string().contains("alice-id"));
    let limit = h
        .write(
            Method::PUT,
            &format!("{ROOT}/issues/pins"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"version":1,"numbers":[1,2,3,4]}),
        )
        .await;
    assert_eq!(limit.status(), StatusCode::BAD_REQUEST);
    let stale = h
        .write(
            Method::PUT,
            &format!("{ROOT}/issues/pins"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"version":0,"numbers":[3,2,1]}),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.headers()["x-crab-write-outcome"], "rejected");
    let mut altered = input.clone();
    altered["numbers"] = json!([2, 1, 3]);
    let mismatch = h
        .write(Method::PUT, &format!("{ROOT}/issues/pins"), &alice, altered)
        .await;
    assert_eq!(mismatch.status(), StatusCode::CONFLICT);
    assert!(!mismatch.headers().contains_key("x-crab-write-outcome"));
    members[1].access = RepositoryAccess::Write;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members.clone(), true)
        .await
        .unwrap();
    h.write(
        Method::PUT,
        &format!("{ROOT}/issues/pins"),
        &bob,
        json!({"request_id":uuid::Uuid::now_v7(),"version":1,"numbers":[3,2,1]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/archive"),
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    members[0].access = RepositoryAccess::Read;
    members[1].access = RepositoryAccess::Admin;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    h.write(
        Method::PUT,
        &format!("{ROOT}/issues/pins"),
        &alice,
        input.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    let page = h.json(&format!("{ROOT}/issues/pins"), &alice).await;
    assert_eq!(
        (
            page["version"].clone(),
            page["items"][0]["number"].clone(),
            page["can_manage"].clone()
        ),
        (json!(2), json!(3), json!(false))
    );
    let fresh = h
        .write(
            Method::PUT,
            &format!("{ROOT}/issues/pins"),
            &bob,
            json!({"request_id":uuid::Uuid::now_v7(),"version":2,"numbers":[]}),
        )
        .await;
    assert_eq!(fresh.status(), StatusCode::FORBIDDEN);
    assert_eq!(fresh.headers()["x-crab-write-outcome"], "rejected");
    let state = h
        .write(
            Method::POST,
            &format!("{ROOT}/issues/1/state"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"version":1,"state":"closed"}),
        )
        .await;
    assert_eq!(state.status(), StatusCode::FORBIDDEN);
    assert_eq!(state.headers()["x-crab-write-outcome"], "rejected");
    let mut changed = input;
    changed["numbers"] = json!([]);
    let uncertain = h
        .write(Method::PUT, &format!("{ROOT}/issues/pins"), &alice, changed)
        .await;
    assert_eq!(uncertain.status(), StatusCode::FORBIDDEN);
    assert!(!uncertain.headers().contains_key("x-crab-write-outcome"));
    h.close().await;
}
