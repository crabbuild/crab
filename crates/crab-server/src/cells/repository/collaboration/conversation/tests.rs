use super::read::Page;
use super::*;
use crate::cells::{
    RepositoryCell,
    application::{Mutation, MutationResult},
    repository::*,
};
use crate::test_support::server as fixtures;

fn actor(name: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: name.into(),
        name: name.into(),
    }
}
fn subject(kind: SubjectKind) -> Subject {
    Subject { kind, number: 1 }
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
async fn page(cell: &RepositoryCell, subject: Subject, position: Position, limit: u8) -> Page {
    cell.client
        .query::<ListConversation>(
            &cell.target,
            None,
            Query {
                subject,
                position,
                limit,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
async fn seed(cell: &RepositoryCell, count: usize) {
    write::<CreateIssue>(
        cell,
        CreateIssueInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            title: "Issue".into(),
            body: "Root description".into(),
        },
    )
    .await;
    write::<CreatePull>(
        cell,
        CreatePullInput {
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            title: "Pull".into(),
            body: "Root description".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    for _ in 0..count {
        write::<CreateComment>(
            cell,
            CreateCommentInput {
                issue: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Alice"),
                body: "Issue comment".into(),
            },
        )
        .await;
        write::<CreatePullComment>(
            cell,
            CreatePullCommentInput {
                pull: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Alice"),
                body: "Pull comment".into(),
            },
        )
        .await;
        write::<CreatePullReview>(
            cell,
            CreatePullReviewInput {
                pull: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Bob"),
                body: "Review".into(),
                state: ReviewState::Commented,
                commit_oid: "b".repeat(40),
            },
        )
        .await;
    }
}
fn cell(server: &crate::server::Server) -> RepositoryCell {
    server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_ordered_stream_contains_contributions_and_lifecycle_without_duplicate_cards() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 7).await;
    for (version, state) in [(1, 1), (2, 0)] {
        write::<UpdateIssue>(
            &cell,
            UpdateIssueInput {
                state_reason: None,
                number: 1,
                actor: actor("Alice"),
                can_triage: true,
                version,
                title: None,
                body: None,
                state: Some(state),
                label_ids: None,
                assignees: None,
            },
        )
        .await;
    }
    let issue = page(&cell, subject(SubjectKind::Issue), Position::Latest, 50).await;
    assert_eq!(issue.items.len(), 9);
    assert_eq!(
        issue
            .items
            .iter()
            .filter(|e| matches!(e, Entry::IssueComment(_)))
            .count(),
        7
    );
    assert!(matches!(
        issue.items.last(),
        Some(Entry::Activity(Event {
            action: Action::Reopened,
            ..
        }))
    ));
    let all = page(&cell, subject(SubjectKind::Pull), Position::Latest, 50).await;
    assert_eq!(all.items.len(), 14);
    assert!(all.items.iter().all(|e| !matches!(e, Entry::Activity(_))));
    assert_eq!(all.source.unwrap().head_ref, "refs/heads/feature");
    let mut position = Position::Latest;
    let mut merged = Vec::new();
    loop {
        let part = page(&cell, subject(SubjectKind::Pull), position, 3).await;
        merged.splice(0..0, part.items);
        let Some(older) = part.start.filter(|_| part.has_earlier) else {
            break;
        };
        position = Position::Before(older);
    }
    assert_eq!(merged, all.items);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn focused_windows_page_both_directions_and_reject_cross_discussion_cursors() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 12).await;
    let key = Key {
        source: Source::Review,
        number: 4,
    };
    let focus = page(&cell, subject(SubjectKind::Pull), Position::Focus(key), 4).await;
    assert_eq!(focus.items.last().unwrap().key(), key);
    assert!(focus.has_earlier && focus.has_later);
    let mut all = focus.items.clone();
    let mut newer = focus.end.clone().filter(|_| focus.has_later);
    while let Some(cursor) = newer {
        let part = page(
            &cell,
            subject(SubjectKind::Pull),
            Position::After(cursor),
            4,
        )
        .await;
        all.extend(part.items);
        newer = part.end.filter(|_| part.has_later);
    }
    let mut older = focus.start.clone().filter(|_| focus.has_earlier);
    while let Some(cursor) = older {
        let part = page(
            &cell,
            subject(SubjectKind::Pull),
            Position::Before(cursor),
            4,
        )
        .await;
        all.splice(0..0, part.items);
        older = part.start.filter(|_| part.has_earlier);
    }
    assert_eq!(
        all,
        page(&cell, subject(SubjectKind::Pull), Position::Latest, 50)
            .await
            .items
    );
    let cross = cell
        .client
        .query::<ListConversation>(
            &cell.target,
            None,
            Query {
                subject: subject(SubjectKind::Issue),
                position: Position::Before(focus.start.unwrap()),
                limit: 3,
            },
        )
        .await;
    assert!(cross.is_err());
    for position in [
        Position::Focus(Key {
            source: Source::Review,
            number: 0,
        }),
        Position::Focus(Key {
            source: Source::Activity,
            number: repository::MAX_NUMBER + 1,
        }),
    ] {
        assert!(
            cell.client
                .query::<ListConversation>(
                    &cell.target,
                    None,
                    Query {
                        subject: subject(SubjectKind::Pull),
                        position,
                        limit: 1
                    }
                )
                .await
                .is_err()
        );
    }
    assert!(
        cell.client
            .query::<ListConversation>(
                &cell.target,
                None,
                Query {
                    subject: subject(SubjectKind::Pull),
                    position: Position::Focus(Key {
                        source: Source::Review,
                        number: 99
                    }),
                    limit: 3
                }
            )
            .await
            .unwrap()
            .output
            .is_none()
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn serialized_byte_bounds_keep_every_large_contribution_reachable() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 0).await;
    for _ in 0..7 {
        write::<CreateComment>(
            &cell,
            CreateCommentInput {
                issue: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Alice"),
                body: "\u{1}".repeat(64 * 1024),
            },
        )
        .await;
    }
    let mut position = Position::Latest;
    let mut count = 0;
    loop {
        let part = page(&cell, subject(SubjectKind::Issue), position, 50).await;
        assert!(serde_json::to_vec(&part).unwrap().len() < repository::MAX_LIST_OUTPUT_BYTES);
        assert!(!part.items.is_empty());
        count += part.items.len();
        let Some(older) = part.start.filter(|_| part.has_earlier) else {
            break;
        };
        position = Position::Before(older);
    }
    assert_eq!(count, 7);
    fixtures::close(&server).await;
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::cells::repository) enum Fixture {
    Historical,
    Plan,
}
pub(in crate::cells::repository) struct Inspect;
impl Mutation for Inspect {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 65_527;
    type Input = Fixture;
    type Output = Vec<String>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Fixture,
    ) -> Result<MutationResult<Self::Output>> {
        match input {
            Fixture::Historical => {
                // These historical source timestamps deliberately roll backwards.
                // Preparation must index them rather than infer order from IDs.
                context.query("UPDATE repository_issue_comments SET created_at_ms = 100 - number, updated_at_ms = 100 - number WHERE issue_number = 1",&[])?;
                context.install_schema("DROP TABLE discussion_conversation;")?;
                let mut state: serde_json::Value =
                    context.document("discussion-search/v1/state")?.unwrap();
                state["format"] = serde_json::json!(9);
                context.put_document("discussion-search/v1/state", &state)?;
                Ok(MutationResult::Success(Vec::new()))
            }
            Fixture::Plan => {
                let rows=context.query("EXPLAIN QUERY PLAN SELECT created_at_ms, source, number FROM discussion_conversation WHERE kind = 1 AND discussion = 1 AND (created_at_ms, source, number) < (100, 1, 4) ORDER BY created_at_ms DESC, source DESC, number DESC LIMIT 30",&[])?;
                let plans = rows
                    .rows
                    .iter()
                    .map(|row| repository::result_text(row, 3))
                    .collect::<Result<_>>()?;
                Ok(MutationResult::Success(plans))
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_preparation_restores_historical_rows_and_invalidates_previous_generation() {
    use repository::search::index::{
        GetSearchIndexStatus, RebuildIndex, RebuildSearchInput, SearchIndexPhase, SearchIndexStatus,
    };
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 17).await;
    let old = page(&cell, subject(SubjectKind::Issue), Position::Latest, 3).await;
    let plans = write::<Inspect>(&cell, Fixture::Plan).await;
    assert!(
        plans
            .iter()
            .any(|s| s.contains("discussion_conversation_order") && s.contains("SEARCH"))
    );
    assert!(
        plans
            .iter()
            .all(|s| !s.contains("SCAN") && !s.contains("TEMP B-TREE"))
    );
    write::<Inspect>(&cell, Fixture::Historical).await;
    let status = cell
        .client
        .query::<GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let SearchIndexStatus::Outdated {
        generation: previous,
        format: 9,
    } = status
    else {
        panic!("expected historical preparation");
    };
    assert!(
        cell.client
            .query::<ListConversation>(
                &cell.target,
                None,
                Query {
                    subject: subject(SubjectKind::Issue),
                    position: Position::Latest,
                    limit: 30
                }
            )
            .await
            .is_err()
    );
    let generation = Uuid::now_v7();
    write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(previous),
            generation,
        },
    )
    .await;
    let mut frames = 0;
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
        if progress.phase == SearchIndexPhase::Conversation {
            frames += 1;
        }
        let input = RebuildSearchInput::Step {
            generation,
            version: progress.version,
        };
        write::<RebuildIndex>(&cell, input.clone()).await;
        write::<RebuildIndex>(&cell, input).await;
    }
    assert!(frames > 8);
    let after = page(&cell, subject(SubjectKind::Issue), Position::Latest, 50).await;
    assert_eq!(
        after
            .items
            .iter()
            .map(|e| e.key().number)
            .collect::<Vec<_>>(),
        (1..=17).rev().collect::<Vec<_>>()
    );
    assert!(
        cell.client
            .query::<ListConversation>(
                &cell.target,
                None,
                Query {
                    subject: subject(SubjectKind::Issue),
                    position: Position::Before(old.start.unwrap()),
                    limit: 3
                }
            )
            .await
            .is_err()
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn relationships_and_inline_thread_lifecycle_share_canonical_event_identity() {
    use super::super::links;
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 0).await;
    for (active, version) in [(true, 0), (false, 1)] {
        write::<links::SetLink>(
            &cell,
            links::Selection {
                pair: links::Pair { pull: 1, issue: 1 },
                active,
                version,
                actor: actor("Alice"),
            },
        )
        .await;
    }
    let issue = page(&cell, subject(SubjectKind::Issue), Position::Latest, 50).await;
    let pull = page(&cell, subject(SubjectKind::Pull), Position::Latest, 50).await;
    assert_eq!(issue.items, pull.items);
    assert!(matches!(
        issue.items.as_slice(),
        [
            Entry::Activity(Event {
                action: Action::IssueLinked { .. },
                ..
            }),
            Entry::Activity(Event {
                action: Action::IssueUnlinked { .. },
                ..
            })
        ]
    ));
    write::<CreatePullReviewThread>(
        &cell,
        CreatePullReviewThreadInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "Inline discussion".into(),
            suggested_text: None,
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
            path: b"README.md".to_vec(),
            old_blob_oid: Some("c".repeat(40)),
            new_blob_oid: Some("d".repeat(40)),
            side: PullReviewThreadSide::New,
            start_line: 1,
            end_line: 1,
        },
    )
    .await;
    write::<CreatePullReviewReply>(
        &cell,
        CreatePullReviewReplyInput {
            pull: 1,
            thread: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            body: "Reply".into(),
        },
    )
    .await;
    write::<UpdatePullReviewThread>(
        &cell,
        UpdatePullReviewThreadInput {
            key: PullReviewThreadKey { pull: 1, number: 1 },
            actor: actor("Bob"),
            can_resolve: true,
            version: 1,
            body: None,
            suggested_text: None,
            resolved: Some(true),
        },
    )
    .await;
    let all = page(&cell, subject(SubjectKind::Pull), Position::Latest, 50).await;
    assert_eq!(all.items.len(), 5);
    assert!(matches!(
        all.items.last(),
        Some(Entry::Activity(Event {
            target: Target::PullThread { pull: 1, number: 1 },
            action: Action::Resolved,
            ..
        }))
    ));
    let focused = page(
        &cell,
        subject(SubjectKind::Pull),
        Position::Focus(all.items[3].key()),
        1,
    )
    .await;
    assert_eq!(focused.items, [all.items[3].clone()]);
    assert_eq!(
        page(&cell, subject(SubjectKind::Issue), Position::Latest, 50)
            .await
            .items,
        issue.items
    );
    fixtures::close(&server).await;
}
