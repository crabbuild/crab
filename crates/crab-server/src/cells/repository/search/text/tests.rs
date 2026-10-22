use crate::{
    cells::repository::search::{Query, index, navigation, people},
    cells::{RepositoryCell, application::Mutation, repository::*},
    test_support::server as fixtures,
};
use uuid::Uuid;

fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "UnsearchableRecordedAuthor".into(),
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
fn input(source: &str) -> SearchInput {
    let actor = actor();
    let people = Query::parse(Some(source))
        .unwrap()
        .people()
        .into_iter()
        .map(|name| people::Binding {
            name,
            identity: Some(people::identity(&actor.issuer, &actor.subject).unwrap()),
        })
        .collect();
    SearchInput {
        query: Some(source.into()),
        people,
        ..Default::default()
    }
}
async fn page(cell: &RepositoryCell, source: &str) -> index::Page<DiscussionSummary> {
    cell.client
        .query::<ListDiscussions>(&cell.target, None, input(source))
        .await
        .unwrap()
        .output
}
fn subjects(page: &index::Page<DiscussionSummary>) -> Vec<(u8, u64)> {
    let mut result = page
        .items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => (0, issue.number),
            DiscussionSummary::Pull(pull) => (1, pull.number),
        })
        .collect::<Vec<_>>();
    result.sort_unstable();
    result
}

#[test]
fn field_grammar_normalizes_combinations_and_preserves_scope_through_navigation() {
    for source in [
        "in:",
        "in:author",
        "in:title,",
        "in:,body",
        "in:title,,body",
        "in:title/body",
        "-in:title",
    ] {
        assert!(Query::parse(Some(source)).is_err(), "{source}");
    }
    for source in [
        "needle in:TITLE,body",
        "in:body in:title needle",
        "(needle in:title) OR (other in:comments)",
        "in:title (needle in:body)",
    ] {
        let query = Query::parse(Some(source)).unwrap();
        let rendered = navigation::source(&query.expression, 0);
        let reparsed = Query::parse(Some(&rendered)).unwrap();
        assert_eq!(
            serde_json::to_value(&query.expression).unwrap(),
            serde_json::to_value(&reparsed.expression).unwrap(),
            "{source}"
        );
    }
    let query = Query::parse(Some("is:open author:@me needle in:comments")).unwrap();
    assert!(query.authored_by_me());
    let assignees = Query::parse(Some("assignee:alice needle in:comments")).unwrap();
    assert_eq!(
        assignees.assignees().into_iter().collect::<Vec<_>>(),
        ["alice"]
    );
    assert!(assignees.validate_assignees(&[]).is_err());

    let choice = query.author_choice(None).unwrap();
    assert_eq!(
        choice.query.as_deref(),
        Some("(is:open needle in:comments)")
    );
    let query = Query::parse(choice.query.as_deref()).unwrap();
    let (_, navigation::StateNavigation::State { closed, .. }) = query.state_navigation() else {
        panic!("expected state navigation")
    };
    assert!(closed.unwrap().contains("needle in:comments"));
}

