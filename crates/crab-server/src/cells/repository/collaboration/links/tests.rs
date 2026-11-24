use super::*;
use crate::cells::repository::*;
use crate::cells::{MutationIdentity, RepositoryCell};
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
async fn seed(cell: &RepositoryCell, count: u64) {
    for number in 1..=count {
        write::<CreateIssue>(
            cell,
            CreateIssueInput {
                metadata: Default::default(),
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: format!("Issue {number}"),
                body: String::new(),
            },
        )
        .await;
    }
    write::<CreatePull>(
        cell,
        CreatePullInput {
            code_owner_requests: Default::default(),
            metadata: Default::default(),
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Pull".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            head_repository: None,
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
fn selection(issue: u64, active: bool, version: u64) -> Selection {
    Selection {
        pair: Pair { pull: 1, issue },
        active,
        version,
        actor: actor(),
        closes_on_merge: None,
    }
}
fn subject(kind: SubjectKind, number: u64) -> Subject {
    Subject { kind, number }
}
async fn list(
    cell: &RepositoryCell,
    subject: Subject,
    before: Option<u64>,
    limit: u8,
) -> Page<LinkedWork> {
    cell.client
        .query::<ListLinks>(
            &cell.target,
            None,
            Query {
                subject,
                before,
                limit,
                default_ref: "refs/heads/main".into(),
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
async fn activity(cell: &RepositoryCell, subject: Option<Subject>) -> Vec<activity::Event> {
    cell.client
        .query::<activity::ListActivity>(
            &cell.target,
            None,
            activity::Query {
                subject,
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
        .items
}

#[tokio::test(flavor = "multi_thread")]
async fn links_are_bidirectional_versioned_and_record_one_event_in_both_timelines() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 1).await;
    let pair = Pair { pull: 1, issue: 1 };
    let request = identity();
    let input = selection(1, true, 0);
    let first = cell
        .client
        .command::<SetLink>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert_eq!(
        first,
        cell.client
            .command::<SetLink>(&cell.target, request, input.clone())
            .await
            .unwrap()
    );
    assert_eq!(first.output, write::<SetLink>(&cell, input.clone()).await);
    let pull = subject(SubjectKind::Pull, 1);
    let issue = subject(SubjectKind::Issue, 1);
    let linked = list(&cell, pull, None, 50).await;
    assert_eq!(linked.items[0].work.subject, issue);
    assert_eq!(
        list(&cell, issue, None, 50).await.items[0].work.subject,
        pull
    );
    let a = activity(&cell, Some(issue)).await;
    let b = activity(&cell, Some(pull)).await;
    assert_eq!(a[0], b[0]);
    assert_eq!(activity(&cell, None).await.len(), 3);
    assert!(matches!(
        a[0].action,
        activity::Action::IssueLinked { number: 1, .. }
    ));
    assert!(matches!(
        write::<SetLink>(&cell, selection(1, false, 1)).await,
        Outcome::Applied(_)
    ));
    assert!(list(&cell, issue, None, 50).await.items.is_empty());
    assert!(list(&cell, pull, None, 50).await.items.is_empty());
    assert_eq!(write::<SetLink>(&cell, input).await, Outcome::Conflict);
    assert!(matches!(
        write::<SetLink>(&cell, selection(1, true, 2)).await,
        Outcome::Applied(_)
    ));
    assert_eq!(
        write::<SetLink>(&cell, selection(1, false, 1)).await,
        Outcome::Conflict
    );
    let current = cell
        .client
        .query::<GetLink>(&cell.target, None, pair)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!((current.active, current.version), (true, 3));
    assert_eq!(activity(&cell, None).await.len(), 5);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_work_uses_live_subject_summaries_and_exclusive_pages() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 3).await;
    for number in 1..=3 {
        write::<SetLink>(&cell, selection(number, true, 0)).await;
    }
    let pull = subject(SubjectKind::Pull, 1);
    let newest = list(&cell, pull, None, 1).await;
    assert_eq!(newest.items[0].work.subject.number, 3);
    let older = list(&cell, pull, newest.next, 1).await;
    assert_eq!(older.items[0].work.subject.number, 2);
    assert_eq!(
        list(&cell, pull, older.next, 1).await.items[0]
            .work
            .subject
            .number,
        1
    );
    write::<UpdateIssue>(
        &cell,
        UpdateIssueInput {
            state_reason: None,
            number: 2,
            version: 1,
            actor: actor(),
            can_triage: false,
            can_moderate: false,
            title: Some("Renamed".into()),
            body: None,
            state: Some(1),
            label_ids: None,
            assignees: None,
        },
    )
    .await;
    let current = list(&cell, pull, None, 50).await;
    assert_eq!(
        (
            current.items[1].work.title.as_str(),
            current.items[1].work.state
        ),
        ("Renamed", super::super::SubjectState::Closed)
    );
    assert_eq!(current.next, None);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_completion_closes_captured_issues_once() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 4).await;
    for number in [1, 2] {
        let mut input = selection(number, true, 0);
        input.closes_on_merge = Some(number == 1);
        assert!(matches!(
            write::<SetLink>(&cell, input).await,
            Outcome::Applied(_)
        ));
    }
    let change_body = |version, body: &str| UpdatePullInput {
        number: 1,
        actor: actor(),
        can_triage: false,
        version,
        title: None,
        body: Some(body.into()),
        state: None,
        reopen_available: false,
        label_ids: None,
        assignees: None,
    };
    assert!(matches!(
        write::<UpdatePull>(&cell, change_body(1, "Fixes #1 and closes #4. `Fixes #2`.")).await,
        UpdatePullOutcome::Updated(_)
    ));
    let pull = subject(SubjectKind::Pull, 1);
    let initial = list(&cell, pull, None, 2).await;
    assert_eq!(
        initial
            .items
            .iter()
            .map(|link| (
                link.work.subject.number,
                link.version,
                link.from_description
            ))
            .collect::<Vec<_>>(),
        [(4, None, true), (2, Some(1), false)]
    );
    assert_eq!(initial.next, Some(2));
    assert_eq!(
        list(&cell, pull, initial.next, 2).await.items[0].version,
        Some(1)
    );
    let keyword_issue = subject(SubjectKind::Issue, 4);
    assert!(list(&cell, keyword_issue, None, 1).await.items[0].from_description);
    for (version, base, oid) in [(2, "refs/heads/release", "c"), (3, "refs/heads/main", "a")] {
        let retarget = write::<pull_base::Retarget>(
            &cell,
            pull_base::Input {
                pull: 1,
                actor: actor(),
                can_write: true,
                comparison_current: true,
                change: pull_base::Change {
                    request_id: Uuid::now_v7(),
                    version,
                    base_ref: base.into(),
                    base_oid: oid.repeat(40),
                    head_oid: "b".repeat(40),
                },
                code_owner_requests: Default::default(),
            },
        )
        .await;
        assert!(matches!(retarget, pull_base::Outcome::Saved));
        assert_eq!(
            !list(&cell, keyword_issue, None, 1).await.items.is_empty(),
            base == "refs/heads/main"
        );
    }
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        pull_version: 4,
        method: MergeMethod::FastForward,
        base_oid: "a".repeat(40),
        head_oid: "b".repeat(40),
        commit_oid: "b".repeat(40),
        message: String::new(),
        created_at_ms: 1,
        publication_plan: None,
        bypass_rulesets: Vec::new(),
        tested_candidate: false,
    };
    let transition = |transition| TransitionPullMergeInput {
        pull: 1,
        merge: merge.clone(),
        transition,
    };
    assert!(matches!(
        write::<TransitionPullMerge>(&cell, transition(PullMergeTransition::Begin)).await,
        TransitionPullMergeOutcome::Applied(_)
    ));
    assert!(
        write::<CaptureMergeEffects>(
            &cell,
            CaptureMergeEffectsInput {
                pull: 1,
                request_id: merge.request_id,
                on_default: true,
                cleanup: None,
            }
        )
        .await
    );
    // Recovery uses the snapshot taken before publication despite later edits.
    assert!(matches!(
        write::<SetLink>(&cell, selection(1, false, 1)).await,
        Outcome::Applied(_)
    ));
    let mut late = selection(3, true, 0);
    late.closes_on_merge = Some(true);
    assert!(matches!(
        write::<SetLink>(&cell, late).await,
        Outcome::Applied(_)
    ));
    for _ in 0..2 {
        assert!(matches!(
            write::<TransitionPullMerge>(&cell, transition(PullMergeTransition::Complete)).await,
            TransitionPullMergeOutcome::Applied(_)
        ));
    }
    for (number, expected) in [(1, 1), (2, 0), (3, 0), (4, 1)] {
        let issue = cell
            .client
            .query::<GetIssue>(&cell.target, None, number)
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!(issue.state, expected);
    }
    let history = activity(&cell, Some(subject(SubjectKind::Issue, 1))).await;
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(
                event.action,
                activity::Action::IssueClosed { pull: Some(1), .. }
            ))
            .count(),
        1
    );
    let merged = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert!(matches!(
        write::<UpdatePull>(&cell, change_body(merged.version, "Fixes #2.")).await,
        UpdatePullOutcome::Updated(_)
    ));
    let historical = cell
        .client
        .query::<ListLinks>(
            &cell.target,
            None,
            Query {
                subject: pull,
                before: None,
                limit: 10,
                default_ref: "refs/heads/renamed".into(),
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        historical
            .items
            .iter()
            .filter(|link| link.from_description)
            .map(|link| link.work.subject.number)
            .collect::<Vec<_>>(),
        [4, 1]
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_duplicate_links_create_one_membership_and_activity_event() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 1).await;
    let results = futures_util::future::join_all(
        (0..12).map(|_| write::<SetLink>(&cell, selection(1, true, 0))),
    )
    .await;
    assert!(
        results
            .iter()
            .all(|result| matches!(result,Outcome::Applied(link) if link.version==1))
    );
    assert_eq!(
        list(&cell, subject(SubjectKind::Pull, 1), None, 50)
            .await
            .items
            .len(),
        1
    );
    assert_eq!(activity(&cell, None).await.len(), 3);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_invalid_and_archived_links_leave_state_and_activity_unchanged() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell, 1).await;
    assert_eq!(
        write::<SetLink>(&cell, selection(2, true, 0)).await,
        Outcome::NotFound
    );
    assert!(
        cell.client
            .command::<SetLink>(&cell.target, identity(), selection(0, true, 0))
            .await
            .is_err()
    );
    for (limit, before) in [(0, None), (51, None), (1, Some(0))] {
        assert!(
            cell.client
                .query::<ListLinks>(
                    &cell.target,
                    None,
                    Query {
                        subject: subject(SubjectKind::Pull, 1),
                        before,
                        limit,
                        default_ref: "refs/heads/main".into(),
                    }
                )
                .await
                .is_err()
        );
    }
    let input = selection(1, true, 0);
    let request = identity();
    let first = cell
        .client
        .command::<SetLink>(&cell.target, request, input.clone())
        .await
        .unwrap();
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    assert_eq!(
        cell.client
            .command::<SetLink>(&cell.target, request, input)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        write::<SetLink>(&cell, selection(1, false, 1)).await,
        Outcome::Archived
    );
    assert_eq!(
        list(&cell, subject(SubjectKind::Pull, 1), None, 50)
            .await
            .items
            .len(),
        1
    );
    assert_eq!(activity(&cell, None).await.len(), 3);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_restart_restores_both_indexes_tombstones_and_receipts() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    seed(&cell, 2).await;
    write::<SetLink>(&cell, selection(1, true, 0)).await;
    write::<SetLink>(&cell, selection(2, true, 0)).await;
    let input = selection(2, false, 1);
    let request = identity();
    let committed = cell
        .client
        .command::<SetLink>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let before = activity(&cell, None).await;
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(
        cell.client
            .command::<SetLink>(&cell.target, request, input)
            .await
            .unwrap(),
        committed
    );
    assert_eq!(
        list(&cell, subject(SubjectKind::Pull, 1), None, 50)
            .await
            .items[0]
            .work
            .subject
            .number,
        1
    );
    assert_eq!(
        list(&cell, subject(SubjectKind::Issue, 1), None, 50)
            .await
            .items[0]
            .work
            .subject
            .number,
        1
    );
    assert!(
        list(&cell, subject(SubjectKind::Issue, 2), None, 50)
            .await
            .items
            .is_empty()
    );
    assert_eq!(
        write::<SetLink>(&cell, selection(2, true, 0)).await,
        Outcome::Conflict
    );
    assert_eq!(activity(&cell, None).await, before);
    let NodeOutcome::Stopped(report) = restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap()
    else {
        panic!("restored node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
}
