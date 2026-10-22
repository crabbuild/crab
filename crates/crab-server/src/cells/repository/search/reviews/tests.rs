use super::super::index;
use super::*;
use crate::{
    cells::{RepositoryCell, application::Mutation, repository::*},
    test_support::server as fixtures,
};
use uuid::Uuid;

fn actor(issuer: &str, subject: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: issuer.into(),
        subject: subject.into(),
        name: subject.into(),
    }
}
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    cell.client
        .command::<M>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .unwrap()
        .output
}
async fn search(
    cell: &RepositoryCell,
    query: &str,
    reviewer: &RepositoryAuthor,
) -> index::Page<DiscussionSummary> {
    let parsed = Query::parse(Some(query)).unwrap();
    let reviewers = parsed
        .reviewers()
        .into_iter()
        .map(|name| Binding {
            name,
            identity: Some(people::identity(&reviewer.issuer, &reviewer.subject).unwrap()),
        })
        .collect();
    cell.client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                query: Some(query.into()),
                reviewers,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
}
async fn create_pull(cell: &RepositoryCell, author: &RepositoryAuthor, number: u64) {
    let outcome = write::<CreatePull>(
        cell,
        CreatePullInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: author.clone(),
            draft: false,
            title: format!("Pull {number}"),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: format!("refs/heads/feature-{number}"),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    assert!(matches!(outcome, CreatePullOutcome::Created(_)));
}
async fn request(
    cell: &RepositoryCell,
    pull: u64,
    author: &RepositoryAuthor,
    reviewer: &RepositoryAuthor,
    requested: bool,
) {
    let current = cell
        .client
        .query::<GetPull>(&cell.target, None, pull)
        .await
        .unwrap()
        .output
        .unwrap();
    let outcome = write::<pulls::review_requests::SetReviewRequest>(
        cell,
        pulls::review_requests::Selection {
            pull,
            version: current.version,
            actor: author.clone(),
            can_manage: true,
            reviewer: reviewer.clone(),
            requested,
        },
    )
    .await;
    assert!(matches!(
        outcome,
        pulls::review_requests::Outcome::Applied(_)
    ));
}
#[test]
fn reviewer_navigation_preserves_conditional_groups_and_native_personal_selection() {
    for (source, kind, handle, expected, mine) in [
        (
            "is:open review-requested:BOB",
            Kind::ReviewedBy,
            Some("bob"),
            "is:open reviewed-by:bob",
            false,
        ),
        (
            "(review-requested:bob OR label:bug) is:open",
            Kind::ReviewRequested,
            Some("@me"),
            "(review-requested:bob OR label:bug) is:open review-requested:@me",
            false,
        ),
        (
            "is:open user-review-requested:@me",
            Kind::ReviewRequested,
            None,
            "is:open",
            true,
        ),
        (
            "-review-requested:@me",
            Kind::ReviewRequested,
            Some("@me"),
            "review-requested:@me",
            false,
        ),
    ] {
        let query = Query::parse(Some(source)).unwrap();
        assert_eq!(
            query
                .reviewer_choice(kind, handle)
                .unwrap()
                .query
                .as_deref(),
            Some(expected)
        );
        assert_eq!(query.review_requested_from_me(), mine);
    }
    for source in [
        "review-requested:",
        "reviewed-by:@unknown",
        "review-involves:a,b",
        "user-review-requested:team/name",
    ] {
        assert!(Query::parse(Some(source)).is_err(), "{source}");
    }
    let query = Query::parse(Some("review-requested:@me")).unwrap();
    assert!(query.validate_reviewers(&[]).is_err());
    assert!(
        query
            .validate_reviewers(&[Binding {
                name: "@me".into(),
                identity: None
            }])
            .is_err()
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn request_review_cancel_replay_and_refresh_preserve_indexed_participation() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let alice = actor("https://one.test", "alice");
    let bob = actor("https://one.test", "bob");
    create_pull(&cell, &alice, 1).await;
    request(&cell, 1, &alice, &bob, true).await;
    assert_eq!(
        search(&cell, "review-requested:bob", &bob).await.matching,
        1
    );
    assert_eq!(search(&cell, "reviewed-by:bob", &bob).await.matching, 0);
    assert_eq!(
        search(
            &cell,
            "review-requested:bob",
            &actor("https://two.test", "bob")
        )
        .await
        .matching,
        0
    );
    request(&cell, 1, &alice, &bob, false).await;
    assert_eq!(search(&cell, "review-involves:bob", &bob).await.matching, 0);
    request(&cell, 1, &alice, &bob, true).await;
    let review = CreatePullReviewInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        pull: 1,
        author: bob.clone(),
        body: "Review comment".into(),
        state: ReviewState::Commented,
        commit_oid: "b".repeat(40),
    };
    write::<CreatePullReview>(&cell, review.clone()).await;
    assert_eq!(
        search(&cell, "review-requested:bob", &bob).await.matching,
        0
    );
    assert_eq!(search(&cell, "reviewed-by:bob", &bob).await.matching, 1);
    assert_eq!(search(&cell, "review-involves:bob", &bob).await.matching, 1);
    request(&cell, 1, &alice, &bob, true).await;
    write::<CreatePullReview>(&cell, review).await;
    assert_eq!(
        search(&cell, "user-review-requested:bob", &bob)
            .await
            .matching,
        1
    );
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    write::<UpdatePull>(
        &cell,
        UpdatePullInput {
            number: 1,
            actor: alice.clone(),
            can_triage: true,
            version: pull.version,
            title: Some("Refreshed title".into()),
            body: None,
            state: None,
            label_ids: None,
            assignees: None,
        },
    )
    .await;
    assert_eq!(
        search(&cell, "review-requested:bob reviewed-by:bob", &bob)
            .await
            .matching,
        1
    );
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn rebuild_pages_review_history_and_sparse_requests_without_changing_versions_or_submissions()
{
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let alice = actor("https://one.test", "alice");
    let bob = actor("https://one.test", "bob");
    for number in 1..=17 {
        create_pull(&cell, &alice, number).await;
        request(&cell, number, &alice, &bob, true).await;
        write::<CreatePullReview>(
            &cell,
            CreatePullReviewInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: number,
                author: bob.clone(),
                body: "Reviewed".into(),
                state: ReviewState::Commented,
                commit_oid: "b".repeat(40),
            },
        )
        .await;
        if number % 2 == 0 {
            request(&cell, number, &alice, &bob, true).await;
        }
    }
    let before = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output;
    let status = cell
        .client
        .query::<index::GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let index::SearchIndexStatus::Ready {
        generation: old, ..
    } = status
    else {
        panic!("not ready")
    };
    let generation = Uuid::now_v7();
    write::<index::RebuildIndex>(
        &cell,
        index::RebuildSearchInput::Begin {
            expected: Some(old),
            generation,
        },
    )
    .await;
    let mut review_steps = 0;
    let mut request_steps = 0;
    loop {
        let status = cell
            .client
            .query::<index::GetSearchIndexStatus>(&cell.target, None, ())
            .await
            .unwrap()
            .output;
        let index::SearchIndexStatus::Building { progress, .. } = status else {
            break;
        };
        if progress.phase == index::SearchIndexPhase::Reviews {
            review_steps += 1;
        }
        if progress.phase == index::SearchIndexPhase::ReviewRequests {
            request_steps += 1;
        }
        write::<index::RebuildIndex>(
            &cell,
            index::RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert!(review_steps >= 3 && request_steps >= 3);
    assert_eq!(search(&cell, "reviewed-by:bob", &bob).await.matching, 17);
    let pending = search(&cell, "review-requested:bob", &bob).await;
    assert_eq!((pending.matching, pending.counts.open), (8, 8));
    assert_eq!(
        cell.client
            .query::<GetPull>(&cell.target, None, 1)
            .await
            .unwrap()
            .output,
        before
    );
    fixtures::close(&server).await;
}
