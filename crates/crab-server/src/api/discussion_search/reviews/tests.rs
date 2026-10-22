use crate::{
    cells::repository::{
        review_requests::{Selection, SetReviewRequest},
        *,
    },
    test_support::http::Harness,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
#[tokio::test]
async fn reviewer_filters_resolve_public_handles_and_current_accounts() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let root = "/api/repos/team/private";
    let repo = h.server.repositories.values()[0].id;
    let client = h.server.cells().unwrap();
    let author = RepositoryAuthor {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
        name: "Alice".into(),
    };
    let reviewer = RepositoryAuthor {
        issuer: h.provider.issuer.clone(),
        subject: "bob-id".into(),
        name: "Bob".into(),
    };
    client
        .command::<CreatePull>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: author.clone(),
                draft: false,
                title: "Review me".into(),
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
        .command::<SetReviewRequest>(
            repo,
            crate::app::mutation_identity().unwrap(),
            Selection {
                pull: 1,
                version: 1,
                actor: author,
                can_manage: true,
                reviewer: reviewer.clone(),
                requested: true,
            },
        )
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let before = h.json(&format!("{root}/pulls/1"), &alice).await;
    let mine = h
        .json(&format!("{root}/pulls?q=review-requested%3A%40me"), &bob)
        .await;
    assert_eq!(
        (
            mine["matching"].clone(),
            mine["review_requested_from_me"].clone()
        ),
        (json!(1), json!(true))
    );
    let _: Value = h
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
    for view in ["issues", "pulls"] {
        let page = h
            .json(
                &format!("{root}/{view}?q=review-requested%3APUBLIC-BOB"),
                &alice,
            )
            .await;
        assert_eq!(page["matching"], 1);
    }
    assert_eq!(h.json(&format!("{root}/pulls/1"), &alice).await, before);
    assert_eq!(
        h.json(&format!("{root}/pulls?q=commenter%3Apublic-bob"), &alice)
            .await["matching"],
        0
    );
    client
        .command::<CreatePullReview>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreatePullReviewInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                pull: 1,
                author: reviewer,
                body: "Review submitted".into(),
                state: ReviewState::Commented,
                commit_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        h.json(&format!("{root}/pulls?q=review-requested%3A%40me"), &bob)
            .await["matching"],
        0
    );
    assert_eq!(
        h.json(&format!("{root}/pulls?q=reviewed-by%3Apublic-bob"), &alice)
            .await["matching"],
        1
    );
    for query in [
        "commenter:PUBLIC-BOB",
        "commenter:@me",
        "reviewed-by:public-bob commenter:public-bob",
        "commenter:@me review-involves:@me",
    ] {
        let response: Value = h
            .http
            .get(format!("{}{root}/pulls", h.origin))
            .header(reqwest::header::COOKIE, &bob)
            .query(&[("q", query)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["matching"], 1, "{query}");
    }
    let completion = h
        .json(
            &format!("{root}/discussion-search?query=commenter%3APUB&cursor=13"),
            &alice,
        )
        .await;
    assert_eq!(completion["items"][0]["value"], "commenter:public-bob");
    assert!(
        !completion.to_string().contains("bob-id") && !completion.to_string().contains("issuer")
    );
    assert_eq!(
        h.json(
            &format!("{root}/pulls?q=author%3A%40me%20review-involves%3Apublic-bob"),
            &alice
        )
        .await["matching"],
        1
    );
    h.close().await;
}

