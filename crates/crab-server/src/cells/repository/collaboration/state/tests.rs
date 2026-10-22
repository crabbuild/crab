use super::super::{
    Decision,
    activity::{ListActivity, Query},
    milestones::{
        AssignMilestone, AssignMilestoneInput, CreateMilestone, CreateMilestoneInput, GetMilestone,
    },
};
use super::*;
use crate::cells::{RepositoryCell, application::Read};
use crate::test_support::server as fixtures;
use uuid::Uuid;
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
async fn read<R: Read>(cell: &RepositoryCell, input: R::Input) -> R::Output {
    cell.client
        .query::<R>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
async fn seed(cell: &RepositoryCell) {
    write::<CreateIssue>(
        cell,
        CreateIssueInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Issue".into(),
            body: "Description".into(),
        },
    )
    .await;
    write::<CreatePull>(
        cell,
        CreatePullInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            draft: false,
            title: "Pull".into(),
            body: "Description".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
fn input(kind: SubjectKind) -> Input {
    Input {
        submission_id: *Uuid::now_v7().as_bytes(),
        subject: Subject { kind, number: 1 },
        actor: actor(),
        can_triage: false,
        version: 1,
        state: State::Closed,
        reason: None,
        body: Some("Atomic state context".into()),
    }
}
fn number(outcome: &Outcome) -> u64 {
    match outcome {
        Outcome::Applied(Some(Comment::Issue(value))) => value.number,
        Outcome::Applied(Some(Comment::Pull(value))) => value.number,
        _ => panic!("missing comment"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn both_discussion_kinds_commit_comment_state_progress_and_activity_once() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        let command = input(kind);
        let subject = command.subject;
        let Decision::Applied(milestone) = write::<CreateMilestone>(
            &cell,
            CreateMilestoneInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: format!("Release {kind:?}"),
                description: "".into(),
                due_on: None,
            },
        )
        .await
        else {
            panic!("milestone rejected")
        };
        write::<AssignMilestone>(
            &cell,
            AssignMilestoneInput {
                subject,
                milestone: Some(milestone.number),
                version: 0,
                actor: actor(),
            },
        )
        .await;
        let first = write::<ChangeState>(&cell, command.clone()).await;
        assert_eq!(number(&first), 1);
        let mut replay = command.clone();
        replay.actor.name = "Renamed actor".into();
        replay.can_triage = true;
        assert_eq!(number(&write::<ChangeState>(&cell, replay).await), 1);
        let progress = read::<GetMilestone>(&cell, milestone.number).await.unwrap();
        assert_eq!((progress.open_count, progress.closed_count), (0, 1));
        let events = read::<ListActivity>(
            &cell,
            Query {
                subject: Some(subject),
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
        assert_eq!(events.items.len(), 4);
        let mut reopen = command.clone();
        reopen.submission_id = *Uuid::now_v7().as_bytes();
        reopen.version = 2;
        reopen.state = State::Open;
        reopen.body = Some("Reopened context".into());
        assert_eq!(number(&write::<ChangeState>(&cell, reopen).await), 2);
        let progress = read::<GetMilestone>(&cell, milestone.number).await.unwrap();
        assert_eq!((progress.open_count, progress.closed_count), (1, 0));
        let wrong = Input {
            body: Some("Changed old intent".into()),
            ..command
        };
        assert!(matches!(
            write::<ChangeState>(&cell, wrong).await,
            Outcome::RequestConflict
        ));
    }
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn rejection_rolls_back_child_numbers_history_and_search_contributions() {
    use crate::cells::repository::search::{index::SearchInput, results::ListDiscussions};
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        let command = input(kind);
        let mut denied = command.clone();
        denied.submission_id = *Uuid::now_v7().as_bytes();
        denied.actor.subject = "bob".into();
        denied.body = Some("orphanneedle".into());
        assert!(matches!(
            write::<ChangeState>(&cell, denied.clone()).await,
            Outcome::Rejected(Rejection::Forbidden)
        ));
        denied.can_triage = true;
        assert!(matches!(
            write::<ChangeState>(&cell, denied.clone()).await,
            Outcome::Rejected(Rejection::Forbidden)
        ));
        let changed = Input {
            body: Some("Changed payload".into()),
            ..denied.clone()
        };
        assert!(matches!(
            write::<ChangeState>(&cell, changed).await,
            Outcome::RequestConflict
        ));
        assert!(matches!(
            write::<ChangeState>(&cell, denied).await,
            Outcome::Rejected(Rejection::Forbidden)
        ));
        let bad = Input {
            submission_id: *Uuid::now_v7().as_bytes(),
            reason: Some(Reason::Reopened),
            ..command.clone()
        };
        assert!(matches!(
            write::<ChangeState>(&cell, bad).await,
            Outcome::Rejected(Rejection::InvalidState)
        ));
        assert_eq!(number(&write::<ChangeState>(&cell, command).await), 1);
    }
    assert_eq!(
        read::<ListDiscussions>(
            &cell,
            SearchInput {
                query: Some("orphanneedle".into()),
                ..Default::default()
            }
        )
        .await
        .matching,
        0
    );
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn state_submission_recovery_survives_restart_and_later_archive() {
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
    seed(&cell).await;
    let commands = [input(SubjectKind::Issue), input(SubjectKind::Pull)];
    for command in &commands {
        assert_eq!(
            number(&write::<ChangeState>(&cell, command.clone()).await),
            1
        );
    }
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    for command in commands {
        assert_eq!(
            number(&write::<ChangeState>(&cell, command.clone()).await),
            1
        );
        let fresh = Input {
            submission_id: *Uuid::now_v7().as_bytes(),
            version: 2,
            state: State::Open,
            ..command
        };
        assert!(matches!(
            write::<ChangeState>(&cell, fresh).await,
            Outcome::Rejected(Rejection::Archived)
        ));
    }
    stop(restored.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_retries_and_competing_versions_cannot_duplicate_comments() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        let original = input(kind);
        let retries = futures_util::future::join_all(
            (0..2).map(|_| write::<ChangeState>(&cell, original.clone())),
        )
        .await;
        assert_eq!(retries.iter().map(number).collect::<Vec<_>>(), [1, 1]);
        let candidates = (0..2).map(|_| Input {
            submission_id: *Uuid::now_v7().as_bytes(),
            version: 2,
            state: State::Open,
            ..original.clone()
        });
        let results = futures_util::future::join_all(
            candidates.map(|candidate| write::<ChangeState>(&cell, candidate)),
        )
        .await;
        assert_eq!(
            results
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Applied(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Rejected(Rejection::Conflict)))
                .count(),
            1
        );
        let events = read::<ListActivity>(
            &cell,
            Query {
                subject: Some(original.subject),
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
        assert_eq!(events.items.len(), 5);
    }
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 65_519;
    type Input = (SubjectKind, bool);
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        (kind, corrupt): Self::Input,
    ) -> Result<MutationResult<()>> {
        let rows = context.documents::<Submission>(preparation::prefix(kind), None, 51)?;
        for (index, (key, saved)) in rows.iter().enumerate() {
            let Resolution::Applied { comment } = &saved.resolution else {
                return Err(Error::Invalid(
                    "historical fixture requires applied submissions",
                ));
            };
            let digest = if corrupt && index == rows.len() - 1 {
                "bad"
            } else {
                &saved.digest
            };
            context.put_document(key, &serde_json::json!({"digest":digest,"comment":comment}))?;
        }
        context.delete_document(preparation::key(kind))?;
        Ok(MutationResult::Success(()))
    }
}
pub(in crate::cells::repository) struct Inspect;
impl Read for Inspect {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 65_519;
    type Input = SubjectKind;
    type Output = Vec<(String, serde_json::Value)>;
    fn execute(context: &mut Transaction<'_>, kind: SubjectKind) -> Result<Self::Output> {
        context.documents(preparation::prefix(kind), None, 51)
    }
}
pub(in crate::cells::repository) struct FailedPreparation;
impl Mutation for FailedPreparation {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 65_520;
    type Input = preparation::Input;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<MutationResult<()>> {
        preparation::Prepare::execute(context, input)?;
        Err(Error::Invalid("interrupted state submission preparation"))
    }
}
async fn history(cell: &RepositoryCell, kind: SubjectKind) -> Vec<Input> {
    let mut commands = Vec::new();
    for index in 0..10 {
        let command = Input {
            version: index + 1,
            state: if index % 2 == 0 {
                State::Closed
            } else {
                State::Open
            },
            ..input(kind)
        };
        assert!(matches!(
            write::<ChangeState>(cell, command.clone()).await,
            Outcome::Applied(_)
        ));
        commands.push(command);
    }
    commands
}
#[tokio::test(flavor = "multi_thread")]
async fn state_submission_preparation_is_bounded_atomic_and_preserves_replay_and_history() {
    use crate::cells::repository::collaboration::lifecycle::{
        LifecycleStatus, PrepareLifecycleOutcome,
    };
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        let commands = history(&cell, kind).await;
        let before = serde_json::to_value(
            read::<ListActivity>(
                &cell,
                Query {
                    subject: Some(Subject { kind, number: 1 }),
                    before: None,
                    limit: 50,
                },
            )
            .await,
        )
        .unwrap();
        write::<Historical>(&cell, (kind, false)).await;
        assert_eq!(
            read::<preparation::GetPreparation>(&cell, kind).await,
            LifecycleStatus::Outdated
        );
        assert!(matches!(
            cell.client
                .command::<ChangeState>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    Input {
                        version: 11,
                        ..input(kind)
                    }
                )
                .await
                .err()
                .unwrap(),
            Error::SchemaUnavailable
        ));
        assert!(matches!(
            write::<preparation::Prepare>(&cell, preparation::Input { kind, version: 0 }).await,
            PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
                version: 1,
                copied: 8
            })
        ));
        let staged = read::<Inspect>(&cell, kind).await;
        assert!(
            cell.client
                .command::<FailedPreparation>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    preparation::Input { kind, version: 1 }
                )
                .await
                .is_err()
        );
        assert_eq!(read::<Inspect>(&cell, kind).await, staged);
        assert!(matches!(
            write::<preparation::Prepare>(&cell, preparation::Input { kind, version: 0 }).await,
            PrepareLifecycleOutcome::Conflict(_)
        ));
        assert!(matches!(
            write::<preparation::Prepare>(&cell, preparation::Input { kind, version: 1 }).await,
            PrepareLifecycleOutcome::Applied(LifecycleStatus::Ready)
        ));
        for command in commands {
            let expected = command.version;
            assert_eq!(
                number(&write::<ChangeState>(&cell, command).await),
                expected
            );
        }
        let after = serde_json::to_value(
            read::<ListActivity>(
                &cell,
                Query {
                    subject: Some(Subject { kind, number: 1 }),
                    before: None,
                    limit: 50,
                },
            )
            .await,
        )
        .unwrap();
        assert_eq!(after, before);
        let current = read::<Inspect>(&cell, kind).await;
        for (key, old) in staged {
            let saved = &current
                .iter()
                .find(|(candidate, _)| candidate == &key)
                .unwrap()
                .1;
            assert_eq!(saved["digest"], old["digest"]);
            if old.get("resolution").is_none() {
                assert_eq!(saved["resolution"]["comment"], old["comment"]);
            }
        }
    }
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn corrupt_historical_submission_rolls_back_its_entire_current_frame() {
    use crate::cells::repository::collaboration::lifecycle::{
        LifecycleStatus, PrepareLifecycleOutcome,
    };
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    history(&cell, SubjectKind::Issue).await;
    let kind = SubjectKind::Issue;
    write::<Historical>(&cell, (kind, true)).await;
    assert!(matches!(
        write::<preparation::Prepare>(&cell, preparation::Input { kind, version: 0 }).await,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
            version: 1,
            copied: 8
        })
    ));
    let before = read::<Inspect>(&cell, kind).await;
    assert!(
        cell.client
            .command::<preparation::Prepare>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                preparation::Input { kind, version: 1 }
            )
            .await
            .is_err()
    );
    assert_eq!(read::<Inspect>(&cell, kind).await, before);
    assert_eq!(
        read::<preparation::GetPreparation>(&cell, kind).await,
        LifecycleStatus::Migrating {
            version: 1,
            copied: 8
        }
    );
    fixtures::close(&server).await;
}
