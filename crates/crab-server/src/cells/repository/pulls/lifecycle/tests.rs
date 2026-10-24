use super::*;
use crate::cells::RepositoryCell;
use crate::cells::repository::collaboration::{
    Subject, Target,
    activity::{self, ListActivity, Query},
    lifecycle::{PrepareLifecycleInput, PrepareLifecycleOutcome},
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
async fn create(cell: &RepositoryCell) -> PullRecord {
    let CreatePullOutcome::Created(pull) = write::<CreatePull>(
        cell,
        CreatePullInput {
            metadata: Default::default(),
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Lifecycle".into(),
            body: "Original".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await
    else {
        panic!("create rejected")
    };
    *pull
}
fn update(pull: &PullRecord, state: Option<PullState>) -> UpdatePullInput {
    UpdatePullInput {
        number: pull.number,
        version: pull.version,
        actor: actor(),
        can_triage: false,
        title: Some("Edited".into()),
        body: None,
        state,
        label_ids: None,
        assignees: None,
    }
}
async fn change(cell: &RepositoryCell, pull: &PullRecord, state: Option<PullState>) -> PullRecord {
    let UpdatePullOutcome::Updated(pull) = write::<UpdatePull>(cell, update(pull, state)).await
    else {
        panic!("update rejected")
    };
    *pull
}
async fn metadata(cell: &RepositoryCell, number: u64) -> PullLifecycle {
    read::<GetPullDetail>(cell, number).await.unwrap().lifecycle
}
async fn history(cell: &RepositoryCell, number: u64) -> Vec<Event> {
    read::<ListActivity>(
        cell,
        Query {
            subject: Some(Subject {
                kind: SubjectKind::Pull,
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
async fn closure_survives_edits_and_rejections_while_reopening_clears_it() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let original = create(&cell).await;
    let closed = change(&cell, &original, Some(PullState::Closed)).await;
    let expected = PullLifecycle {
        closed_at_ms: Some(closed.updated_at_ms),
        merged_at_ms: None,
    };
    assert_eq!(metadata(&cell, 1).await, expected);
    let edited = change(&cell, &closed, Some(PullState::Closed)).await;
    assert_eq!(metadata(&cell, 1).await, expected);
    let input = update(&original, Some(PullState::Open));
    assert_eq!(
        write::<UpdatePull>(&cell, input).await,
        UpdatePullOutcome::Conflict
    );
    let mut denied = update(&edited, Some(PullState::Open));
    denied.actor.subject = "other".into();
    assert_eq!(
        write::<UpdatePull>(&cell, denied).await,
        UpdatePullOutcome::Forbidden
    );
    assert_eq!(metadata(&cell, 1).await, expected);
    let opened = change(&cell, &edited, Some(PullState::Open)).await;
    assert_eq!(metadata(&cell, 1).await, PullLifecycle::default());
    let closed = change(&cell, &opened, Some(PullState::Closed)).await;
    assert_eq!(
        metadata(&cell, 1).await.closed_at_ms,
        Some(closed.updated_at_ms)
    );
    assert_eq!(
        history(&cell, 1)
            .await
            .iter()
            .filter(|event| event.action == Action::Closed)
            .count(),
        2
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_completion_is_independent_of_reservation_edits_and_replay() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell).await;
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        method: MergeMethod::FastForward,
        pull_version: pull.version,
        base_oid: pull.base_oid.clone(),
        head_oid: pull.head_oid.clone(),
        commit_oid: pull.head_oid.clone(),
        message: String::new(),
        created_at_ms: 1,
    };
    write::<ReservePullMerge>(
        &cell,
        ReservePullMergeInput {
            pull: 1,
            merge: merge.clone(),
        },
    )
    .await;
    write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge: merge.clone(),
            transition: PullMergeTransition::Begin,
        },
    )
    .await;
    assert_eq!(metadata(&cell, 1).await, PullLifecycle::default());
    let complete = TransitionPullMergeInput {
        pull: 1,
        merge: merge.clone(),
        transition: PullMergeTransition::Complete,
    };
    let TransitionPullMergeOutcome::Applied(done) =
        write::<TransitionPullMerge>(&cell, complete.clone()).await
    else {
        panic!("merge rejected")
    };
    let expected = PullLifecycle {
        closed_at_ms: Some(done.updated_at_ms),
        merged_at_ms: Some(done.updated_at_ms),
    };
    assert_ne!(expected.merged_at_ms, Some(merge.created_at_ms));
    assert_eq!(metadata(&cell, 1).await, expected);
    let event = history(&cell, 1)
        .await
        .into_iter()
        .find(|event| event.action == Action::Merged)
        .unwrap();
    assert_eq!(expected.merged_at_ms, Some(event.created_at));
    let edited = change(&cell, &done, None).await;
    write::<TransitionPullMerge>(&cell, complete).await;
    assert_eq!(metadata(&cell, 1).await, expected);
    assert_eq!(read::<GetPull>(&cell, 1).await.unwrap(), edited);
    assert_eq!(
        history(&cell, 1)
            .await
            .iter()
            .filter(|event| event.action == Action::Merged)
            .count(),
        1
    );
    fixtures::close(&server).await;
}

// Recreate the pre-lifecycle schema without changing canonical pulls or receipts.
pub(in crate::cells::repository) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_517;
    type Input = bool;
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        abort: bool,
    ) -> crate::cells::Result<MutationResult<()>> {
        if abort {
            PreparePullLifecycle::execute(context, PrepareLifecycleInput { version: 0 })?;
            return Err(crate::cells::Error::Invalid(
                "injected preparation rollback",
            ));
        }
        for _ in 0..19 {
            activity::append(
                context,
                Target::Pull { number: 1 },
                &actor(),
                Action::Updated,
            )?;
        }
        context.query("UPDATE repository_pulls SET state=1 WHERE number=2", &[])?;
        context.install_schema("DROP TABLE repository_pull_lifecycle;")?;
        context.delete_document("repository/pulls/lifecycle/v1/preparation")?;
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn preparation_is_bounded_atomic_and_preserves_original_receipts_and_unknown_dates() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell).await;
    create(&cell).await;
    let request = crate::app::mutation_identity().unwrap();
    let input = update(&pull, Some(PullState::Closed));
    let closed = cell
        .client
        .command::<UpdatePull>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let source = read::<GetPull>(&cell, 1).await;
    let third = create(&cell).await;
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        method: MergeMethod::FastForward,
        pull_version: third.version,
        base_oid: third.base_oid.clone(),
        head_oid: third.head_oid.clone(),
        commit_oid: third.head_oid.clone(),
        message: String::new(),
        created_at_ms: 1,
    };
    for transition in [PullMergeTransition::Begin, PullMergeTransition::Complete] {
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: third.number,
                merge: merge.clone(),
                transition,
            },
        )
        .await;
    }
    let merged = read::<GetPull>(&cell, third.number).await.unwrap();
    let merge_time = metadata(&cell, third.number).await;
    write::<Historical>(&cell, false).await;
    let events = history(&cell, 1).await;
    assert_eq!(
        read::<GetPullLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Outdated
    );
    assert!(
        cell.client
            .query::<GetPullDetail>(&cell.target, None, 1)
            .await
            .is_err()
    );
    assert!(
        cell.client
            .command::<Historical>(&cell.target, crate::app::mutation_identity().unwrap(), true)
            .await
            .is_err()
    );
    assert_eq!(
        read::<GetPullLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Outdated
    );
    let PrepareLifecycleOutcome::Applied(first) =
        write::<PreparePullLifecycle>(&cell, PrepareLifecycleInput { version: 0 }).await
    else {
        panic!("preparation rejected")
    };
    assert_eq!(
        first,
        LifecycleStatus::Migrating {
            version: 1,
            copied: 0
        }
    );
    assert!(matches!(
        write::<PreparePullLifecycle>(&cell, PrepareLifecycleInput { version: 0 }).await,
        PrepareLifecycleOutcome::Conflict(_)
    ));
    assert!(
        cell.client
            .command::<UpdatePull>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                update(source.as_ref().unwrap(), Some(PullState::Open))
            )
            .await
            .is_err()
    );
    for _ in 0..8 {
        match read::<GetPullLifecycleStatus>(&cell, ()).await {
            LifecycleStatus::Ready => break,
            LifecycleStatus::Migrating { version, .. } => {
                write::<PreparePullLifecycle>(&cell, PrepareLifecycleInput { version }).await;
            }
            _ => panic!("preparation lost progress"),
        }
    }
    assert_eq!(
        read::<GetPullLifecycleStatus>(&cell, ()).await,
        LifecycleStatus::Ready
    );
    assert_eq!(read::<GetPull>(&cell, 1).await, source);
    assert_eq!(
        cell.client
            .command::<UpdatePull>(&cell.target, request, input)
            .await
            .unwrap(),
        closed
    );
    let event = events
        .iter()
        .find(|event| event.action == Action::Closed)
        .unwrap();
    assert_eq!(
        metadata(&cell, 1).await.closed_at_ms,
        Some(event.created_at)
    );
    assert_eq!(metadata(&cell, 2).await, PullLifecycle::default());
    assert_eq!(history(&cell, 1).await, events);
    assert_eq!(metadata(&cell, third.number).await, merge_time);
    assert_eq!(read::<GetPull>(&cell, third.number).await.unwrap(), merged);
    assert_ne!(merge_time.merged_at_ms, Some(merge.created_at_ms));
    fixtures::close(&server).await;
}