#[tokio::test]
async fn reference_menu_choices_are_scoped_native_destinations_without_account_enrollment() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let path = "/api/repos/team/private/discussion-search/reviews";
    let mut url = url::Url::parse(&format!("{}{path}", h.origin)).unwrap();
    url.query_pairs_mut()
        .append_pair("query", "is:open -reviewed-by:@me label:Bug");
    let menu: Value = h
        .http
        .get(url)
        .header("cookie", &alice)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(menu["items"].as_array().unwrap().len(), 7);
    assert_eq!(
        menu["items"][2]["query"],
        "is:open label:Bug review:approved"
    );
    assert_eq!(menu["items"][5]["selected"], true);
    assert_eq!(
        h.json("/api/profile", &alice).await,
        json!({"profile":null})
    );
    let empty = h
        .json("/api/repos/team/private/pulls?q=review%3Aapproved", &alice)
        .await;
    assert_eq!(empty["matching"], 0);
    assert_eq!(
        h.http
            .get(format!("{}{path}?query=review%3Aunknown", h.origin))
            .header("cookie", &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    h.close().await;
}

async fn ready_review_search(h: &Harness, cookie: &str, view: &str, query: &str) -> Value {
    let mut url = url::Url::parse(&format!("{}/api/repos/team/private/{view}", h.origin)).unwrap();
    url.query_pairs_mut().append_pair("q", query);
    for _ in 0..150 {
        let response = h
            .http
            .get(url.clone())
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        if status == StatusCode::OK {
            return body;
        }
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("review_index_pending")
            ),
            "{body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("review reference projection did not become ready");
}
#[tokio::test]
async fn real_git_review_transitions_filter_both_list_kinds_with_consistent_rollups() {
    use crate::test_support::git;
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let root = "/api/repos/team/private";
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    h.write(Method::POST,&format!("{root}/pulls"),&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Review status search","body":"Full native query","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    let required = ready_review_search(&h, &alice, "pulls", "is:open review:required").await;
    assert_eq!(required["matching"], 1);
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for state in ["approved", "changes_requested"] {
        h.write(Method::POST,&format!("{root}/pulls/1/reviews"),&bob,json!({"request_id":uuid::Uuid::now_v7(),"state":state,"body":"Review feedback","commit_oid":head})).await.error_for_status().unwrap();
        for view in ["issues", "pulls"] {
            let result =
                ready_review_search(&h, &bob, view, &format!("is:pr review:{state}")).await;
            assert_eq!(result["matching"], 1);
            assert_eq!(result["counts"]["open"], 1);
            assert_eq!(result["items"][0]["rollup"]["head_oid"], head);
            assert_eq!(result["items"][0]["rollup"]["reviews"][state], 1);
        }
    }
    assert_eq!(
        ready_review_search(&h, &bob, "pulls", "review:approved").await["matching"],
        0
    );
    assert_eq!(
        ready_review_search(&h, &bob, "pulls", "reviewed-by:@me").await["matching"],
        1
    );
    std::fs::write(source.path().join("README.md"), "base\nfeature\nupdated\n").unwrap();
    git::success(source.path(), &["commit", "-am", "updated"]).await;
    let advanced = git::success(source.path(), &["rev-parse", "HEAD"]).await;
    git::success(source.path(), &["push", remote.as_str(), "feature"]).await;
    let blocked = h.json(&format!("{root}/pulls/1"), &alice).await;
    assert_eq!(blocked["merge_requirements"]["changes_requested"], 1);
    assert_eq!(blocked["merge_requirements"]["satisfied"], false);
    for query in ["review:changes_requested", "review:required"] {
        let result = ready_review_search(&h, &bob, "pulls", query).await;
        assert_eq!(
            result["matching"],
            if query == "review:required" { 0 } else { 1 }
        );
        if query == "review:changes_requested" {
            assert_eq!(result["items"][0]["rollup"]["head_oid"], advanced);
            assert_eq!(
                result["items"][0]["rollup"]["reviews"]["changes_requested"],
                1
            );
            assert_eq!(result["items"][0]["rollup"]["reviews"]["outdated"], 1);
        }
    }
    let carol = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    let CreatePullReviewOutcome::Created(carol_review) = carol
        .client
        .command::<CreatePullReview>(
            &carol.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullReviewInput {
                pull: 1,
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "carol-id".into(),
                    name: "Carol".into(),
                },
                state: ReviewState::Approved,
                body: String::new(),
                commit_oid: advanced.clone(),
            },
        )
        .await
        .unwrap()
        .output
    else {
        panic!("review rejected")
    };
    let blocked = h.json(&format!("{root}/pulls/1"), &alice).await;
    assert_eq!(blocked["merge_requirements"]["approvals"], 1);
    // Passing checks and another approval cannot resolve Bob's requested changes.
    h.write(
        Method::POST,
        &format!("{root}/statuses/{advanced}"),
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"context":"ci/test","state":"success"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let denied=h.write(Method::POST,&format!("{root}/pulls/1/merge"),&alice,json!({"request_id":uuid::Uuid::now_v7(),"version":blocked["version"],"method":"fast_forward","base_oid":blocked["base_oid"],"head_oid":advanced})).await;
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "merge_blocked"
    );
    h.write(Method::PUT,&format!("{root}/pulls/1/reviews/{}/dismissals",carol_review.number),&alice,json!({"request_id":uuid::Uuid::now_v7(),"version":carol_review.version,"body":"Remove temporary approval"})).await.error_for_status().unwrap();
    let review = h.json(&format!("{root}/pulls/1/reviews/2"), &alice).await;
    h.write(
        Method::PUT,
        &format!("{root}/pulls/1/reviews/2/dismissals"),
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"version":review["version"],"body":"Resolved"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        ready_review_search(&h, &bob, "pulls", "review:none").await["matching"],
        1
    );
    assert_eq!(
        ready_review_search(&h, &bob, "pulls", "review:required").await["matching"],
        1
    );
    h.close().await;
}
