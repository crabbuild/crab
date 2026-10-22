use super::*;
use crate::cells::repository::*;
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn actor(name: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: name.into(),
        name: name.into(),
    }
}
fn identity() -> MutationIdentity {
    crate::app::mutation_identity().unwrap()
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
        .command::<M>(&cell.target, identity(), input)
        .await
        .unwrap()
        .output
}
fn issue(title: &str) -> CreateIssueInput {
    CreateIssueInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        title: title.into(),
        body: "Private discussion body".into(),
    }
}
fn query(subject: Option<Subject>, before: Option<u64>, limit: u8) -> Query {
    Query {
        subject,
        before,
        limit,
    }
}
async fn page(cell: &RepositoryCell, input: Query) -> Page<Event> {
    cell.client
        .query::<ListActivity>(&cell.target, None, input)
        .await
        .unwrap()
        .output
        .unwrap()
}
fn subject(number: u64) -> Subject {
    Subject {
        kind: super::super::SubjectKind::Issue,
        number,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn command_and_submission_replays_do_not_duplicate_activity() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let input = issue("Issue");
    let request = identity();
    let committed = cell
        .client
        .command::<CreateIssue>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let replay = cell
        .client
        .command::<CreateIssue>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert_eq!(committed, replay);
    write::<CreateIssue>(&cell, input.clone()).await;
    let mut changed = input;
    changed.title = "Different payload".into();
    assert_eq!(
        write::<CreateIssue>(&cell, changed).await,
        CreateIssueOutcome::RequestConflict
    );
    let events = page(&cell, query(None, None, 50)).await;
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| (event.number, event.action.clone()))
            .collect::<Vec<_>>(),
        vec![(1, Action::Created)]
    );
    assert_eq!(events.items[0].title, "Issue");
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn state_and_content_changes_are_ordered_and_rejected_or_noop_edits_emit_nothing() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<CreateIssue>(&cell, issue("Issue")).await;
    let input = UpdateIssueInput {
        state_reason: None,
        number: 1,
        version: 1,
        actor: actor("Alice"),
        can_triage: false,
        title: Some("Updated title".into()),
        body: None,
        state: Some(1),
        label_ids: None,
        assignees: None,
    };
    assert!(matches!(
        write::<UpdateIssue>(&cell, input.clone()).await,
        UpdateIssueOutcome::Updated(_)
    ));
    assert_eq!(
        write::<UpdateIssue>(&cell, input.clone()).await,
        UpdateIssueOutcome::Conflict
    );
    let mut noop = input;
    noop.version = 2;
    assert!(matches!(
        write::<UpdateIssue>(&cell, noop).await,
        UpdateIssueOutcome::Updated(_)
    ));
    let events = page(&cell, query(Some(subject(1)), None, 50)).await;
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| (event.number, event.action.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                3,
                Action::IssueClosed {
                    reason: crate::cells::repository::issues::lifecycle::Reason::Completed
                }
            ),
            (2, Action::Updated),
            (1, Action::Created)
        ]
    );
    assert_eq!(events.items[0].title, "Updated title");
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    assert_eq!(
        write::<CreateIssue>(&cell, issue("Archived")).await,
        CreateIssueOutcome::Archived
    );
    assert_eq!(page(&cell, query(None, None, 50)).await, events);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn descending_subject_pages_remain_stable_when_new_events_arrive() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for title in ["First", "Second"] {
        write::<CreateIssue>(&cell, issue(title)).await;
    }
    for _ in 0..5 {
        write::<CreateComment>(
            &cell,
            CreateCommentInput {
                issue: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Bob"),
                body: "Comment".into(),
            },
        )
        .await;
    }
    let first = page(&cell, query(Some(subject(1)), None, 2)).await;
    assert_eq!(
        first
            .items
            .iter()
            .map(|event| event.number)
            .collect::<Vec<_>>(),
        vec![7, 6]
    );
    write::<CreateComment>(
        &cell,
        CreateCommentInput {
            issue: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "New comment".into(),
        },
    )
    .await;
    let mut numbers = first
        .items
        .iter()
        .map(|event| event.number)
        .collect::<Vec<_>>();
    let mut before = first.next;
    while before.is_some() {
        let next = page(&cell, query(Some(subject(1)), before, 2)).await;
        numbers.extend(next.items.iter().map(|event| event.number));
        before = next.next;
    }
    assert_eq!(numbers, vec![7, 6, 5, 4, 3, 1]);
    let other = page(&cell, query(Some(subject(2)), None, 2)).await;
    assert_eq!(other.items[0].number, 2);
    assert_eq!(page(&cell, query(None, None, 50)).await.items.len(), 8);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_commands_get_unique_ordered_event_numbers() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let commands =
        (0..20).map(|number| write::<CreateIssue>(&cell, issue(&format!("Issue {number}"))));
    for result in futures_util::future::join_all(commands).await {
        assert!(matches!(result, CreateIssueOutcome::Created(_)));
    }
    let events = page(&cell, query(None, None, 50)).await;
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.number)
            .collect::<Vec<_>>(),
        (1..=20).rev().collect::<Vec<_>>()
    );
    for input in [
        query(None, None, 0),
        query(None, None, 51),
        query(None, Some(0), 1),
    ] {
        assert!(
            cell.client
                .query::<ListActivity>(&cell.target, None, input)
                .await
                .is_err()
        );
    }
    let absent = cell
        .client
        .query::<ListActivity>(&cell.target, None, query(Some(subject(21)), None, 1))
        .await
        .unwrap()
        .output;
    assert!(absent.is_none());
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn event_sequence_subject_index_and_receipts_survive_native_restart() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let request = identity();
    let input = issue("Durable activity");
    let committed = started
        .client
        .command::<CreateIssue>(&repository, request, input.clone())
        .await
        .unwrap();
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let replay = restored
        .client
        .command::<CreateIssue>(&repository, request, input)
        .await
        .unwrap();
    assert_eq!(committed, replay);
    restored
        .client
        .command::<CreateComment>(
            &repository,
            identity(),
            CreateCommentInput {
                issue: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Bob"),
                body: "After restart".into(),
            },
        )
        .await
        .unwrap();
    let events = restored
        .client
        .query::<ListActivity>(&repository, None, query(Some(subject(1)), None, 50))
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.number)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    let NodeOutcome::Stopped(report) = restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
}

