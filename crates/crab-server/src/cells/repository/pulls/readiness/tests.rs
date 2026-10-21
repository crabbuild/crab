use super::*;
use crate::cells::repository::collaboration::activity::{ListActivity, Query};
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn actor(subject: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: subject.into(),
        name: subject.into(),
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
fn input(draft: bool) -> CreatePullInput {
    CreatePullInput {
        draft,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        title: "Draft workflow".into(),
        body: "Initial body".into(),
        base_ref: "refs/heads/main".into(),
        head_ref: "refs/heads/feature".into(),
        base_oid: "a".repeat(40),
        head_oid: "b".repeat(40),
    }
}
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    cell.client
        .command::<M>(&cell.target, identity(), input)
        .await
        .unwrap()
        .output
}
async fn create(cell: &RepositoryCell, draft: bool) -> PullRecord {
    let CreatePullOutcome::Created(pull) = write::<CreatePull>(cell, input(draft)).await else {
        panic!("pull was not created")
    };
    *pull
}
fn selection(pull: &PullRecord, state: PullReadiness) -> SetPullReadinessInput {
    SetPullReadinessInput {
        number: pull.number,
        version: pull.version,
        actor: actor("Alice"),
        can_manage: false,
        state,
    }
}
async fn state(cell: &RepositoryCell, number: u64) -> PullReadiness {
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, number)
        .await
        .unwrap()
        .output
        .unwrap();
    if pull.draft {
        PullReadiness::Draft
    } else {
        PullReadiness::Ready
    }
}
async fn events(cell: &RepositoryCell) -> Vec<activity::Event> {
    cell.client
        .query::<ListActivity>(
            &cell.target,
            None,
            Query {
                subject: None,
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
fn merge(pull: &PullRecord) -> PullMerge {
    PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        method: MergeMethod::FastForward,
        pull_version: pull.version,
        base_oid: pull.base_oid.clone(),
        head_oid: pull.head_oid.clone(),
        commit_oid: pull.head_oid.clone(),
        message: String::new(),
        created_at_ms: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn draft_creation_and_mode_conflicts_preserve_the_original_submission() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let input = input(true);
    let request = identity();
    let committed = cell
        .client
        .command::<CreatePull>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert_eq!(
        committed,
        cell.client
            .command::<CreatePull>(&cell.target, request, input.clone())
            .await
            .unwrap()
    );
    assert_eq!(state(&cell, 1).await, PullReadiness::Draft);
    let listed = cell
        .client
        .query::<ListPulls>(
            &cell.target,
            None,
            SearchInput {
                before: None,
                limit: 50,
                query: None,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert!(listed.items[0].draft);
    let mut changed = input.clone();
    changed.draft = false;
    assert_eq!(
        write::<CreatePull>(&cell, changed).await,
        CreatePullOutcome::RequestConflict
    );
    let CreatePullOutcome::Created(pull) = committed.output else {
        panic!("pull was not created")
    };
    let SetPullReadinessOutcome::Applied(ready) =
        write::<SetPullReadiness>(&cell, selection(&pull, PullReadiness::Ready)).await
    else {
        panic!("pull was not marked ready")
    };
    assert_eq!(ready.version, 2);
    let CreatePullOutcome::Created(replayed) = write::<CreatePull>(&cell, input).await else {
        panic!("creation replay failed")
    };
    assert_eq!(replayed.version, 2);
    assert_eq!(state(&cell, 1).await, PullReadiness::Ready);
    assert_eq!(
        events(&cell)
            .await
            .iter()
            .map(|e| e.action.clone())
            .collect::<Vec<_>>(),
        vec![Action::ReadyForReview, Action::Created]
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_transitions_are_versioned_idempotent_and_authorized() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, false).await;
    let mut change = selection(&pull, PullReadiness::Draft);
    change.actor = actor("Bob");
    assert_eq!(
        write::<SetPullReadiness>(&cell, change.clone()).await,
        SetPullReadinessOutcome::Forbidden
    );
    change.can_manage = true;
    let SetPullReadinessOutcome::Applied(draft) =
        write::<SetPullReadiness>(&cell, change.clone()).await
    else {
        panic!("writer conversion failed")
    };
    assert_eq!(draft.version, 2);
    assert_eq!(
        write::<SetPullReadiness>(&cell, change).await,
        SetPullReadinessOutcome::Applied(draft.clone())
    );
    assert_eq!(
        write::<SetPullReadiness>(&cell, selection(&pull, PullReadiness::Ready)).await,
        SetPullReadinessOutcome::Conflict
    );
    let SetPullReadinessOutcome::Applied(ready) =
        write::<SetPullReadiness>(&cell, selection(&draft, PullReadiness::Ready)).await
    else {
        panic!("ready conversion failed")
    };
    assert_eq!(ready.version, 3);
    assert_eq!(events(&cell).await.len(), 3);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn drafts_reject_both_merge_reservation_and_fresh_admission() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, true).await;
    let merge = merge(&pull);
    assert_eq!(
        write::<ReservePullMerge>(
            &cell,
            ReservePullMergeInput {
                pull: 1,
                merge: merge.clone()
            }
        )
        .await,
        ReservePullMergeOutcome::Draft
    );
    assert_eq!(
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: 1,
                merge: merge.clone(),
                transition: PullMergeTransition::Begin
            }
        )
        .await,
        TransitionPullMergeOutcome::Draft
    );
    assert!(
        cell.client
            .query::<GetPullMergeSubmission>(
                &cell.target,
                None,
                PullSubmissionKey {
                    pull: Some(1),
                    submission_id: merge.request_id
                }
            )
            .await
            .unwrap()
            .output
            .is_none()
    );
    assert_eq!(events(&cell).await.len(), 1);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conversion_after_reservation_invalidates_a_stale_merge_and_pending_merges_block_conversion()
 {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, false).await;
    let original = merge(&pull);
    write::<ReservePullMerge>(
        &cell,
        ReservePullMergeInput {
            pull: 1,
            merge: original.clone(),
        },
    )
    .await;
    let SetPullReadinessOutcome::Applied(draft) =
        write::<SetPullReadiness>(&cell, selection(&pull, PullReadiness::Draft)).await
    else {
        panic!("draft conversion failed")
    };
    assert_eq!(
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: 1,
                merge: original.clone(),
                transition: PullMergeTransition::Begin
            }
        )
        .await,
        TransitionPullMergeOutcome::Draft
    );
    let SetPullReadinessOutcome::Applied(ready) =
        write::<SetPullReadiness>(&cell, selection(&draft, PullReadiness::Ready)).await
    else {
        panic!("ready conversion failed")
    };
    assert_eq!(
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: 1,
                merge: original,
                transition: PullMergeTransition::Begin
            }
        )
        .await,
        TransitionPullMergeOutcome::Conflict
    );
    let fresh = merge(&ready);
    write::<ReservePullMerge>(
        &cell,
        ReservePullMergeInput {
            pull: 1,
            merge: fresh.clone(),
        },
    )
    .await;
    let TransitionPullMergeOutcome::Applied(pending) = write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge: fresh.clone(),
            transition: PullMergeTransition::Begin,
        },
    )
    .await
    else {
        panic!("merge admission failed")
    };
    assert_eq!(
        write::<SetPullReadiness>(&cell, selection(&pending, PullReadiness::Draft)).await,
        SetPullReadinessOutcome::MergePending
    );
    // Native completion represents already-published Git work. Archive cannot
    // strand that completion or turn an actual merge into an open PR.
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    let TransitionPullMergeOutcome::Applied(done) = write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge: fresh,
            transition: PullMergeTransition::Complete,
        },
    )
    .await
    else {
        panic!("admitted merge did not complete")
    };
    assert_eq!(done.state, PullState::Merged);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_conversion_and_merge_cannot_admit_a_draft() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    for _ in 0..8 {
        let pull = create(&cell, false).await;
        let merge = merge(&pull);
        write::<ReservePullMerge>(
            &cell,
            ReservePullMergeInput {
                pull: pull.number,
                merge: merge.clone(),
            },
        )
        .await;
        let (converted, begun) = tokio::join!(
            write::<SetPullReadiness>(&cell, selection(&pull, PullReadiness::Draft)),
            write::<TransitionPullMerge>(
                &cell,
                TransitionPullMergeInput {
                    pull: pull.number,
                    merge,
                    transition: PullMergeTransition::Begin
                }
            )
        );
        assert!(matches!(
            (converted, begun),
            (
                SetPullReadinessOutcome::Applied(_),
                TransitionPullMergeOutcome::Draft
            ) | (
                SetPullReadinessOutcome::MergePending,
                TransitionPullMergeOutcome::Applied(_)
            )
        ));
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_closed_and_archived_pulls_reject_readiness_changes() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, false).await;
    let mut missing = selection(&pull, PullReadiness::Draft);
    missing.number = 2;
    assert_eq!(
        write::<SetPullReadiness>(&cell, missing).await,
        SetPullReadinessOutcome::NotFound
    );
    let UpdatePullOutcome::Updated(closed) = write::<UpdatePull>(
        &cell,
        UpdatePullInput {
            number: 1,
            actor: actor("Alice"),
            can_triage: false,
            version: 1,
            title: None,
            body: None,
            state: Some(PullState::Closed),
            label_ids: None,
            assignee_subjects: None,
        },
    )
    .await
    else {
        panic!("pull did not close")
    };
    assert_eq!(
        write::<SetPullReadiness>(&cell, selection(&closed, PullReadiness::Draft)).await,
        SetPullReadinessOutcome::Closed
    );
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    assert_eq!(
        write::<SetPullReadiness>(&cell, selection(&closed, PullReadiness::Draft)).await,
        SetPullReadinessOutcome::Archived
    );
    assert_eq!(state(&cell, 1).await, PullReadiness::Ready);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_restart_restores_draft_readiness_and_command_receipts() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    let input = input(true);
    let request = identity();
    let committed = cell
        .client
        .command::<CreatePull>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(state(&cell, 1).await, PullReadiness::Draft);
    assert_eq!(
        cell.client
            .command::<CreatePull>(&cell.target, request, input)
            .await
            .unwrap(),
        committed
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
async fn closing_and_reopening_a_draft_preserves_its_merge_block() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, true).await;
    let close = UpdatePullInput {
        number: pull.number,
        actor: actor("Alice"),
        can_triage: false,
        version: pull.version,
        title: None,
        body: None,
        state: Some(PullState::Closed),
        label_ids: None,
        assignee_subjects: None,
    };
    let UpdatePullOutcome::Updated(closed) = write::<UpdatePull>(&cell, close.clone()).await else {
        panic!("draft did not close")
    };
    assert_eq!(state(&cell, pull.number).await, PullReadiness::Draft);
    let reopen = UpdatePullInput {
        version: closed.version,
        state: Some(PullState::Open),
        ..close
    };
    let UpdatePullOutcome::Updated(open) = write::<UpdatePull>(&cell, reopen).await else {
        panic!("draft did not reopen")
    };
    assert_eq!(state(&cell, pull.number).await, PullReadiness::Draft);
    assert_eq!(
        write::<ReservePullMerge>(
            &cell,
            ReservePullMergeInput {
                pull: open.number,
                merge: merge(&open)
            }
        )
        .await,
        ReservePullMergeOutcome::Draft
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_readiness_command_replies_decode_as_ready() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let outcome = write::<CreatePull>(&cell, input(false)).await;
    let mut stored = serde_json::to_value(&outcome).unwrap();
    stored["Created"].as_object_mut().unwrap().remove("draft");
    let decoded: CreatePullOutcome = serde_json::from_value(stored).unwrap();
    assert_eq!(decoded, outcome);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_and_version_share_one_native_read_snapshot() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let pull = create(&cell, false).await;
    let writer = async {
        let mut pull = Box::new(pull);
        for _ in 0..16 {
            let next = if pull.draft {
                PullReadiness::Ready
            } else {
                PullReadiness::Draft
            };
            let SetPullReadinessOutcome::Applied(updated) =
                write::<SetPullReadiness>(&cell, selection(&pull, next)).await
            else {
                panic!("conversion failed")
            };
            pull = updated;
        }
    };
    let reader = async {
        for _ in 0..32 {
            let pull = cell
                .client
                .query::<GetPull>(&cell.target, None, 1)
                .await
                .unwrap()
                .output
                .unwrap();
            assert_eq!(pull.draft, pull.version % 2 == 0);
        }
    };
    tokio::join!(writer, reader);
    fixtures::close(&server).await;
}
