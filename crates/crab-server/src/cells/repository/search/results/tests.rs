use super::*;
use crate::cells::application::Mutation;
use crate::cells::repository::search::index::{Direction, Sort};
use crate::cells::repository::*;
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::test_support::server as fixtures;
use uuid::Uuid;

async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    let identity: MutationIdentity = crate::app::mutation_identity().unwrap();
    cell.client
        .command::<M>(&cell.target, identity, input)
        .await
        .unwrap()
        .output
}

async fn seed(cell: &RepositoryCell) {
    let actor = RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    };
    for number in 1..=3 {
        assert!(matches!(
            write::<CreateIssue>(
                cell,
                CreateIssueInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor.clone(),
                    title: "Shared searchable title".into(),
                    body: "Shared searchable body".into(),
                }
            )
            .await,
            CreateIssueOutcome::Created(_)
        ));
        assert!(matches!(
            write::<CreatePull>(
                cell,
                CreatePullInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor.clone(),
                    draft: number == 2,
                    title: "Shared searchable title".into(),
                    body: "Shared searchable body".into(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: format!("refs/heads/topic-{number}"),
                    base_oid: "a".repeat(40),
                    head_oid: format!("{number:040x}"),
                }
            )
            .await,
            CreatePullOutcome::Created(_)
        ));
    }
}

fn subjects(page: &Page<DiscussionSummary>) -> Vec<(SubjectKind, u64)> {
    page.items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => (SubjectKind::Issue, issue.number),
            DiscussionSummary::Pull(pull) => (SubjectKind::Pull, pull.number),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn colliding_numbers_paginate_without_skips_for_tied_sorts_in_both_directions() {
    let server = fixtures::fixture().await;
    let cell = server
        .repository_cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let ascending = (1..=3)
        .flat_map(|number| [(SubjectKind::Issue, number), (SubjectKind::Pull, number)])
        .collect::<Vec<_>>();
    for sort in [
        Sort::Comments,
        Sort::Reactions,
        Sort::ThumbsUp,
        Sort::ThumbsDown,
        Sort::Laugh,
        Sort::Hooray,
        Sort::Confused,
        Sort::Heart,
        Sort::Rocket,
        Sort::Eyes,
        Sort::Relevance,
    ] {
        for direction in [Direction::Asc, Direction::Desc] {
            let mut input = SearchInput {
                limit: 1,
                sort,
                direction,
                query: Some("searchable".into()),
                ..Default::default()
            };
            let mut seen = Vec::new();
            for position in 0..6 {
                let page = cell
                    .client
                    .query::<ListDiscussions>(&cell.target, None, input.clone())
                    .await
                    .unwrap()
                    .output;
                assert_eq!(page.matching, 6);
                seen.extend(subjects(&page));
                assert_eq!(page.next.is_some(), position != 5);
                input.before = page.next;
            }
            let expected = if direction == Direction::Asc {
                ascending.clone()
            } else {
                ascending.iter().rev().copied().collect()
            };
            assert_eq!(seen, expected, "{sort:?} {direction:?}");
        }
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_query_kinds_counts_and_scoped_cursors_share_the_authoritative_snapshot() {
    let server = fixtures::fixture().await;
    let cell = server
        .repository_cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for (query, expected) in [
        ("is:issue", 3),
        ("is:pr", 3),
        ("draft:true", 1),
        ("is:issue OR draft:true", 4),
        ("is:issue is:pr", 0),
    ] {
        let page = cell
            .client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                SearchInput {
                    query: Some(query.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .output;
        assert_eq!(
            (page.matching, page.counts.open, page.items.len()),
            (expected, expected, expected as usize),
            "{query}"
        );
    }
    let input = SearchInput {
        limit: 1,
        sort: Sort::Comments,
        ..Default::default()
    };
    let page = cell
        .client
        .query::<ListDiscussions>(&cell.target, None, input.clone())
        .await
        .unwrap()
        .output;
    let error = cell
        .client
        .query::<ListPulls>(
            &cell.target,
            None,
            SearchInput {
                before: page.next,
                ..input
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(error, crate::cells::Error::StaleSearch));
    let pulls = cell
        .client
        .query::<ListPulls>(&cell.target, None, SearchInput::default())
        .await
        .unwrap()
        .output;
    assert_eq!(pulls.matching, 3);
    fixtures::close(&server).await;
}
