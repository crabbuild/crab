use super::*;
use crate::cells::RepositoryCell;
use crate::cells::repository::collaboration::{Target, reactions};
use crate::cells::repository::*;
use crate::test_support::server as fixtures;

fn cell(server: &crate::server::Server) -> RepositoryCell {
    server
        .cells()
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
        metadata: Default::default(),
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
async fn page(cell: &RepositoryCell, input: SearchInput) -> Page<DiscussionSummary> {
    cell.client
        .query::<ListDiscussions>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
fn numbers(page: &Page<DiscussionSummary>) -> Vec<u64> {
    page.items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => issue.number,
            DiscussionSummary::Pull(pull) => pull.number,
        })
        .collect()
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
            query: Some("is:closed".into()),
            ..Default::default()
        },
    ] {
        let error = cell
            .client
            .query::<ListDiscussions>(
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
        .query::<ListDiscussions>(
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
                can_mark_duplicate: false,
                can_post_locked: false,
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
        let DiscussionSummary::Issue(issue) = &result.items[0] else {
            panic!("expected issue")
        };
        assert_eq!(issue.comment_count, 1);
        assert_eq!(issue.reaction_count, 2);
    }
    let rejected = write::<UpdateIssue>(
        &cell,
        UpdateIssueInput {
            state_reason: None,
            number: 1,
            actor: actor(),
            can_triage: true,
            can_moderate: false,
            version: 99,
            title: Some("Wrong".into()),
            body: None,
            state: Some(1),
            label_ids: None,
            assignees: None,
        },
    )
    .await;
    assert!(matches!(rejected, UpdateIssueOutcome::Conflict));
    let result = page(&cell, SearchInput::default()).await;
    assert_eq!(result.counts.open, 2);
    assert!(result.items.iter().all(
        |item| matches!(item, DiscussionSummary::Issue(issue) if issue.title == "Discussion")
    ));
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
        // Conversation preparation adds separate bounded source frames.
        assert!(steps < 32, "the small fixture did not finish rebuilding");
    }
    assert!(steps > 3);
    let ready = page(&cell, SearchInput::default()).await;
    assert_eq!(ready.matching, 11);
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
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

#[tokio::test(flavor = "multi_thread")]
async fn state_counters_equal_their_navigation_queries_across_boolean_and_literal_filters() {
    use super::super::navigation::{StateNavigation, StateSelection};
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for (title, body) in [
        ("Keyboard focus", "AND"),
        ("Color contrast", "AND"),
        ("Other", ""),
    ] {
        create(&cell, title, body).await;
    }
    let closed = write::<UpdateIssue>(
        &cell,
        UpdateIssueInput {
            state_reason: None,
            number: 2,
            actor: actor(),
            can_triage: true,
            can_moderate: false,
            version: 1,
            title: None,
            body: None,
            state: Some(1),
            label_ids: None,
            assignees: None,
        },
    )
    .await;
    assert!(matches!(closed, UpdateIssueOutcome::Updated(_)));
    for (query, expected, open, closed, selected) in [
        (
            "is:closed",
            vec![2],
            vec![3, 1],
            vec![2],
            Some(StateSelection::Closed),
        ),
        (
            "keyboard is:closed",
            vec![],
            vec![1],
            vec![],
            Some(StateSelection::Closed),
        ),
        (
            "(keyboard is:open) OR (contrast is:closed)",
            vec![2, 1],
            vec![1],
            vec![2],
            None,
        ),
        (
            "is:open OR contrast",
            vec![3, 2, 1],
            vec![3, 1],
            vec![2],
            None,
        ),
        ("-is:closed \"AND\"", vec![1], vec![1], vec![], None),
        ("\"is:closed\"", vec![], vec![], vec![], None),
        (
            "is:open OR contrast",
            vec![3, 2, 1],
            vec![3, 1],
            vec![2],
            None,
        ),
        (
            "is:closed (is:open OR contrast)",
            vec![2],
            vec![],
            vec![2],
            None,
        ),
        ("no:label -is:open", vec![2], vec![], vec![2], None),
        (
            "\"AND\" AND (keyboard OR contrast)",
            vec![2, 1],
            vec![1],
            vec![2],
            None,
        ),
    ] {
        let result = page(
            &cell,
            SearchInput {
                query: Some(query.into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(numbers(&result), expected, "{query}");
        assert_eq!(result.matching, expected.len() as u64, "{query}");
        match result.navigation {
            StateNavigation::State {
                selected: actual,
                open: open_query,
                closed: closed_query,
            } => {
                assert_eq!(Some(actual), selected, "{query}");
                for (target, expected, count) in [
                    (open_query, open, result.counts.open),
                    (closed_query, closed, result.counts.closed),
                ] {
                    let result = page(
                        &cell,
                        SearchInput {
                            query: Some(target.unwrap()),
                            ..Default::default()
                        },
                    )
                    .await;
                    assert_eq!(numbers(&result), expected, "{query}");
                    assert_eq!(result.matching, count, "{query}");
                }
            }
            StateNavigation::Results => {
                assert!(selected.is_none(), "{query}");
                assert_eq!(result.counts.open, open.len() as u64, "{query}");
                assert_eq!(result.counts.closed, closed.len() as u64, "{query}");
            }
        }
    }
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct IndexFixture;

#[derive(Deserialize, Serialize)]
pub(in crate::cells::repository) enum IndexFixtureInput {
    Format14,
    Format15,
    ShortDeadline { number: u64 },
}

impl Mutation for IndexFixture {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 985;
    type Input = IndexFixtureInput;
    type Output = Option<u64>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        let deadline = match input {
            IndexFixtureInput::Format15 => {
                let mut current = state(context)?.unwrap();
                current.format = 15;
                context.put_document(STATE_KEY, &current)?;
                context.install_schema("DROP TABLE repository_issue_type_index;")?;
                None
            }
            IndexFixtureInput::Format14 => {
                let mut current = state(context)?.unwrap();
                current.format = 14;
                context.put_document(STATE_KEY, &current)?;
                context.install_schema("DROP TABLE discussion_index_locks;")?;
                None
            }
            IndexFixtureInput::ShortDeadline { number } => {
                let subject = Subject {
                    kind: SubjectKind::Issue,
                    number,
                };
                let key = format!("collaboration/v1/locks/{}", subject.key());
                let mut lock = context.document::<serde_json::Value>(&key)?.unwrap();
                let deadline = timestamp(context.now_ms())? + 5_000;
                lock["deadline"]["at"] = serde_json::json!(deadline);
                context.put_document(&key, &lock)?;
                locks::project(context, subject, serde_json::from_value(lock)?)?;
                Some(deadline)
            }
        };
        Ok(MutationResult::Success(deadline))
    }
}

fn lock(
    number: u64,
    duration: Option<collaboration::locks::Duration>,
) -> collaboration::locks::Input {
    collaboration::locks::Input {
        subject: Subject {
            kind: SubjectKind::Issue,
            number,
        },
        actor: actor(),
        version: 0,
        change: collaboration::locks::Change::Lock {
            reason: None,
            duration,
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn temporary_locks_expire_in_enforcement_and_search_and_invalidate_continuations() {
    use collaboration::locks::{Duration, Get, Outcome, Set};
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for title in ["Temporary", "Permanent", "Never locked"] {
        create(&cell, title, "").await;
    }
    let temporary = || lock(1, Some(Duration::Hour));
    let Outcome::Saved(first, false) = write::<Set>(&cell, temporary()).await else {
        panic!("lock failed")
    };
    let Outcome::Saved(recovered, false) = write::<Set>(&cell, temporary()).await else {
        panic!("lock replay failed")
    };
    assert_eq!(first.expires_at_ms(), recovered.expires_at_ms());
    assert!(matches!(
        write::<Set>(&cell, lock(2, None)).await,
        Outcome::Saved(_, false)
    ));

    let deadline = write::<IndexFixture>(&cell, IndexFixtureInput::ShortDeadline { number: 1 })
        .await
        .unwrap();
    let comment = || CreateCommentInput {
        can_mark_duplicate: false,
        can_post_locked: false,
        issue: 1,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        body: "Comment after expiration".into(),
    };
    assert!(matches!(
        write::<CreateComment>(&cell, comment()).await,
        CreateCommentOutcome::Locked
    ));
    let search = SearchInput {
        limit: 1,
        query: Some("is:locked is:open".into()),
        ..Default::default()
    };
    let first_page = page(&cell, search.clone()).await;
    assert_eq!(first_page.counts.open, 2);
    assert_eq!(numbers(&first_page), [2]);
    let continuation = first_page.next.unwrap();
    let remaining =
        deadline.saturating_sub(timestamp(crate::cells::unix_now_ms().unwrap()).unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(remaining + 30)).await;

    let state = cell
        .client
        .query::<Get>(&cell.target, None, temporary().subject)
        .await
        .unwrap()
        .output
        .unwrap()
        .0;
    assert_eq!(state.version, 1);
    assert!(!state.locked);
    assert_eq!(state.expires_at_ms(), None);
    let error = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                before: Some(continuation),
                ..search.clone()
            },
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, crate::cells::Error::StaleSearch),
        "{error:?}"
    );
    assert_eq!(page(&cell, search).await.matching, 1);
    let unlocked = page(
        &cell,
        SearchInput {
            query: Some("is:unlocked is:open".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(numbers(&unlocked), [3, 1]);
    assert!(matches!(
        write::<CreateComment>(&cell, comment()).await,
        CreateCommentOutcome::Created(_)
    ));
    let Outcome::Saved(replayed, false) = write::<Set>(&cell, temporary()).await else {
        panic!("lock replay failed")
    };
    assert!(!replayed.locked);
    let Outcome::Saved(renewed, false) = write::<Set>(
        &cell,
        collaboration::locks::Input {
            version: 1,
            ..temporary()
        },
    )
    .await
    else {
        panic!("new lock failed")
    };
    assert!(renewed.locked);
    assert_eq!(renewed.version, 2);
    assert!(renewed.expires_at_ms().unwrap() > deadline);
    server.shutdown_runtimes().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn format_14_preparation_pages_existing_locks_and_preserves_other_indexes() {
    use collaboration::locks::{Outcome, Set};
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for number in 1..=10 {
        create(&cell, "Indexed needle", "").await;
        assert!(matches!(
            write::<Set>(&cell, lock(number, None)).await,
            Outcome::Saved(_, false)
        ));
    }
    let ready = cell
        .client
        .query::<GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let SearchIndexStatus::Ready {
        generation: previous,
        revision,
    } = ready
    else {
        panic!("index not ready")
    };
    write::<IndexFixture>(&cell, IndexFixtureInput::Format14).await;
    let generation = Uuid::now_v7();
    let mut result = write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(previous),
            generation,
        },
    )
    .await;
    let mut phases = Vec::new();
    while let RebuildSearchOutcome::Progress {
        status: SearchIndexStatus::Building { progress, .. },
    } = result
    {
        phases.push(progress.phase);
        assert!(phases.len() <= 3);
        result = write::<RebuildIndex>(
            &cell,
            RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert_eq!(
        phases,
        [
            SearchIndexPhase::IssueLocks,
            SearchIndexPhase::IssueLocks,
            SearchIndexPhase::PullLocks
        ]
    );
    assert!(matches!(result, RebuildSearchOutcome::Progress {
        status: SearchIndexStatus::Ready { generation: new, revision: unchanged }
    } if generation == new && revision == unchanged));
    let filtered = page(
        &cell,
        SearchInput {
            query: Some("needle is:locked".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(filtered.matching, 10);
    server.shutdown_runtimes().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn format_15_preparation_pages_existing_issue_type_assignments() {
    use crate::cells::repository::issues::issue_type;
    use crate::cells::repository::search::issue_types::Binding;
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let selection = issue_type::Selection {
        id: Uuid::now_v7(),
        organization: Uuid::now_v7(),
        ownership_epoch: 1,
    };
    for number in 1..=10 {
        create(&cell, "Searchable report", "Details").await;
        assert!(matches!(
            write::<issue_type::Set>(
                &cell,
                issue_type::SetInput {
                    number,
                    expected_version: 0,
                    selection: Some(selection),
                }
            )
            .await,
            issue_type::Outcome::Saved(_),
        ));
    }
    let SearchIndexStatus::Ready {
        generation: previous,
        revision,
    } = cell
        .client
        .query::<GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output
    else {
        panic!("index not ready")
    };
    write::<IndexFixture>(&cell, IndexFixtureInput::Format15).await;
    let generation = Uuid::now_v7();
    let mut result = write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(previous),
            generation,
        },
    )
    .await;
    for after in [0, 8] {
        let RebuildSearchOutcome::Progress {
            status: SearchIndexStatus::Building { progress, .. },
        } = result
        else {
            panic!("index finished before reading all assignments")
        };
        assert_eq!(
            (progress.phase, progress.after),
            (SearchIndexPhase::IssueTypes, after)
        );
        result = write::<RebuildIndex>(
            &cell,
            RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert!(matches!(result, RebuildSearchOutcome::Progress {
        status: SearchIndexStatus::Ready { generation: new, revision: unchanged }
    } if generation == new && revision == unchanged));
    for (epoch, expected) in [(1, 10), (2, 0)] {
        let result = page(
            &cell,
            SearchInput {
                query: Some("report type:bug".into()),
                issue_types: Some(Binding {
                    organization: selection.organization,
                    ownership_epoch: epoch,
                    version: 0,
                    names: [("bug".into(), selection.id)].into(),
                }),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(result.matching, expected);
    }
    fixtures::close(&server).await;
}
