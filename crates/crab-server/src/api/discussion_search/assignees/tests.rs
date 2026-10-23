use crate::{
    cells::repository::{assignments::Assignee, *},
    test_support::http::Harness,
};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

async fn page(h: &Harness, cookie: &str, view: &str, query: &str) -> Value {
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    h.json(&format!("/api/repos/team/private/{view}?{params}"), cookie)
        .await
}

#[tokio::test]
async fn assignee_filters_use_stable_accounts_across_profiles_revocation_boolean_counts_and_cursors()
 {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let alice_id = h.assignee_id(&alice, "Alice").await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    let root = "/api/repos/team/private";
    for title in ["Assigned work", "Unassigned work"] {
        h.write(
            Method::POST,
            &format!("{root}/issues"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::PATCH,
        &format!("{root}/issues/1"),
        &alice,
        json!({"version":1,"assignee_ids":[alice_id,bob_id]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let repo = h.server.repositories.values()[0].id;
    let author = RepositoryAuthor {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
        name: "Alice".into(),
    };
    let client = h.server.cells().unwrap();
    client
        .command::<CreatePull>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                metadata: Default::default(),
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: author.clone(),
                draft: false,
                title: "Assigned pull".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    client
        .command::<UpdatePull>(
            repo,
            crate::app::mutation_identity().unwrap(),
            UpdatePullInput {
                number: 1,
                actor: author,
                can_triage: true,
                version: 1,
                title: None,
                body: None,
                state: None,
                label_ids: None,
                assignees: Some(vec![Assignee {
                    id: uuid::Uuid::parse_str(bob_id.as_str().unwrap()).unwrap(),
                    name: "Bob".into(),
                }]),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page(&h, &alice, "issues", "assignee:@me").await["matching"],
        1
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        page(&h, &bob, "issues", "assignee:@me").await["matching"],
        2
    );
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":uuid::Uuid::now_v7(),"handle":"public-bob","name":"Public Bob"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(profile["id"], bob_id);
    for (view, query, expected) in [
        ("issues", "assignee:public-bob", 2),
        ("issues", "assignee:public-bob in:title", 2),
        ("pulls", "assignee:public-bob assigned in:title", 1),
        (
            "issues",
            "(assignee:public-bob in:comments) OR no:assignee",
            3,
        ),
        ("pulls", "assignee:public-bob", 1),
        ("issues", "assignee:*", 2),
        ("issues", "no:assignee", 1),
        ("issues", "-assignee:public-bob", 1),
        ("issues", "assignee:unknown OR assignee:public-bob", 2),
        ("issues", "assignee:public-bob no:assignee", 0),
        ("issues", "assignee:public-bob author:@me", 2),
        ("issues", "(assignee:public-bob OR no:assignee) is:open", 3),
    ] {
        let result = page(&h, &alice, view, query).await;
        assert_eq!(
            (&result["matching"], &result["counts"]["open"]),
            (&json!(expected), &json!(expected)),
            "{query}"
        );
    }
    let literal = format!("assignee:actor/{}", bob_id.as_str().unwrap());
    assert_eq!(page(&h, &alice, "issues", &literal).await["matching"], 2);
    let first = h
        .json(
            &format!("{root}/issues?q=assignee%3Apublic-bob&limit=1"),
            &alice,
        )
        .await;
    let before = first["next"].as_str().unwrap();
    h.write(
        Method::PATCH,
        "/api/profile",
        &bob,
        json!({"version":profile["version"],"handle":"renamed-bob","name":"Renamed Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        page(&h, &alice, "issues", "assignee:public-bob").await["matching"],
        0
    );
    let stale = h
        .http
        .get(format!("{}{root}/issues", h.origin))
        .header(header::COOKIE, &alice)
        .query(&[("q", "assignee:public-bob"), ("before", before)])
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        page(&h, &alice, "issues", "assignee:renamed-bob").await["matching"],
        2
    );
    let options = h
        .json(
            &format!(
                "{root}/discussion-search/assignees?query=is%3Aopen%20no%3Aassignee&search=renamed"
            ),
            &alice,
        )
        .await;
    assert_eq!(options["items"][0]["person"]["id"], bob_id);
    assert_eq!(options["items"][0]["query"], "is:open assignee:renamed-bob");
    assert_eq!(options["self"]["query"], "is:open assignee:@me");
    assert_eq!(options["missing"]["selected"], true);
    assert!(
        !options.to_string().contains("alice-id")
            && !options.to_string().contains("bob-id")
            && !options.to_string().contains("issuer")
    );
    let catalog = h.server.catalog().unwrap();
    let mut members = h.server.repositories.values()[0].config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(
        page(&h, &alice, "issues", "assignee:renamed-bob").await["matching"],
        2
    );
    let historical = h
        .json(
            &format!("{root}/discussion-search/assignees?search=Bob"),
            &alice,
        )
        .await;
    assert_eq!(historical["items"][0]["person"]["id"], bob_id);
    let denied = h
        .http
        .get(format!("{}{root}/discussion-search/assignees", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
