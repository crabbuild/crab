use crate::test_support::{git, http::Harness};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
const ROOT: &str = "/api/repos/team/private/pulls/1";
async fn create_pull(h: &Harness, alice: &str) -> String {
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), url.as_str()).await;
    h.write(Method::POST,"/api/repos/team/private/pulls",alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Dismissal qualification","body":"Review lifecycle","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    head
}
#[tokio::test]
async fn dismissal_is_authorized_audited_and_recovers_after_write_revocation_and_archive() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let head = create_pull(&h, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let review=h.write(Method::POST,&format!("{ROOT}/reviews"),&bob,json!({"request_id":uuid::Uuid::now_v7(),"state":"changes_requested","body":"Please fix this","commit_oid":head})).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    let target = format!("{ROOT}/reviews/1/dismissals");
    let input = json!({"request_id":uuid::Uuid::now_v7(),"version":review["version"],"body":"Confirmed that the requested changes were completed."});
    assert_eq!(
        h.write(Method::PUT, &target, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.json(&format!("{ROOT}/reviews/1"), &bob).await["can_dismiss"],
        false
    );
    let stale = h
        .write(
            Method::PUT,
            &target,
            &alice,
            json!({"version":99,"request_id":uuid::Uuid::now_v7(),"body":"Changed source"}),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.headers()["x-crab-write-outcome"], "rejected");
    h.write(Method::PUT, &target, &alice, input.clone())
        .await
        .error_for_status()
        .unwrap();
    let saved = h.json(&format!("{ROOT}/reviews/1"), &bob).await;
    assert_eq!(saved["state"], "dismissed");
    assert_eq!(saved["dismissal"]["body"], input["body"]);
    assert_eq!(
        saved["dismissal"]["author"],
        json!({"name":"Alice","profile":null})
    );
    let roster = h.json(&format!("{ROOT}/reviewers"), &bob).await;
    assert_eq!(roster["items"][0]["review"]["state"], "dismissed");
    assert_eq!(roster["items"][0]["decision"], Value::Null);
    let conversation = h.json(&format!("{ROOT}/conversation"), &bob).await;
    assert!(
        conversation["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "review" && item["value"]["state"] == "dismissed")
    );
    assert!(
        conversation["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["value"]["action"]["kind"] == "review_dismissed")
    );
    for private in [h.provider.issuer.as_str(), "alice-id", "bob-id"] {
        assert!(!conversation.to_string().contains(private));
    }
    let mut conflicting = input.clone();
    conflicting["body"] = json!("Different intent");
    let changed = h
        .write(Method::PUT, &target, &alice, conflicting.clone())
        .await;
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    assert!(!changed.headers().contains_key("x-crab-write-outcome"));
    let lifecycle = h.json("/api/repos?owner=team&name=private", &alice).await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":lifecycle["repositories"][0]["archive_version"],"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let members = h.json("/api/repos/team/private/members", &alice).await;
    h.write(Method::PUT,"/api/repos/team/private/members",&alice,json!({"expected_revision":members["revision"],"members":[{"subject":"alice-id","name":"Alice","access":"read"},{"subject":"bob-id","name":"Bob","access":"admin"}]})).await.error_for_status().unwrap();
    h.write(Method::PUT, &target, &alice, input.clone())
        .await
        .error_for_status()
        .unwrap();
    let restored = h.json(&format!("{ROOT}/reviews/1"), &alice).await;
    assert_eq!(restored["dismissal"], saved["dismissal"]);
    assert_eq!(restored["version"], saved["version"]);
    // Archive presentation may replace an error status; it cannot mark an
    // intent conflict as a proven rejection of an earlier completed write.
    let changed = h.write(Method::PUT, &target, &alice, conflicting).await;
    assert_eq!(changed.status(), StatusCode::FORBIDDEN);
    assert!(!changed.headers().contains_key("x-crab-write-outcome"));
    h.close().await;
}

#[tokio::test]
async fn configured_protection_blocks_writer_dismissal_until_an_admin_replaces_it() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let head = create_pull(&h, &alice).await;
    let members = h.json("/api/repos/team/private/members", &alice).await;
    h.write(Method::PUT, "/api/repos/team/private/members", &alice, json!({
        "expected_revision":members["revision"],
        "members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Bob","access":"write"}]
    })).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let review = h.write(Method::POST, &format!("{ROOT}/reviews"), &bob, json!({
        "request_id":uuid::Uuid::now_v7(), "state":"approved", "body":"Approved", "commit_oid":head
    })).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    let input = json!({"request_id":uuid::Uuid::now_v7(),"version":review["version"],"body":"Superseded approval"});
    let catalog = h.json("/api/repos?owner=team&name=private", &bob).await;
    assert_eq!(catalog["repositories"][0]["protection_version"], 0);
    for (persisted, allowed) in [(false, false), (true, true)] {
        if persisted {
            h.write(
                Method::PUT,
                "/api/repos/team/private/settings/branch-protections",
                &alice,
                json!({"expected_version":0,"rules":[]}),
            )
            .await
            .error_for_status()
            .unwrap();
        }
        let detail = h.json(&format!("{ROOT}/reviews/1"), &bob).await;
        let list = h.json(&format!("{ROOT}/reviews"), &bob).await;
        let conversation = h.json(&format!("{ROOT}/conversation"), &bob).await;
        let entry = conversation["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "review")
            .unwrap();
        assert_eq!(
            (
                detail["can_dismiss"].as_bool(),
                list["items"][0]["can_dismiss"].as_bool(),
                entry["value"]["can_dismiss"].as_bool()
            ),
            (Some(allowed), Some(allowed), Some(allowed))
        );
        let response = h
            .write(
                Method::PUT,
                &format!("{ROOT}/reviews/1/dismissals"),
                &bob,
                input.clone(),
            )
            .await;
        if allowed {
            response.error_for_status().unwrap();
        } else {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()["x-crab-write-outcome"], "rejected");
            assert_eq!(
                h.json(&format!("{ROOT}/reviews/1"), &bob).await["state"],
                "approved"
            );
        }
    }
    assert_eq!(
        h.json(&format!("{ROOT}/reviews/1"), &bob).await["state"],
        "dismissed"
    );
    h.close().await;
}