#[tokio::test(flavor = "multi_thread")]
async fn field_search_covers_all_contribution_bodies_and_keeps_edits_deletion_and_ranking_current()
{
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    assert!(matches!(
        write::<CreateIssue>(
            &cell,
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Titleneedle".into(),
                body: "Bodyneedle".into(),
            }
        )
        .await,
        CreateIssueOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePull>(
            &cell,
            CreatePullInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Pull title".into(),
                body: "Pull body".into(),
                draft: false,
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            }
        )
        .await,
        CreatePullOutcome::Created(_)
    ));
    for body in ["Commentneedle sharedneedle ÉCOLE 🦀船舶 alpha", "beta"] {
        assert!(matches!(
            write::<CreateComment>(
                &cell,
                CreateCommentInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    issue: 1,
                    author: actor(),
                    body: body.into(),
                }
            )
            .await,
            CreateCommentOutcome::Created(_)
        ));
    }
    assert!(matches!(
        write::<CreatePullComment>(
            &cell,
            CreatePullCommentInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: 1,
                author: actor(),
                body: "Pullcommentneedle sharedneedle".into(),
            }
        )
        .await,
        CreatePullCommentOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePullReview>(
            &cell,
            CreatePullReviewInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: 1,
                author: actor(),
                body: "Reviewneedle".into(),
                state: ReviewState::Commented,
                commit_oid: "b".repeat(40),
            }
        )
        .await,
        CreatePullReviewOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePullReviewThread>(
            &cell,
            CreatePullReviewThreadInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: 1,
                author: actor(),
                body: "Threadneedle".into(),
                suggested_text: None,
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
                path: b"file.rs".to_vec(),
                old_blob_oid: None,
                new_blob_oid: Some("c".repeat(40)),
                side: PullReviewThreadSide::New,
                start_line: 1,
                end_line: 1,
            }
        )
        .await,
        CreatePullReviewThreadOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePullReviewReply>(
            &cell,
            CreatePullReviewReplyInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: 1,
                thread: 1,
                author: actor(),
                body: "Replyneedle".into(),
            }
        )
        .await,
        CreatePullReviewReplyOutcome::Created(_)
    ));
    for (query, expected) in [
        ("titleneedle", vec![(0, 1)]),
        ("bodyneedle in:body", vec![(0, 1)]),
        ("bodyneedle in:title", vec![]),
        ("titleneedle in:title,body", vec![(0, 1)]),
        ("titleneedle in:body in:title", vec![(0, 1)]),
        ("commentneedle in:comments is:issue", vec![(0, 1)]),
        ("pullcommentneedle", vec![(1, 1)]),
        ("reviewneedle in:comments", vec![(1, 1)]),
        ("threadneedle in:comments", vec![(1, 1)]),
        ("replyneedle in:comments", vec![(1, 1)]),
        ("sharedneedle", vec![(0, 1), (1, 1)]),
        ("sharedneedle in:title,body", vec![]),
        ("\"école\" in:comments", vec![(0, 1)]),
        ("🦀船 in:comments", vec![(0, 1)]),
        ("🦀 in:comments", vec![(0, 1)]),
        ("alpha beta in:comments", vec![(0, 1)]),
        ("\"alpha beta\" in:comments", vec![]),
        (
            "(titleneedle in:title) OR (replyneedle in:comments)",
            vec![(0, 1), (1, 1)],
        ),
        (
            "(titleneedle in:comments) OR (replyneedle in:title)",
            vec![],
        ),
        ("in:title (bodyneedle in:body)", vec![(0, 1)]),
        ("UnsearchableRecordedAuthor", vec![]),
        ("author:@me sharedneedle in:comments", vec![(0, 1), (1, 1)]),
    ] {
        let result = page(&cell, query).await;
        assert_eq!(subjects(&result), expected, "{query}");
        assert_eq!(result.matching, expected.len() as u64, "{query}");
    }
    let first = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                limit: 1,
                sort: index::Sort::Relevance,
                ..input("sharedneedle in:comments")
            },
        )
        .await
        .unwrap()
        .output;
    let next = first.next.clone().unwrap();
    let last = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                limit: 1,
                sort: index::Sort::Relevance,
                before: Some(next.clone()),
                ..input("sharedneedle in:comments")
            },
        )
        .await
        .unwrap()
        .output;
    let mut seen = subjects(&first);
    seen.extend(subjects(&last));
    seen.sort_unstable();
    assert_eq!(seen, [(0, 1), (1, 1)]);
    assert!(last.next.is_none());
    assert!(matches!(
        write::<UpdateComment>(
            &cell,
            UpdateCommentInput {
                key: CommentKey {
                    issue: 1,
                    number: 1
                },
                actor: actor(),
                version: 1,
                body: "replacementissueword".into(),
            }
        )
        .await,
        UpdateCommentOutcome::Updated(_)
    ));
    assert!(matches!(
        write::<UpdatePullComment>(
            &cell,
            UpdatePullCommentInput {
                key: PullChildKey { pull: 1, number: 1 },
                actor: actor(),
                version: 1,
                body: "replacementpullword".into(),
            }
        )
        .await,
        UpdatePullCommentOutcome::Updated(_)
    ));
    assert!(matches!(
        write::<UpdatePullReview>(
            &cell,
            UpdatePullReviewInput {
                key: PullChildKey { pull: 1, number: 1 },
                actor: actor(),
                version: 1,
                body: "replacementreviewword".into(),
            }
        )
        .await,
        UpdatePullReviewOutcome::Updated(_)
    ));
    assert!(matches!(
        write::<UpdatePullReviewThread>(
            &cell,
            UpdatePullReviewThreadInput {
                key: PullReviewThreadKey { pull: 1, number: 1 },
                actor: actor(),
                version: 1,
                can_resolve: true,
                body: Some("replacementthreadword".into()),
                suggested_text: None,
                resolved: None,
            }
        )
        .await,
        UpdatePullReviewThreadOutcome::Updated(_)
    ));
    assert!(matches!(
        write::<UpdatePullReviewReply>(
            &cell,
            UpdatePullReviewReplyInput {
                key: PullReviewReplyKey {
                    pull: 1,
                    thread: 1,
                    number: 1
                },
                actor: actor(),
                version: 1,
                body: "replacementreplyword".into(),
            }
        )
        .await,
        UpdatePullReviewReplyOutcome::Updated(_)
    ));
    for old in [
        "commentneedle",
        "pullcommentneedle",
        "reviewneedle",
        "threadneedle",
        "replyneedle",
        "🦀",
    ] {
        assert_eq!(
            page(&cell, &format!("{old} in:comments")).await.matching,
            0,
            "{old}"
        );
    }
    for (new, expected) in [
        ("replacementissueword", (0, 1)),
        ("replacementpullword", (1, 1)),
        ("replacementreviewword", (1, 1)),
        ("replacementthreadword", (1, 1)),
        ("replacementreplyword", (1, 1)),
    ] {
        assert_eq!(
            subjects(&page(&cell, &format!("{new} in:comments")).await),
            [expected]
        );
    }
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                SearchInput {
                    limit: 1,
                    sort: index::Sort::Relevance,
                    before: Some(next),
                    ..input("sharedneedle in:comments")
                }
            )
            .await
            .err()
            .unwrap(),
        crate::cells::Error::StaleSearch
    ));
    fixtures::close(&server).await;
}
