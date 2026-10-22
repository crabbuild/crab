use super::*;
use crate::cells::repository::{
    CreateIssue, CreateIssueInput, CreateIssueOutcome, RepositoryAuthor, UpdateIssue,
    UpdateIssueInput, UpdateIssueOutcome,
};
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::server::Server;
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}

fn identity() -> MutationIdentity {
    let now = crate::cells::unix_now_ms().unwrap();
    MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
    }
}

fn cell(server: &Server) -> RepositoryCell {
    server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}

fn input(title: &str) -> CreateMilestoneInput {
    CreateMilestoneInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        title: title.into(),
        description: "Release planning".into(),
        due_on: Some("2028-02-29".into()),
    }
}

fn applied<T>(decision: Decision<T>) -> T {
    match decision {
        Decision::Applied(value) => value,
        _ => panic!("collaboration command was rejected"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_progress_tracks_issue_state_and_duplicate_commands() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let created = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), input("Version one"))
        .await
        .unwrap();
    let milestone = applied(created.output);
    let created = cell
        .client
        .command::<CreateIssue>(
            &cell.target,
            identity(),
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Ship the release".into(),
                body: String::new(),
            },
        )
        .await
        .unwrap();
    let CreateIssueOutcome::Created(issue) = created.output else {
        panic!("issue was not created");
    };
    let subject = Subject {
        kind: SubjectKind::Issue,
        number: issue.number,
    };
    let selected = cell
        .client
        .command::<AssignMilestone>(
            &cell.target,
            identity(),
            AssignMilestoneInput {
                subject,
                milestone: Some(milestone.number),
                version: 0,
                actor: actor(),
            },
        )
        .await
        .unwrap();
    assert_eq!(applied(selected.output).milestone.unwrap().open_count, 1);
    let request = identity();
    let close = UpdateIssueInput {
        number: issue.number,
        actor: actor(),
        can_triage: true,
        version: issue.version,
        title: None,
        body: None,
        state: Some(1),
        label_ids: None,
        assignees: None,
    };
    let first = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, close.clone())
        .await
        .unwrap();
    assert!(matches!(first.output, UpdateIssueOutcome::Updated(_)));
    let replay = cell
        .client
        .command::<UpdateIssue>(&cell.target, request, close)
        .await
        .unwrap();
    assert_eq!(first.receipt, replay.receipt);
    let current = cell
        .client
        .query::<GetMilestone>(&cell.target, Some(replay.receipt), milestone.number)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!((current.open_count, current.closed_count), (0, 1));
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn milestone_indexes_page_exclusively_across_state_changes() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let mut created = Vec::new();
    for name in ["One", "Two", "Three"] {
        created.push(applied(
            cell.client
                .command::<CreateMilestone>(&cell.target, identity(), input(name))
                .await
                .unwrap()
                .output,
        ));
    }
    let second = &created[1];
    cell.client
        .command::<EditMilestone>(
            &cell.target,
            identity(),
            EditMilestoneInput {
                number: second.number,
                version: second.version,
                actor: actor(),
                title: second.title.clone(),
                description: second.description.clone(),
                due_on: second.due_on.clone(),
                state: Some(MilestoneState::Closed),
            },
        )
        .await
        .unwrap();
    let first = cell
        .client
        .query::<ListMilestones>(
            &cell.target,
            None,
            ListMilestonesInput {
                before: None,
                limit: 1,
                state: Some(MilestoneState::Open),
            },
        )
        .await
        .unwrap()
        .output;
    let next = cell
        .client
        .query::<ListMilestones>(
            &cell.target,
            None,
            ListMilestonesInput {
                before: first.next,
                limit: 1,
                state: Some(MilestoneState::Open),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        (first.items[0].number, next.items[0].number, next.next),
        (3, 1, None)
    );
    let closed = cell
        .client
        .query::<ListMilestones>(
            &cell.target,
            None,
            ListMilestonesInput {
                before: None,
                limit: 50,
                state: Some(MilestoneState::Closed),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        closed
            .items
            .iter()
            .map(|value| value.number)
            .collect::<Vec<_>>(),
        [2]
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_edits_and_reused_creation_ids_preserve_saved_milestones() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let original = input("Original");
    let value = applied(
        cell.client
            .command::<CreateMilestone>(&cell.target, identity(), original.clone())
            .await
            .unwrap()
            .output,
    );
    let edit = EditMilestoneInput {
        number: value.number,
        version: value.version,
        actor: actor(),
        title: "Saved".into(),
        description: value.description,
        due_on: None,
        state: Some(MilestoneState::Open),
    };
    let saved = cell
        .client
        .command::<EditMilestone>(&cell.target, identity(), edit.clone())
        .await
        .unwrap();
    let mut stale = edit;
    stale.title = "Stale".into();
    let rejected = cell
        .client
        .command::<EditMilestone>(&cell.target, identity(), stale)
        .await
        .unwrap();
    assert_eq!(rejected.output, Decision::Conflict);
    let mut changed = original;
    changed.title = "Different creation".into();
    let rejected = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), changed)
        .await
        .unwrap();
    assert_eq!(rejected.output, Decision::Conflict);
    let current = cell
        .client
        .query::<GetMilestone>(&cell.target, Some(saved.receipt), value.number)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(current.title, "Saved");
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_calendar_dates_do_not_allocate_numbers() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for date in ["2027-02-29", "2028-13-01", "2028-1-01", "0000-01-01"] {
        let mut invalid = input("Invalid");
        invalid.due_on = Some(date.into());
        assert!(
            cell.client
                .command::<CreateMilestone>(&cell.target, identity(), invalid)
                .await
                .is_err(),
            "{date}"
        );
    }
    let created = applied(
        cell.client
            .command::<CreateMilestone>(&cell.target, identity(), input("Leap day"))
            .await
            .unwrap()
            .output,
    );
    assert_eq!(created.number, 1);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn milestone_documents_and_sql_state_survive_native_restart() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let repository = Uuid::now_v7();
    let started =
        crate::cells::runtime::test_node(directory.path(), listener.local_addr().unwrap()).await;
    started.client.initialize(repository).await.unwrap();
    let request = identity();
    let command = input("Durable milestone");
    let first = started
        .client
        .command::<CreateMilestone>(&repository, request, command.clone())
        .await
        .unwrap();
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop");
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored =
        crate::cells::runtime::test_node(directory.path(), listener.local_addr().unwrap()).await;
    let replay = restored
        .client
        .command::<CreateMilestone>(&repository, request, command)
        .await
        .unwrap();
    assert_eq!(first, replay);
    let page = restored
        .client
        .query::<ListMilestones>(
            &repository,
            Some(replay.receipt),
            ListMilestonesInput {
                before: None,
                limit: 50,
                state: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(page.items.len(), 1);
    let NodeOutcome::Stopped(report) = restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap()
    else {
        panic!("node did not stop");
    };
    assert!(report.cells_drained && !report.drain_timed_out);
}

#[tokio::test(flavor = "multi_thread")]
async fn assigned_work_pages_follow_reassignment_without_duplicates_or_orphans() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for title in ["First", "Second"] {
        cell.client
            .command::<CreateMilestone>(&cell.target, identity(), input(title))
            .await
            .unwrap();
    }
    for number in 1..=3 {
        cell.client
            .command::<CreateIssue>(
                &cell.target,
                identity(),
                CreateIssueInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor(),
                    title: format!("Work {number}"),
                    body: String::new(),
                },
            )
            .await
            .unwrap();
        cell.client
            .command::<AssignMilestone>(
                &cell.target,
                identity(),
                AssignMilestoneInput {
                    subject: Subject {
                        kind: SubjectKind::Issue,
                        number,
                    },
                    milestone: Some(1),
                    version: 0,
                    actor: actor(),
                },
            )
            .await
            .unwrap();
    }
    let page = cell
        .client
        .query::<ListMilestoneItems>(
            &cell.target,
            None,
            ListMilestoneItemsInput {
                number: 1,
                before: None,
                limit: 1,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(page.items[0].subject.number, 3);
    let subject = Subject {
        kind: SubjectKind::Issue,
        number: 2,
    };
    let moved = cell
        .client
        .command::<AssignMilestone>(
            &cell.target,
            identity(),
            AssignMilestoneInput {
                subject,
                milestone: Some(2),
                version: 1,
                actor: actor(),
            },
        )
        .await
        .unwrap();
    assert_eq!(applied(moved.output).version, 2);
    let stale = cell
        .client
        .command::<AssignMilestone>(
            &cell.target,
            identity(),
            AssignMilestoneInput {
                subject,
                milestone: None,
                version: 1,
                actor: actor(),
            },
        )
        .await
        .unwrap();
    assert_eq!(stale.output, Decision::Conflict);
    let next = cell
        .client
        .query::<ListMilestoneItems>(
            &cell.target,
            None,
            ListMilestoneItemsInput {
                number: 1,
                before: page.next,
                limit: 1,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!((next.items[0].subject.number, next.next), (1, None));
    let first = cell
        .client
        .query::<GetMilestone>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(first.open_count, 2);
    let second = cell
        .client
        .query::<ListMilestoneItems>(
            &cell.target,
            None,
            ListMilestoneItemsInput {
                number: 2,
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(second.items[0].subject, subject);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn milestone_creation_retries_use_stable_author_identity() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let mut command = input("Identity");
    let original = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), command.clone())
        .await
        .unwrap();
    command.author.name = "Alice renamed".into();
    let retry = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), command)
        .await
        .unwrap();
    assert_eq!(original.output, retry.output);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_milestone_commands_do_not_allocate_entities() {
    use crate::cells::repository::{ReplaceRepositoryLifecycle, ReplaceRepositoryLifecycleInput};
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    cell.client
        .command::<ReplaceRepositoryLifecycle>(
            &cell.target,
            identity(),
            ReplaceRepositoryLifecycleInput {
                expected_version: 0,
                archived: true,
            },
        )
        .await
        .unwrap();
    let rejected = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), input("Archived"))
        .await
        .unwrap();
    assert_eq!(rejected.output, Decision::Archived);
    cell.client
        .command::<ReplaceRepositoryLifecycle>(
            &cell.target,
            identity(),
            ReplaceRepositoryLifecycleInput {
                expected_version: 1,
                archived: false,
            },
        )
        .await
        .unwrap();
    let created = cell
        .client
        .command::<CreateMilestone>(&cell.target, identity(), input("Open"))
        .await
        .unwrap();
    assert_eq!(applied(created.output).number, 1);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_close_reopen_and_merge_completion_update_milestone_progress() {
    use crate::cells::repository::{
        CreatePull, CreatePullInput, CreatePullOutcome, MergeMethod, PullMerge,
        PullMergeTransition, PullState, ReservePullMerge, ReservePullMergeInput,
        TransitionPullMerge, TransitionPullMergeInput, TransitionPullMergeOutcome, UpdatePull,
        UpdatePullInput,
    };
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    cell.client
        .command::<CreateMilestone>(&cell.target, identity(), input("Pull progress"))
        .await
        .unwrap();
    let created = cell
        .client
        .command::<CreatePull>(
            &cell.target,
            identity(),
            CreatePullInput {
                draft: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Ship".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    let CreatePullOutcome::Created(pull) = created.output else {
        panic!("pull was not created")
    };
    cell.client
        .command::<AssignMilestone>(
            &cell.target,
            identity(),
            AssignMilestoneInput {
                subject: Subject {
                    kind: SubjectKind::Pull,
                    number: pull.number,
                },
                milestone: Some(1),
                version: 0,
                actor: actor(),
            },
        )
        .await
        .unwrap();
    for (version, state, expected) in [(1, PullState::Closed, (0, 1)), (2, PullState::Open, (1, 0))]
    {
        cell.client
            .command::<UpdatePull>(
                &cell.target,
                identity(),
                UpdatePullInput {
                    number: pull.number,
                    actor: actor(),
                    can_triage: true,
                    version,
                    title: None,
                    body: None,
                    state: Some(state),
                    label_ids: None,
                    assignees: None,
                },
            )
            .await
            .unwrap();
        let progress = cell
            .client
            .query::<GetMilestone>(&cell.target, None, 1)
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!((progress.open_count, progress.closed_count), expected);
    }
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        method: MergeMethod::FastForward,
        pull_version: 3,
        base_oid: pull.base_oid.clone(),
        head_oid: pull.head_oid.clone(),
        commit_oid: pull.head_oid.clone(),
        message: "Merge".into(),
        created_at_ms: crate::cells::unix_now_ms().unwrap() as u64,
    };
    cell.client
        .command::<ReservePullMerge>(
            &cell.target,
            identity(),
            ReservePullMergeInput {
                pull: pull.number,
                merge: merge.clone(),
            },
        )
        .await
        .unwrap();
    for transition in [PullMergeTransition::Begin, PullMergeTransition::Complete] {
        let result = cell
            .client
            .command::<TransitionPullMerge>(
                &cell.target,
                identity(),
                TransitionPullMergeInput {
                    pull: pull.number,
                    merge: merge.clone(),
                    transition,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            result.output,
            TransitionPullMergeOutcome::Applied(_)
        ));
    }
    let progress = cell
        .client
        .query::<GetMilestone>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!((progress.open_count, progress.closed_count), (0, 1));
    fixtures::close(&server).await;
}
