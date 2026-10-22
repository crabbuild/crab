use super::*;
use crate::cells::repository::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};
use crate::cells::repository::collaboration::{
    Subject, SubjectKind, Target,
    activity::{self, Action, ListActivity, Query},
};
use crate::cells::{
    RepositoryCell,
    application::{Mutation, MutationResult, Read},
    repository::*,
};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}
fn cell(server: &crate::server::Server) -> RepositoryCell {
    server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
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
async fn read<R: Read>(cell: &RepositoryCell, input: R::Input) -> R::Output {
    cell.client
        .query::<R>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
async fn create(cell: &RepositoryCell) -> IssueRecord {
    let CreateIssueOutcome::Created(issue) = write::<CreateIssue>(
        cell,
        CreateIssueInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Lifecycle".into(),
            body: "Report".into(),
        },
    )
    .await
    else {
        panic!("create rejected")
    };
    *issue
}
fn update(issue: &IssueRecord, state: Option<u8>, reason: Option<Reason>) -> UpdateIssueInput {
    UpdateIssueInput {
        number: issue.number,
        actor: actor(),
        can_triage: false,
        version: issue.version,
        title: None,
        body: None,
        state,
        state_reason: reason,
        label_ids: None,
        assignees: None,
    }
}
async fn changed(cell: &RepositoryCell, input: UpdateIssueInput) -> IssueRecord {
    let UpdateIssueOutcome::Updated(issue) = write::<UpdateIssue>(cell, input).await else {
        panic!("change rejected")
    };
    *issue
}
async fn lifecycle(cell: &RepositoryCell, number: u64) -> Lifecycle {
    read::<GetIssueDetail>(cell, number)
        .await
        .lifecycle
        .unwrap()
}
async fn events(cell: &RepositoryCell, number: u64) -> Vec<activity::Event> {
    read::<ListActivity>(
        cell,
        Query {
            subject: Some(Subject {
                kind: SubjectKind::Issue,
                number,
            }),
            before: None,
            limit: 50,
        },
    )
    .await
    .unwrap()
    .items
}

#[tokio::test(flavor = "multi_thread")]
async fn reasons_dates_history_and_receipt_replay_share_the_issue_transaction() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let issue = create(&cell).await;
    assert_eq!(lifecycle(&cell, issue.number).await, Lifecycle::default());
    let request = crate::app::mutation_identity().unwrap();
    let input = update(&issue, Some(1), None);
    let first = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let replay = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, input)
        .await
        .unwrap();
    assert_eq!(first.receipt, replay.receipt);
    assert_eq!(
        serde_json::to_value(&first.output).unwrap(),
        serde_json::to_value(&replay.output).unwrap()
    );
    let UpdateIssueOutcome::Updated(closed) = first.output else {
        panic!("close rejected")
    };
    let closed_lifecycle = lifecycle(&cell, issue.number).await;
    assert_eq!(
        closed_lifecycle,
        Lifecycle {
            state_reason: Some(Reason::Completed),
            closed_at_ms: Some(closed.updated_at_ms)
        }
    );
    let closed = changed(&cell, update(&closed, None, Some(Reason::NotPlanned))).await;
    assert_eq!(
        lifecycle(&cell, issue.number).await,
        Lifecycle {
            state_reason: Some(Reason::NotPlanned),
            ..closed_lifecycle
        }
    );
    let history = events(&cell, issue.number).await;
    assert!(matches!(
        history[0].action,
        Action::IssueCloseReasonChanged {
            from: Some(Reason::Completed),
            to: Reason::NotPlanned
        }
    ));
    assert!(matches!(
        history[1].action,
        Action::IssueClosed {
            reason: Reason::Completed
        }
    ));
    let reopened = changed(&cell, update(&closed, Some(0), None)).await;
    assert_eq!(
        lifecycle(&cell, issue.number).await,
        Lifecycle {
            state_reason: Some(Reason::Reopened),
            closed_at_ms: None
        }
    );
    let reclosed = changed(&cell, update(&reopened, Some(1), Some(Reason::NotPlanned))).await;
    assert_eq!(
        lifecycle(&cell, issue.number).await.closed_at_ms,
        Some(reclosed.updated_at_ms)
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_unauthorized_and_invalid_reason_changes_leave_metadata_and_history_unchanged() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let issue = create(&cell).await;
    let mut outsider = update(&issue, Some(1), Some(Reason::NotPlanned));
    outsider.actor.subject = "bob".into();
    assert!(matches!(
        write::<UpdateIssue>(&cell, outsider.clone()).await,
        UpdateIssueOutcome::Forbidden
    ));
    outsider.can_triage = true;
    let closed = changed(&cell, outsider).await;
    assert!(matches!(
        write::<UpdateIssue>(&cell, update(&issue, None, Some(Reason::Completed))).await,
        UpdateIssueOutcome::Conflict
    ));
    let before = lifecycle(&cell, issue.number).await;
    let history = serde_json::to_value(events(&cell, issue.number).await).unwrap();
    for input in [
        update(&closed, None, Some(Reason::Reopened)),
        update(&closed, Some(0), Some(Reason::Completed)),
    ] {
        assert!(matches!(
            write::<UpdateIssue>(&cell, input).await,
            UpdateIssueOutcome::InvalidStateReason
        ));
    }
    assert_eq!(lifecycle(&cell, issue.number).await, before);
    assert_eq!(
        serde_json::to_value(events(&cell, issue.number).await).unwrap(),
        history
    );
    assert_eq!(
        read::<GetIssue>(&cell, issue.number).await.unwrap().version,
        closed.version
    );
    fixtures::close(&server).await;
}

