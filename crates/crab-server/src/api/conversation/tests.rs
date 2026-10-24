use crate::test_support::{git, http::Harness};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn conversation_windows_enrich_public_actors_and_keep_cursor_scope_and_authority() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in ["Issue", "Other"] {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":title,"body":"Description"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    for _ in 0..8 {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues/1/comments"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"body":"Conversation comment"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::PATCH,
        &format!("{ROOT}/issues/1"),
        &alice,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let path = format!("{ROOT}/issues/1/conversation");
    let latest = h.json(&format!("{path}?limit=3"), &alice).await;
    assert_eq!(latest["items"].as_array().unwrap().len(), 3);
    assert_eq!(latest["items"][2]["kind"], "activity");
    assert_eq!(
        latest["items"][2]["value"]["actor"],
        json!({"name":"Alice","profile":null})
    );
    assert_eq!(latest["has_later"], false);
    let older = latest["start"].as_str().unwrap();
    assert_eq!(
        h.http
            .get(format!(
                "{}{ROOT}/issues/2/conversation?before={older}",
                h.origin
            ))
            .header("cookie", &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let focus = h
        .json(&format!("{path}?focus=comment:1&limit=3"), &alice)
        .await;
    assert_eq!(focus["items"][0]["value"]["number"], 1);
    assert_eq!(focus["has_earlier"], false);
    assert_eq!(focus["has_later"], true);
    let mut cursor = focus["end"].as_str().unwrap().to_owned();
    let mut count = 1;
    loop {
        let page = h
            .json(&format!("{path}?after={cursor}&limit=3"), &alice)
            .await;
        count += page["items"].as_array().unwrap().len();
        if page["has_later"] == false {
            break;
        }
        cursor = page["end"].as_str().unwrap().to_owned();
    }
    assert_eq!(count, 9);
    let created=h.write(Method::POST,"/api/profile",&alice,json!({"request_id":uuid::Uuid::now_v7(),"handle":"activity-alice","name":"Public Alice"})).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    for resource in [&path, &format!("{ROOT}/issues/1/activity")] {
        let current = h.json(resource, &alice).await;
        let field = if resource.ends_with("activity") {
            "actor"
        } else {
            "author"
        };
        let entry = &current["items"][0];
        let author = if entry["kind"] == "comment" {
            &entry["value"][field]
        } else if entry["kind"] == "activity" {
            &entry["value"]["actor"]
        } else {
            &entry[field]
        };
        assert_eq!(author["profile"]["id"], created["id"]);
        assert_eq!(author["profile"]["handle"], "activity-alice");
        for private in ["alice-id", "issuer", "subject"] {
            assert!(!current.to_string().contains(private));
        }
    }
    h.write(
        Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":created["version"],"handle":"renamed-activity","name":"Renamed Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let activity = h.json(&format!("{ROOT}/issues/1/activity"), &alice).await;
    assert_eq!(
        activity["items"][0]["actor"]["profile"]["handle"],
        "renamed-activity"
    );
    assert_eq!(
        activity["items"][0]["actor"]["profile"]["id"],
        created["id"]
    );
    for query in [
        "limit=0",
        "limit=51",
        "before=invalid",
        "focus=review:0",
        "focus=comment:1&after=invalid",
        "unknown=1",
    ] {
        assert!(
            h.http
                .get(format!("{}{path}?{query}", h.origin))
                .header("cookie", &alice)
                .send()
                .await
                .unwrap()
                .status()
                .is_client_error(),
            "{query}"
        );
    }
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn review_windows_follow_live_head_and_missing_refs_without_opening_git_packs() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
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
    h.write(Method::POST,&format!("{ROOT}/pulls"),&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Timeline","body":"Description","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(Method::POST,&format!("{ROOT}/pulls/1/reviews"),&bob,json!({"request_id":uuid::Uuid::now_v7(),"body":"Approved","state":"approved","commit_oid":head})).await.error_for_status().unwrap();
    let path = format!("{ROOT}/pulls/1/conversation?focus=review:1");
    let first = h.json(&path, &alice).await;
    assert_eq!(first["head_oid"], head);
    assert_eq!(first["items"][0]["value"]["current"], true);
    std::fs::write(source.path().join("README.md"), "Changed after review\n").unwrap();
    git::success(source.path(), &["commit", "-am", "Advance source"]).await;
    let advanced = git::success(source.path(), &["rev-parse", "HEAD"]).await;
    git::success(source.path(), &["push", url.as_str(), "feature"]).await;
    let changed = h.json(&path, &alice).await;
    assert_eq!(changed["head_oid"], advanced);
    assert_eq!(changed["items"][0]["value"]["current"], false);
    git::success(
        source.path(),
        &["push", url.as_str(), "--delete", "feature"],
    )
    .await;
    let missing = h.json(&path, &alice).await;
    assert!(missing["head_oid"].is_null());
    assert!(missing["items"][0]["value"]["current"].is_null());
    h.close().await;
}
