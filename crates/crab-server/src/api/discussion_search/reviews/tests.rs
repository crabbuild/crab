use crate::{
    cells::repository::{
        review_requests::{Selection, SetReviewRequest},
        *,
    },
    test_support::http::Harness,
};
use reqwest::Method;
use serde_json::{Value, json};
#[tokio::test]
async fn reviewer_filters_resolve_public_handles_and_current_accounts_and_expose_native_menu_choices()
 {
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
    for view in ["issues", "pulls"] {
        let page = h
            .json(
                &format!("{root}/{view}?q=review-requested%3APUBLIC-BOB"),
                &alice,
            )
            .await;
        assert_eq!(page["matching"], 1);
    }
    let options=h.json(&format!("{root}/discussion-search/reviewers?query=is%3Aopen%20-review-requested%3Aother&search=%40PUBLIC&relation=reviewed_by"),&alice).await;
    assert_eq!(options["items"][0]["actor"]["id"], profile["id"]);
    assert_eq!(
        options["items"][0]["query"],
        "is:open reviewed-by:public-bob"
    );
    assert_eq!(options["self"]["query"], "is:open reviewed-by:@me");
    assert!(!options.to_string().contains("bob-id") && !options.to_string().contains("issuer"));
    assert_eq!(h.json(&format!("{root}/pulls/1"), &alice).await, before);
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
