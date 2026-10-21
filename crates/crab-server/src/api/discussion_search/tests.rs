use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use serde_json::{Value, json};

#[tokio::test]
async fn completion_uses_authorized_repository_labels_and_bounds_cursor_input() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let response = h.write(reqwest::Method::POST, &format!("{root}/labels"), &cookie, json!({
        "request_id": uuid::Uuid::now_v7(), "name":"Help wanted", "color":"abcdef", "description":"First contribution",
    })).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let query = "🦀 label:he";
    let cursor = query.encode_utf16().count().to_string();
    let response = h
        .http
        .get(format!("{}{root}/discussion-search", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("query", query), ("cursor", cursor.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["items"][0]["value"], "label:\"Help wanted\"");
    assert_eq!(
        (response["start"].as_u64(), response["end"].as_u64()),
        (Some(3), Some(11))
    );
    for (query, cursor) in [("🦀", "1"), ("a", "3"), ("a\n", "2")] {
        let response = h
            .http
            .get(format!("{}{root}/discussion-search", h.origin))
            .header(header::COOKIE, &cookie)
            .query(&[("query", query), ("cursor", cursor)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{root}/discussion-search", h.origin))
        .header(header::COOKIE, &outsider)
        .query(&[("query", "label:"), ("cursor", "6")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn issue_workspace_returns_tagged_pull_results_and_paginates_colliding_numbers() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let token: Value = h
        .write(
            reqwest::Method::POST,
            "/api/git-token",
            &cookie,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut git_url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    crate::test_support::git::push_review_branches(source.path(), git_url.as_str()).await;
    let issue: Value = h.write(reqwest::Method::POST, &format!("{root}/issues"), &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Issue result","body":"Private issue body"})).await.error_for_status().unwrap().json().await.unwrap();
    let pull: Value = h.write(reqwest::Method::POST, &format!("{root}/pulls"), &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Pull result","body":"Private pull body","base_ref":"refs/heads/main","head_ref":"refs/heads/feature","draft":true})).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(
        (issue["number"].as_u64(), pull["number"].as_u64()),
        (Some(1), Some(1))
    );
    let default = h.json(&format!("{root}/issues"), &cookie).await;
    assert_eq!(default["items"].as_array().unwrap().len(), 1);
    assert_eq!(default["items"][0]["kind"], "issue");
    let pulls = h.json(&format!("{root}/issues?q=is%3Apr"), &cookie).await;
    assert_eq!(pulls["items"][0]["kind"], "pull");
    assert_eq!(pulls["items"][0]["draft"], true);
    assert!(pulls["items"][0].get("body").is_none());
    assert_eq!(
        pulls["items"][0]["author"],
        json!({"name":"Alice","profile":null})
    );
    let query = format!("{root}/issues?q=is%3Aopen&sort=comments&direction=asc&limit=1");
    let first = h.json(&query, &cookie).await;
    assert_eq!(first["matching"], 2);
    assert_eq!(first["counts"]["open"], 2);
    assert_eq!(first["items"][0]["kind"], "issue");
    let cursor = first["next"].as_str().unwrap();
    let last = h.json(&format!("{query}&before={cursor}"), &cookie).await;
    assert_eq!(last["items"][0]["kind"], "pull");
    assert_eq!(last["items"][0]["number"], 1);
    assert!(last["next"].is_null());
    let response = h
        .http
        .get(format!(
            "{}{root}/pulls?q=is%3Aopen&sort=comments&direction=asc&limit=1&before={cursor}",
            h.origin
        ))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    h.write(
        reqwest::Method::PATCH,
        &format!("{root}/pulls/1"),
        &cookie,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let open = h.json(&format!("{root}/issues?q=is%3Aopen"), &cookie).await;
    assert_eq!(open["counts"], json!({"open":1,"closed":1,"merged":0}));
    assert_eq!(open["matching"], 1);
    let default_pulls = h.json(&format!("{root}/pulls"), &cookie).await;
    assert!(default_pulls["items"].as_array().unwrap().is_empty());
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{root}/issues?q=is%3Apr", h.origin))
        .header(header::COOKIE, &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn milestone_options_and_queries_follow_real_assignment_versions_and_repository_access() {
    use reqwest::Method;
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let milestone: Value = h.write(Method::POST, &format!("{root}/milestones"), &cookie,
        json!({"request_id":uuid::Uuid::now_v7(), "title":"Release one", "description":"Planning", "due_on":null}))
        .await.error_for_status().unwrap().json().await.unwrap();
    for number in 1..=2 {
        h.write(Method::POST, &format!("{root}/issues"), &cookie,
            json!({"request_id":uuid::Uuid::now_v7(), "title":format!("Work {number}"), "body":"Work"}))
            .await.error_for_status().unwrap();
        h.write(
            Method::PUT,
            &format!("{root}/issues/{number}/milestone"),
            &cookie,
            json!({"version":0,"milestone":milestone["number"]}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::PATCH,
        &format!("{root}/issues/2"),
        &cookie,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let response = h
        .http
        .get(format!("{}{root}/discussion-search/milestones", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[
            ("query", "is:issue is:open"),
            ("search", "release"),
            ("state", "open"),
        ])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["items"][0]["title"], "Release one");
    let query = response["items"][0]["query"].as_str().unwrap();
    let found = h
        .http
        .get(format!("{}{root}/issues", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("q", query)])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        (
            found["matching"].as_u64(),
            found["counts"]["open"].as_u64(),
            found["counts"]["closed"].as_u64()
        ),
        (Some(1), Some(1), Some(1))
    );
    let missing = h
        .http
        .get(format!("{}{root}/issues", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("q", "no:milestone")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(missing["matching"], 0);
    let current = h.json(&format!("{root}/milestones/1"), &cookie).await;
    h.write(Method::PATCH, &format!("{root}/milestones/1"), &cookie,
        json!({"version":current["milestone"]["version"],"title":"Release two","description":"Planning","due_on":null,"state":"closed"}))
        .await.error_for_status().unwrap();
    let closed = h
        .http
        .get(format!("{}{root}/discussion-search/milestones", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("search", "two"), ("state", "closed")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(closed["items"][0]["title"], "Release two");
    let completion = h
        .http
        .get(format!("{}{root}/discussion-search", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("query", "milestone:two"), ("cursor", "13")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(completion["items"][0]["value"], "milestone:\"Release two\"");
    for params in [
        vec![("query", "is:open OR")],
        vec![("search", "bad\nsearch")],
        vec![("unknown", "true")],
    ] {
        assert_eq!(
            h.http
                .get(format!("{}{root}/discussion-search/milestones", h.origin))
                .header(header::COOKIE, &cookie)
                .query(&params)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let revoked = h.login().await;
    let members = h.server.repositories.values()[0]
        .config
        .members
        .iter()
        .filter(|member| member.subject != "bob-id")
        .cloned()
        .collect();
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    for path in [
        "discussion-search/milestones",
        "discussion-search?query=milestone:&cursor=10",
    ] {
        assert_eq!(
            h.http
                .get(format!("{}{root}/{path}", h.origin))
                .header(header::COOKIE, &revoked)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{root}/discussion-search/milestones", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn label_choices_preserve_query_destinations_and_recheck_durable_access() {
    use reqwest::Method;
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let label: Value = h
        .write(
            Method::POST,
            &format!("{root}/labels"),
            &cookie,
            json!({
                "request_id": uuid::Uuid::now_v7(),
                "name": "Help wanted",
                "color": "abcdef",
                "description": "Good first issue",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/issues"),
        &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Labeled issue","body":"Work"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PATCH,
        &format!("{root}/issues/1"),
        &cookie,
        json!({"version":1,"label_ids":[label["id"]]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let response = h
        .http
        .get(format!("{}{root}/discussion-search/labels", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("query", "is:open"), ("search", "wanted")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["items"][0]["title"], "Help wanted");
    assert_eq!(response["items"][0]["description"], "Good first issue");
    assert_eq!(response["items"][0]["color"], "abcdef");
    for (field, matching) in [("query", 1), ("exclude_query", 0)] {
        let target = response["items"][0][field].as_str().unwrap();
        let result = h
            .http
            .get(format!("{}{root}/issues", h.origin))
            .header(header::COOKIE, &cookie)
            .query(&[("q", target)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(result["matching"], matching);
    }
    for params in [
        vec![("query", "label:")],
        vec![("search", "bad\nvalue")],
        vec![("state", "open")],
    ] {
        assert_eq!(
            h.http
                .get(format!("{}{root}/discussion-search/labels", h.origin))
                .header(header::COOKIE, &cookie)
                .query(&params)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let members = h.server.repositories.values()[0]
        .config
        .members
        .iter()
        .filter(|member| member.subject != "bob-id")
        .cloned()
        .collect();
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(
        h.http
            .get(format!("{}{root}/discussion-search/labels", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