#[tokio::test(flavor = "multi_thread")]
async fn review_threads_replies_and_merge_completion_record_their_actual_targets() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<CreateIssue>(&cell, issue("Issue")).await;
    write::<CreateComment>(
        &cell,
        CreateCommentInput {
            issue: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "Comment".into(),
        },
    )
    .await;
    write::<CreatePull>(
        &cell,
        CreatePullInput {
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            title: "Pull".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    write::<CreatePullComment>(
        &cell,
        CreatePullCommentInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "Comment".into(),
        },
    )
    .await;
    write::<CreatePullReview>(
        &cell,
        CreatePullReviewInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: String::new(),
            state: ReviewState::Approved,
            commit_oid: "b".repeat(40),
        },
    )
    .await;
    write::<CreatePullReviewThread>(
        &cell,
        CreatePullReviewThreadInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "Thread".into(),
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
            actor: actor("Alice"),
            can_resolve: true,
            version: 1,
            body: None,
            suggested_text: None,
            resolved: Some(true),
        },
    )
    .await;
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        method: MergeMethod::FastForward,
        pull_version: pull.version,
        base_oid: pull.base_oid,
        head_oid: pull.head_oid.clone(),
        commit_oid: pull.head_oid,
        message: String::new(),
        created_at_ms: u64::try_from(crate::cells::unix_now_ms().unwrap()).unwrap(),
    };
    write::<ReservePullMerge>(
        &cell,
        ReservePullMergeInput {
            pull: 1,
            merge: merge.clone(),
        },
    )
    .await;
    for transition in [
        PullMergeTransition::Begin,
        PullMergeTransition::Complete,
        PullMergeTransition::Complete,
    ] {
        assert!(matches!(
            write::<TransitionPullMerge>(
                &cell,
                TransitionPullMergeInput {
                    pull: 1,
                    merge: merge.clone(),
                    transition
                }
            )
            .await,
            TransitionPullMergeOutcome::Applied(_)
        ));
    }
    let events = page(&cell, query(None, None, 50)).await;
    assert_eq!(events.items.len(), 9);
    assert_eq!(events.items[0].action, Action::Merged);
    assert_eq!(events.items[1].action, Action::Resolved);
    for target in [
        Target::Issue { number: 1 },
        Target::IssueComment {
            issue: 1,
            number: 1,
        },
        Target::Pull { number: 1 },
        Target::PullComment { pull: 1, number: 1 },
        Target::PullReview { pull: 1, number: 1 },
        Target::PullThread { pull: 1, number: 1 },
        Target::PullReply {
            pull: 1,
            thread: 1,
            number: 1,
        },
    ] {
        assert!(
            events.items.iter().any(|event| event.target == target),
            "{target:?}"
        );
    }
    assert_eq!(
        events
            .items
            .iter()
            .filter(|event| event.action == Action::Approved)
            .count(),
        1
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn milestone_reassignment_emits_both_relationship_changes_once() {
    use super::super::{Decision, milestones::*};
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<CreateIssue>(&cell, issue("Issue")).await;
    for title in ["First milestone", "Second milestone"] {
        assert!(matches!(
            write::<CreateMilestone>(
                &cell,
                CreateMilestoneInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor("Alice"),
                    title: title.into(),
                    description: String::new(),
                    due_on: None
                }
            )
            .await,
            Decision::Applied(_)
        ));
    }
    for (version, milestone) in [(0, Some(1)), (1, Some(2)), (2, None), (3, None)] {
        assert!(matches!(
            write::<AssignMilestone>(
                &cell,
                AssignMilestoneInput {
                    subject: subject(1),
                    actor: actor("Alice"),
                    version,
                    milestone
                }
            )
            .await,
            Decision::Applied(_)
        ));
    }
    let events = page(&cell, query(Some(subject(1)), None, 50)).await;
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.action.clone())
            .collect::<Vec<_>>(),
        vec![
            Action::MilestoneRemoved {
                number: 2,
                title: "Second milestone".into()
            },
            Action::MilestoneAssigned {
                number: 2,
                title: "Second milestone".into()
            },
            Action::MilestoneRemoved {
                number: 1,
                title: "First milestone".into()
            },
            Action::MilestoneAssigned {
                number: 1,
                title: "First milestone".into()
            },
            Action::Created,
        ]
    );
    fixtures::close(&server).await;
}
