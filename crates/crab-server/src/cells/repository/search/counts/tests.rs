use super::super::Query;
use crate::{
    cells::{RepositoryCell, application::Mutation, repository::*},
    test_support::server as fixtures,
};
use uuid::Uuid;

#[test]
fn count_grammar_accepts_integer_boundaries_and_rejects_ambiguous_or_overflowing_values() {
    for value in [
        "0",
        "0001",
        ">0",
        ">=1",
        "<0",
        "<=1",
        "1..1",
        "0..2",
        "0..*",
        "*..0",
        "9223372036854775807",
    ] {
        for qualifier in ["comments", "reactions", "interactions"] {
            let query = format!("-{qualifier}:{value} OR is:closed");
            assert!(Query::parse(Some(&query)).is_ok(), "{query}");
        }
    }
    for value in [
        "",
        "-1",
        "+1",
        "1.5",
        "1e2",
        "１２",
        "1_000",
        "*",
        "*..*",
        "..1",
        "1..",
        "2..1",
        "1..2..3",
        "1...2",
        ">=",
        "=>1",
        ">1..2",
        "1..=2",
        "9223372036854775808",
    ] {
        let query = format!("comments:{value}");
        assert!(Query::parse(Some(&query)).is_err(), "{query}");
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
    before: Option<String>,
    limit: u8,
) -> SearchPage {
    cell.client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                query: Some(query.into()),
                before,
                limit,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
}

type SearchPage = super::super::index::Page<DiscussionSummary>;

fn subjects(page: &SearchPage) -> Vec<(u8, u64)> {
    let mut items = page
        .items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => (0, issue.number),
            DiscussionSummary::Pull(pull) => (1, pull.number),
        })
        .collect::<Vec<_>>();
    items.sort_unstable();
    items
}

#[tokio::test(flavor = "multi_thread")]
async fn count_filters_share_live_issue_and_pull_projections_navigation_and_cursor_invalidation() {
    use collaboration::{Target, reactions};
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let actor = RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    };
    for number in 1..=2 {
        assert!(matches!(
            write::<CreateIssue>(
                &cell,
                CreateIssueInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor.clone(),
                    title: format!("Issue {number}"),
                    body: String::new(),
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
                    author: actor.clone(),
                    draft: false,
                    title: format!("Pull {number}"),
                    body: String::new(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: format!("refs/heads/feature-{number}"),
                    base_oid: "a".repeat(40),
                    head_oid: "b".repeat(40),
                }
            )
            .await,
            CreatePullOutcome::Created(_)
        ));
    }
    assert!(matches!(
        write::<CreateComment>(
            &cell,
            CreateCommentInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                issue: 2,
                author: actor.clone(),
                body: "Issue comment".into(),
            }
        )
        .await,
        CreateCommentOutcome::Created(_)
    ));
    for pull in 1..=2 {
        for _ in 0..pull {
            assert!(matches!(
                write::<CreatePullComment>(
                    &cell,
                    CreatePullCommentInput {
                        submission_id: *Uuid::now_v7().as_bytes(),
                        pull,
                        author: actor.clone(),
                        body: "Pull comment".into(),
                    }
                )
                .await,
                CreatePullCommentOutcome::Created(_)
            ));
        }
    }
    for (target, kind) in [
        (Target::Issue { number: 2 }, reactions::Kind::Heart),
        (Target::Issue { number: 2 }, reactions::Kind::ThumbsUp),
        (Target::Pull { number: 1 }, reactions::Kind::Heart),
    ] {
        assert!(matches!(
            write::<reactions::SetReaction>(
                &cell,
                reactions::Selection {
                    target,
                    kind,
                    active: true,
                    actor: actor.clone(),
                }
            )
            .await,
            reactions::Outcome::Applied(_)
        ));
    }
    for (query, expected) in [
        ("comments:0", vec![(0, 1)]),
        ("comments:1", vec![(0, 2), (1, 1)]),
        ("comments:>1", vec![(1, 2)]),
        ("comments:>=1", vec![(0, 2), (1, 1), (1, 2)]),
        ("comments:<1", vec![(0, 1)]),
        ("comments:<=1", vec![(0, 1), (0, 2), (1, 1)]),
        ("comments:1..2", vec![(0, 2), (1, 1), (1, 2)]),
        ("comments:*..0", vec![(0, 1)]),
        ("comments:2..*", vec![(1, 2)]),
        ("comments:<0", vec![]),
        ("comments:>9223372036854775807", vec![]),
        ("-reactions:>0", vec![(0, 1), (1, 2)]),
        ("reactions:1", vec![(1, 1)]),
        ("reactions:2", vec![(0, 2)]),
        ("interactions:2", vec![(1, 1), (1, 2)]),
        ("interactions:3", vec![(0, 2)]),
        (
            "(interactions:2 OR reactions:2) -comments:2",
            vec![(0, 2), (1, 1)],
        ),
        ("is:issue interactions:>0", vec![(0, 2)]),
        ("is:pr comments:>0", vec![(1, 1), (1, 2)]),
    ] {
        let page = search(&cell, query, None, 30).await;
        assert_eq!(subjects(&page), expected, "{query}");
        assert_eq!(page.matching, expected.len() as u64, "{query}");
    }
    let first = search(&cell, "comments:>=1", None, 1).await;
    let mut seen = subjects(&first);
    let mut cursor = first.next.clone();
    while let Some(before) = cursor {
        let page = search(&cell, "comments:>=1", Some(before), 1).await;
        seen.extend(subjects(&page));
        cursor = page.next;
    }
    seen.sort_unstable();
    assert_eq!(seen, [(0, 2), (1, 1), (1, 2)]);
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                SearchInput {
                    query: Some("comments:>1".into()),
                    before: first.next.clone(),
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap(),
        crate::cells::Error::StaleSearch
    ));
    assert!(matches!(
        write::<reactions::SetReaction>(
            &cell,
            reactions::Selection {
                target: Target::Issue { number: 2 },
                kind: reactions::Kind::Heart,
                active: false,
                actor: actor.clone(),
            }
        )
        .await,
        reactions::Outcome::Applied(_)
    ));
    assert_eq!(
        subjects(&search(&cell, "interactions:2", None, 30).await),
        [(0, 2), (1, 1), (1, 2)]
    );
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                SearchInput {
                    query: Some("comments:>=1".into()),
                    before: first.next,
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap(),
        crate::cells::Error::StaleSearch
    ));
    assert!(matches!(
        write::<UpdateIssue>(
            &cell,
            UpdateIssueInput {
                number: 2,
                actor,
                can_triage: true,
                version: 1,
                title: None,
                body: None,
                state: Some(1),
                label_ids: None,
                assignees: None,
            }
        )
        .await,
        UpdateIssueOutcome::Updated(_)
    ));
    let page = search(&cell, "is:open interactions:2", None, 30).await;
    assert_eq!(
        (page.matching, page.counts.open, page.counts.closed),
        (2, 2, 1)
    );
    let super::super::navigation::StateNavigation::State { closed, .. } = page.navigation else {
        panic!("expected state navigation")
    };
    assert_eq!(
        subjects(&search(&cell, closed.as_deref().unwrap(), None, 30).await),
        [(0, 2)]
    );
    fixtures::close(&server).await;
}
