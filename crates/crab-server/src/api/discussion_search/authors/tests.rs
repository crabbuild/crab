use crate::{
    cells::repository::{
        CreateIssue, CreateIssueInput, CreatePull, CreatePullInput, RepositoryAuthor,
    },
    test_support::http::Harness,
};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

#[tokio::test]
async fn author_filters_bind_qualified_accounts_and_rebound_handle_cursors_without_changing_source()
{
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let root = "/api/repos/team/private";
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"handle":"public-alice","name":"Chosen name"}),
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
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Alice work","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        &format!("{root}/issues"),
        &bob,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Bob work","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    let repo = h.server.repositories.values()[0].id;
    let client = h.server.cells().unwrap();
    client
        .command::<CreateIssue>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreateIssueInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: format!("{}/other", h.provider.issuer),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Other issuer".into(),
                body: String::new(),
            },
        )
        .await
        .unwrap();
    client
        .command::<CreatePull>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                draft: false,
                title: "Alice pull".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    let source = h.json(&format!("{root}/issues/1"), &alice).await;
    let options = h.json(&format!("{root}/discussion-search/authors?query=is%3Aopen%20-author%3Abob&search=%40PUBLIC-A"), &alice).await;
    assert_eq!(options["items"].as_array().unwrap().len(), 1);
    assert_eq!(options["items"][0]["actor"]["id"], profile["id"]);
    assert_eq!(options["items"][0]["query"], "is:open author:public-alice");
    assert_eq!(options["self"]["query"], "is:open author:@me");
    assert_eq!(options["clear"]["query"], "is:open");
    let query = options["items"][0]["query"].as_str().unwrap();
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    for view in ["issues", "pulls"] {
        let page = h.json(&format!("{root}/{view}?{params}"), &alice).await;
        assert_eq!(page["matching"], if view == "issues" { 2 } else { 1 });
    }
    let body = options.to_string();
    assert!(!body.contains("issuer") && !body.contains("subject") && !body.contains("alice-id"));

    for (kind, query, cookie, expected) in [
        ("issues", "author:public-alice", &alice, 2),
        ("pulls", "author:public-alice", &alice, 1),
        ("issues", "-author:public-alice", &alice, 2),
        ("issues", "author:alice", &alice, 0),
        ("issues", "author:@me", &alice, 2),
        ("issues", "author:@me", &bob, 1),
        ("issues", "author:unknown OR author:@me", &alice, 2),
    ] {
        let params: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .finish();
        let page = h.json(&format!("{root}/{kind}?{params}"), cookie).await;
        assert_eq!(page["matching"], expected, "{query}");
        assert_eq!(page["counts"]["open"], expected, "{query}");
        assert_eq!(page["authored_by_me"], query == "author:@me");
    }
    let first = h
        .json(
            &format!("{root}/issues?q=author%3Apublic-alice&limit=1"),
            &alice,
        )
        .await;
    let cursor = first["next"].as_str().unwrap();
    h.write(
        Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":profile["version"],"handle":"renamed-alice","name":"New public name"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{root}/issues?q=author%3Apublic-alice"), &alice)
            .await["matching"],
        0
    );
    assert_eq!(
        h.json(
            &format!("{root}/discussion-search/authors?search=public-alice"),
            &alice
        )
        .await["items"],
        json!([])
    );
    let renamed = h
        .json(
            &format!("{root}/discussion-search/authors?search=renamed"),
            &alice,
        )
        .await;
    assert_eq!(renamed["items"][0]["actor"]["id"], profile["id"]);
    assert_eq!(renamed["items"][0]["actor"]["handle"], "renamed-alice");
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":uuid::Uuid::now_v7(),"handle":"public-alice","name":"Same public name"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{root}/issues?q=author%3Apublic-alice"), &alice)
            .await["matching"],
        1
    );
    let stale = h
        .http
        .get(format!("{}{root}/issues", h.origin))
        .header(header::COOKIE, &alice)
        .query(&[("q", "author:public-alice"), ("before", cursor)])
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        h.json(&format!("{root}/issues?q=author%3Arenamed-alice"), &alice)
            .await["matching"],
        2
    );
    assert_eq!(h.json(&format!("{root}/issues/1"), &alice).await, source);
    h.close().await;
}

#[tokio::test]
async fn author_completion_keeps_utf16_ranges_query_limits_and_durable_access_without_private_claims()
 {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"handle":"alice","name":"Public Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let root = "/api/repos/team/private";
    for (query, expected) in [
        ("\"🦀\" author:al".to_string(), 1),
        (format!("{} author:al", "x".repeat(245)), 0),
    ] {
        let response: Value = h
            .http
            .get(format!("{}{root}/discussion-search", h.origin))
            .header(header::COOKIE, &alice)
            .query(&[
                ("query", query.clone()),
                ("cursor", query.encode_utf16().count().to_string()),
            ])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["items"].as_array().unwrap().len(), expected);
        if expected == 1 {
            assert_eq!(response["start"], 5);
            assert_eq!(
                response["items"][0],
                json!({"value":"author:alice","description":"Public Alice"})
            );
        }
        let body = response.to_string();
        assert!(
            !body.contains("issuer") && !body.contains("subject") && !body.contains("alice-id")
        );
    }
    for (key, value) in [
        ("search", "x".repeat(257)),
        ("search", "bad\nname".into()),
        ("query", "author:".into()),
        ("state", "open".into()),
    ] {
        let response = h
            .http
            .get(format!("{}{root}/discussion-search/authors", h.origin))
            .header(header::COOKIE, &alice)
            .query(&[(key, value)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
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
    for path in [
        format!("{root}/issues?q=author%3Aalice"),
        format!("{root}/pulls?q=author%3Aalice"),
        format!("{root}/discussion-search?query=author%3Aal&cursor=9"),
        format!("{root}/discussion-search/authors?search=al"),
    ] {
        assert_eq!(
            h.http
                .get(format!("{}{path}", h.origin))
                .header(header::COOKIE, &bob)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    h.close().await;
}
