use super::*;
use crate::cells::RepositoryCell;
use crate::cells::repository::collaboration::{Target, reactions};
use crate::cells::repository::*;
use crate::test_support::server as fixtures;

fn cell(server: &crate::server::Server) -> RepositoryCell {
    server
        .repository_cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}
fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
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
fn issue(title: &str, body: &str) -> CreateIssueInput {
    CreateIssueInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        title: title.into(),
        body: body.into(),
    }
}
async fn create(cell: &RepositoryCell, title: &str, body: &str) {
    assert!(matches!(
        write::<CreateIssue>(cell, issue(title, body)).await,
        CreateIssueOutcome::Created(_)
    ));
}
async fn page(cell: &RepositoryCell, input: SearchInput) -> Page<IssueSummary> {
    cell.client
        .query::<ListIssues>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
fn numbers(page: &Page<IssueSummary>) -> Vec<u64> {
    page.items.iter().map(|item| item.number).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn counts_and_matches_cover_large_bodies_beyond_the_old_scan_limit() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    create(&cell, "Needle in oldest issue", "archive").await;
    let body = "ordinary ".repeat(1600);
    for _ in 0..204 {
        create(&cell, "Ordinary issue", &body).await;
    }
    let all = page(
        &cell,
        SearchInput {
            limit: 50,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(all.counts.open, 205);
    assert_eq!(all.matching, 205);
    assert_eq!(all.items.len(), 50);
    let found = page(
        &cell,
        SearchInput {
            query: Some("needle".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(numbers(&found), [1]);
    assert_eq!(found.matching, 1);
    assert!(found.next.is_none());
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tied_sort_keys_paginate_both_directions_and_bind_cursors_to_the_query() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for _ in 0..6 {
        create(&cell, "Searchable issue", "searchable body").await;
    }
    for sort in [
        Sort::Created,
        Sort::Updated,
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
                limit: 2,
                sort,
                direction,
                query: Some("searchable".into()),
                ..Default::default()
            };
            let mut seen = Vec::new();
            for index in 0..3 {
                let result = page(&cell, input.clone()).await;
                seen.extend(numbers(&result));
                assert_eq!(result.next.is_some(), index != 2, "{sort:?} {direction:?}");
                input.before = result.next;
            }
            let expected = if direction == Direction::Asc {
                (1..=6).collect::<Vec<_>>()
            } else {
                (1..=6).rev().collect()
            };
            assert_eq!(seen, expected, "{sort:?} {direction:?}");
        }
    }
    let first = page(
        &cell,
        SearchInput {
            limit: 2,
            ..Default::default()
        },
    )
    .await;
    for input in [
        SearchInput {
            query: Some("searchable".into()),
            ..Default::default()
        },
        SearchInput {
            sort: Sort::Comments,
            ..Default::default()
        },
        SearchInput {
            direction: Direction::Asc,
            ..Default::default()
        },
        SearchInput {
            state: 2,
            ..Default::default()
        },
    ] {
        let error = cell
            .client
            .query::<ListIssues>(
                &cell.target,
                None,
                SearchInput {
                    before: first.next.clone(),
                    ..input
                },
            )
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error, crate::cells::Error::StaleSearch),
            "{error:?}"
        );
    }
    create(&cell, "New mutation", "").await;
    let error = cell
        .client
        .query::<ListIssues>(
            &cell.target,
            None,
            SearchInput {
                before: first.next,
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, crate::cells::Error::StaleSearch),
        "{error:?}"
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn comments_reactions_and_rejected_edits_keep_index_totals_consistent() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for _ in 0..2 {
        create(&cell, "Discussion", "").await;
    }
    assert!(matches!(
        write::<CreateComment>(
            &cell,
            CreateCommentInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                issue: 1,
                author: actor(),
                body: "Comment".into()
            }
        )
        .await,
        CreateCommentOutcome::Created(_)
    ));
    for kind in [reactions::Kind::Heart, reactions::Kind::ThumbsUp] {
        assert!(matches!(
            write::<reactions::SetReaction>(
                &cell,
                reactions::Selection {
                    target: Target::Issue { number: 1 },
                    kind,
                    active: true,
                    actor: actor()
                }
            )
            .await,
            reactions::Outcome::Applied(_)
        ));
    }
    for sort in [Sort::Comments, Sort::Reactions, Sort::Heart, Sort::ThumbsUp] {
        let result = page(
            &cell,
            SearchInput {
                sort,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(numbers(&result), [1, 2]);
        assert_eq!(result.items[0].comment_count, 1);
        assert_eq!(result.items[0].reaction_count, 2);
    }
    let rejected = write::<UpdateIssue>(
        &cell,
        UpdateIssueInput {
            number: 1,
            actor: actor(),
            can_manage_metadata: true,
            version: 99,
            title: Some("Wrong".into()),
            body: None,
            state: Some(1),
            label_ids: None,
            assignee_subjects: None,
        },
    )
    .await;
    assert!(matches!(rejected, UpdateIssueOutcome::Conflict));
    let result = page(&cell, SearchInput::default()).await;
    assert_eq!(result.counts.open, 2);
    assert!(result.items.iter().all(|item| item.title == "Discussion"));
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_rebuild_blocks_fresh_writes_preserves_replays_and_invalidates_cursors() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let request = crate::app::mutation_identity().unwrap();
    let input = issue("Durable issue", "Needle");
    cell.client
        .command::<CreateIssue>(&cell.target, request, input.clone())
        .await
        .unwrap();
    for _ in 0..10 {
        create(&cell, "Other", "").await;
    }
    let old = page(
        &cell,
        SearchInput {
            limit: 2,
            ..Default::default()
        },
    )
    .await;
    let current = cell
        .client
        .query::<GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let SearchIndexStatus::Ready {
        generation: previous,
        ..
    } = current
    else {
        panic!("not ready")
    };
    let generation = Uuid::now_v7();
    let begun = write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(previous),
            generation,
        },
    )
    .await;
    assert!(matches!(
        begun,
        RebuildSearchOutcome::Progress {
            status: SearchIndexStatus::Building { .. }
        }
    ));
    let blocked = cell
        .client
        .command::<CreateIssue>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            issue("Blocked", ""),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(blocked, crate::cells::Error::IndexUnavailable),
        "{blocked:?}"
    );
    assert!(matches!(
        cell.client
            .command::<CreateIssue>(&cell.target, request, input)
            .await
            .unwrap()
            .output,
        CreateIssueOutcome::Created(_)
    ));
    let mut steps = 0;
    loop {
        let status = cell
            .client
            .query::<GetSearchIndexStatus>(&cell.target, None, ())
            .await
            .unwrap()
            .output;
        let SearchIndexStatus::Building { progress, .. } = status else {
            break;
        };
        let input = RebuildSearchInput::Step {
            generation,
            version: progress.version,
        };
        let result = write::<RebuildIndex>(&cell, input.clone()).await;
        let retry = write::<RebuildIndex>(&cell, input).await;
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::to_value(retry).unwrap()
        );
        steps += 1;
        assert!(steps < 20);
    }
    assert!(steps > 3);
    let ready = page(&cell, SearchInput::default()).await;
    assert_eq!(ready.matching, 11);
    assert!(matches!(
        cell.client
            .query::<ListIssues>(
                &cell.target,
                None,
                SearchInput {
                    before: old.next,
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap(),
        crate::cells::Error::StaleSearch
    ));
    let found = page(
        &cell,
        SearchInput {
            query: Some("needle".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(numbers(&found), [1]);
    fixtures::close(&server).await;
}