// Construct the former on-disk schema and immutable generic closure events.
// Only explicit preparation may replace the missing canonical metadata relation.
pub(in crate::cells::repository) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_480;
    type Input = bool;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, abort: bool) -> Result<MutationResult<()>> {
        if abort {
            PrepareIssueLifecycle::execute(context, PrepareLifecycleInput { version: 0 })?;
            return Err(Error::Invalid("injected preparation rollback"));
        }
        for (number, action) in [
            (1, Some(Action::Closed)),
            (2, Some(Action::Reopened)),
            (3, None),
        ] {
            context.query(
                "UPDATE repository_issues SET state=?, updated_at_ms=created_at_ms+1000 WHERE number=?",
                &[integer(u64::from(number != 2))?, integer(number)?],
            )?;
            if let Some(action) = action {
                activity::append(context, Target::Issue { number }, &actor(), action)?;
            }
        }
        for _ in 0..19 {
            activity::append(
                context,
                Target::Issue { number: 2 },
                &actor(),
                Action::Updated,
            )?;
        }
        context.install_schema("DROP TABLE repository_issue_lifecycle;")?;
        context.delete_document("repository/issues/lifecycle/v1/preparation")?;
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_preparation_is_bounded_replayable_and_preserves_unknown_history() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for _ in 0..3 {
        create(&cell).await;
    }
    write::<Historical>(&cell, false).await;
    let original = serde_json::to_value(read::<GetIssue>(&cell, 1).await).unwrap();
    assert_eq!(
        read::<GetIssueLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Outdated
    );
    assert!(
        cell.client
            .query::<GetIssueDetail>(&cell.target, None, 1)
            .await
            .is_err()
    );
    assert!(matches!(
        write::<PrepareIssueLifecycle>(&cell, PrepareLifecycleInput { version: 1 }).await,
        PrepareLifecycleOutcome::Conflict(LifecycleStatus::Outdated)
    ));
    let request = crate::app::mutation_identity().unwrap();
    let first = cell
        .client
        .command::<PrepareIssueLifecycle>(
            &cell.target,
            request,
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap();
    assert_eq!(
        first.output,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
            version: 1,
            copied: 1
        })
    );
    let replay = cell
        .client
        .command::<PrepareIssueLifecycle>(
            &cell.target,
            request,
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap();
    assert_eq!(first.receipt, replay.receipt);
    for _ in 0..8 {
        match read::<GetIssueLifecycleStatus>(&cell, ()).await {
            LifecycleStatus::Ready => break,
            LifecycleStatus::Migrating { version, .. } => {
                write::<PrepareIssueLifecycle>(&cell, PrepareLifecycleInput { version }).await;
            }
            _ => panic!("preparation lost its progress"),
        }
    }
    assert_eq!(
        read::<GetIssueLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Ready
    );
    assert_eq!(
        serde_json::to_value(read::<GetIssue>(&cell, 1).await).unwrap(),
        original
    );
    let close = events(&cell, 1)
        .await
        .into_iter()
        .find(|event| event.action == Action::Closed)
        .unwrap();
    assert_eq!(
        lifecycle(&cell, 1).await,
        Lifecycle {
            state_reason: None,
            closed_at_ms: Some(close.created_at)
        }
    );
    assert_eq!(
        lifecycle(&cell, 2).await,
        Lifecycle {
            state_reason: Some(Reason::Reopened),
            closed_at_ms: None
        }
    );
    assert_eq!(lifecycle(&cell, 3).await, Lifecycle::default());
    assert_eq!(
        write::<PrepareIssueLifecycle>(&cell, PrepareLifecycleInput { version: 99 }).await,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Ready)
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preparation_rolls_back_schema_rows_and_cursor_on_a_failed_native_transaction() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for _ in 0..3 {
        create(&cell).await;
    }
    write::<Historical>(&cell, false).await;
    assert!(
        cell.client
            .command::<Historical>(&cell.target, crate::app::mutation_identity().unwrap(), true)
            .await
            .is_err()
    );
    assert_eq!(
        read::<GetIssueLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Outdated
    );
    assert!(matches!(
        write::<PrepareIssueLifecycle>(&cell, PrepareLifecycleInput { version: 0 }).await,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
            version: 1,
            copied: 1
        })
    ));
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reason_search_counts_boolean_expressions_and_cursors_use_canonical_metadata() {
    use crate::cells::repository::search::{
        index::SearchInput,
        results::{DiscussionSummary, ListDiscussions},
    };
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let completed = create(&cell).await;
    let not_planned = create(&cell).await;
    let reopened = create(&cell).await;
    create(&cell).await;
    changed(&cell, update(&completed, Some(1), None)).await;
    changed(
        &cell,
        update(&not_planned, Some(1), Some(Reason::NotPlanned)),
    )
    .await;
    let closed = changed(&cell, update(&reopened, Some(1), None)).await;
    changed(&cell, update(&closed, Some(0), None)).await;
    write::<CreatePull>(
        &cell,
        CreatePullInput {
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Pull sibling".into(),
            body: "".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    for (query, expected) in [
        ("reason:completed", vec![(false, 1)]),
        ("reason:\"not planned\"", vec![(false, 2)]),
        ("reason:reopened", vec![(false, 3)]),
        ("no:reason", vec![(false, 4)]),
        (
            "reason:completed OR reason:\"not planned\"",
            vec![(false, 2), (false, 1)],
        ),
        (
            "is:issue -reason:completed -reason:\"not planned\"",
            vec![(false, 4), (false, 3)],
        ),
        ("is:pr reason:completed", vec![]),
    ] {
        let page = read::<ListDiscussions>(
            &cell,
            SearchInput {
                query: Some(query.into()),
                ..Default::default()
            },
        )
        .await;
        let actual = page
            .items
            .iter()
            .map(|item| match item {
                DiscussionSummary::Issue(issue) => (false, issue.number),
                DiscussionSummary::Pull(pull) => (true, pull.number),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{query}");
        assert_eq!(page.matching, expected.len() as u64, "{query}");
    }
    let input = SearchInput {
        query: Some("is:issue -reason:completed".into()),
        limit: 1,
        ..Default::default()
    };
    let first = read::<ListDiscussions>(&cell, input.clone()).await;
    let page = read::<ListDiscussions>(
        &cell,
        SearchInput {
            before: first.next.clone(),
            ..input.clone()
        },
    )
    .await;
    assert_eq!(page.matching, 3);
    let newer = read::<GetIssue>(&cell, 2).await.unwrap();
    changed(&cell, update(&newer, None, Some(Reason::Completed))).await;
    assert!(
        cell.client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                SearchInput {
                    before: first.next,
                    ..input
                }
            )
            .await
            .is_err()
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preparation_cursor_and_original_command_bytes_survive_native_restart() {
    use crate::cells::runtime;
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    async fn stop(node: cellar::sdk::Node) {
        let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap()
        else {
            panic!("node did not stop")
        };
        assert!(report.cells_drained && report.application_work_drained && !report.drain_timed_out);
    }
    let local = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:8788".parse().unwrap();
    let started = runtime::test_node(local.path(), address).await;
    let repository = Uuid::now_v7();
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    let issue = create(&cell).await;
    for _ in 0..2 {
        create(&cell).await;
    }
    let mut input = update(&issue, None, None);
    input.body = Some("Stored before lifecycle preparation".into());
    // These are the input and output fields already bound to stored receipts.
    let encoded = serde_json::to_value(&input).unwrap();
    assert!(encoded.get("state_reason").is_none());
    let request = crate::app::mutation_identity().unwrap();
    let original = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let original_bytes = serde_json::to_vec(&original.output).unwrap();
    write::<Historical>(&cell, false).await;
    let frame_id = crate::app::mutation_identity().unwrap();
    let frame = cell
        .client
        .command::<PrepareIssueLifecycle>(
            &cell.target,
            frame_id,
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap();
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    let replay = cell
        .client
        .command::<PrepareIssueLifecycle>(
            &cell.target,
            frame_id,
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap();
    assert_eq!(replay.receipt, frame.receipt);
    assert_eq!(replay.output, frame.output);
    for _ in 0..8 {
        match read::<GetIssueLifecycleStatus>(&cell, ()).await {
            LifecycleStatus::Ready => break,
            LifecycleStatus::Migrating { version, .. } => {
                write::<PrepareIssueLifecycle>(&cell, PrepareLifecycleInput { version }).await;
            }
            _ => panic!("lost cursor"),
        }
    }
    assert_eq!(
        read::<GetIssueLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Ready
    );
    let replay = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, input)
        .await
        .unwrap();
    assert_eq!(replay.receipt, original.receipt);
    assert_eq!(serde_json::to_vec(&replay.output).unwrap(), original_bytes);
    assert_eq!(
        read::<GetIssueDetail>(&cell, issue.number)
            .await
            .issue
            .unwrap()
            .state,
        1
    );
    assert!(lifecycle(&cell, issue.number).await.closed_at_ms.is_some());
    stop(restored.node).await;
}
