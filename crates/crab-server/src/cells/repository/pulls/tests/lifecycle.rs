use crate::cells::repository::collaboration::activity::{ListActivity, Query};
use crate::cells::repository::*;
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::test_support::server as fixtures;
use serde::Serialize;
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
        .repository_cells()
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
fn input() -> CreatePullInput {
    CreatePullInput {
        draft: false,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        title: "Lifecycle".into(),
        body: "Original".into(),
        base_ref: "refs/heads/main".into(),
        head_ref: "refs/heads/feature".into(),
        base_oid: "a".repeat(40),
        head_oid: "b".repeat(40),
    }
}
fn thread() -> CreatePullReviewThreadInput {
    CreatePullReviewThreadInput {
        pull: 1,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        body: "Thread".into(),
        suggested_text: None,
        base_oid: "a".repeat(40),
        head_oid: "b".repeat(40),
        path: b"README.md".to_vec(),
        old_blob_oid: None,
        new_blob_oid: Some("c".repeat(40)),
        side: PullReviewThreadSide::New,
        start_line: 1,
        end_line: 1,
    }
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
fn edit(version: u64, title: &str) -> UpdatePullInput {
    UpdatePullInput {
        number: 1,
        actor: actor("Alice"),
        can_manage: false,
        version,
        title: Some(title.into()),
        body: None,
        state: None,
        label_ids: None,
        assignee_subjects: None,
    }
}
async fn rejected<M: Mutation>(cell: &RepositoryCell, revision: u64, input: M::Input)
where
    M::Output: Serialize,
{
    let result = cell
        .client
        .command::<M>(&cell.target, identity(), input)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(result.output).unwrap(),
        serde_json::json!("Archived"),
        "{}",
        std::any::type_name::<M>()
    );
    assert_eq!(result.receipt.revision, revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_fresh_pull_mutation_obeys_archive_while_committed_receipts_replay() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let input = input();
    let request = identity();
    let created = cell
        .client
        .command::<CreatePull>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert!(matches!(
        write::<CreatePullComment>(
            &cell,
            CreatePullCommentInput {
                pull: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Alice"),
                body: "Comment".into(),
            },
        )
        .await,
        CreatePullCommentOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePullReview>(
            &cell,
            CreatePullReviewInput {
                pull: 1,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Bob"),
                body: "Review".into(),
                state: ReviewState::Commented,
                commit_oid: "b".repeat(40),
            },
        )
        .await,
        CreatePullReviewOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreatePullReviewThread>(&cell, thread()).await,
        CreatePullReviewThreadOutcome::Created(_)
    ));
    assert!(matches!(
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
        .await,
        CreatePullReviewReplyOutcome::Created(_)
    ));
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    let events = cell
        .client
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
        .output;
    let archived = cell
        .client
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
    let revision = archived.receipt.revision;
    assert_eq!(
        cell.client
            .command::<CreatePull>(&cell.target, request, input.clone())
            .await
            .unwrap(),
        created
    );
    rejected::<CreatePull>(&cell, revision, input).await;
    rejected::<UpdatePull>(&cell, revision, edit(pull.version, "Changed")).await;
    rejected::<CreatePullComment>(
        &cell,
        revision,
        CreatePullCommentInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            body: "New comment".into(),
        },
    )
    .await;
    rejected::<UpdatePullComment>(
        &cell,
        revision,
        UpdatePullCommentInput {
            key: PullChildKey { pull: 1, number: 1 },
            actor: actor("Alice"),
            version: 1,
            body: "Changed".into(),
        },
    )
    .await;
    rejected::<CreatePullReview>(
        &cell,
        revision,
        CreatePullReviewInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Bob"),
            body: "New review".into(),
            state: ReviewState::Commented,
            commit_oid: "b".repeat(40),
        },
    )
    .await;
    rejected::<UpdatePullReview>(
        &cell,
        revision,
        UpdatePullReviewInput {
            key: PullChildKey { pull: 1, number: 1 },
            actor: actor("Bob"),
            version: 1,
            body: "Changed".into(),
        },
    )
    .await;
    rejected::<CreatePullReviewThread>(&cell, revision, thread()).await;
    rejected::<UpdatePullReviewThread>(
        &cell,
        revision,
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
    rejected::<CreatePullReviewReply>(
        &cell,
        revision,
        CreatePullReviewReplyInput {
            pull: 1,
            thread: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            body: "New reply".into(),
        },
    )
    .await;
    rejected::<UpdatePullReviewReply>(
        &cell,
        revision,
        UpdatePullReviewReplyInput {
            key: PullReviewReplyKey {
                pull: 1,
                thread: 1,
                number: 1,
            },
            actor: actor("Alice"),
            version: 1,
            body: "Changed".into(),
        },
    )
    .await;
    rejected::<SetPullReadiness>(
        &cell,
        revision,
        SetPullReadinessInput {
            number: 1,
            actor: actor("Alice"),
            can_manage: false,
            version: pull.version,
            state: PullReadiness::Draft,
        },
    )
    .await;
    let merge = merge(&pull);
    rejected::<ReservePullMerge>(
        &cell,
        revision,
        ReservePullMergeInput {
            pull: 1,
            merge: merge.clone(),
        },
    )
    .await;
    rejected::<TransitionPullMerge>(
        &cell,
        revision,
        TransitionPullMergeInput {
            pull: 1,
            merge,
            transition: PullMergeTransition::Begin,
        },
    )
    .await;
    assert_eq!(
        cell.client
            .query::<GetPull>(&cell.target, None, 1)
            .await
            .unwrap()
            .output,
        Some(pull)
    );
    assert_eq!(
        cell.client
            .query::<ListActivity>(
                &cell.target,
                None,
                Query {
                    subject: None,
                    before: None,
                    limit: 50
                }
            )
            .await
            .unwrap()
            .output,
        events
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_can_abort_admitted_work_without_allowing_new_merge_admission() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    let CreatePullOutcome::Created(pull) = write::<CreatePull>(&cell, input()).await else {
        panic!("pull not created")
    };
    let merge = merge(&pull);
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
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    let TransitionPullMergeOutcome::Applied(restored) = write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge: merge.clone(),
            transition: PullMergeTransition::Abort,
        },
    )
    .await
    else {
        panic!("abort failed")
    };
    assert!(restored.merge_pending.is_none());
    assert_eq!(restored.state, PullState::Open);
    assert_eq!(
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: 1,
                merge,
                transition: PullMergeTransition::Begin
            }
        )
        .await,
        TransitionPullMergeOutcome::Archived
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_archive_and_pull_edits_follow_their_durable_receipt_order() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<CreatePull>(&cell, input()).await;
    let requests = (0..8)
        .map(|number| (identity(), edit(1, &format!("Edit {number}"))))
        .collect::<Vec<_>>();
    let changes = futures_util::future::join_all(requests.iter().map(|(request, input)| {
        cell.client
            .command::<UpdatePull>(&cell.target, *request, input.clone())
    }));
    let (archived, changed) = tokio::join!(
        cell.client.command::<ReplaceRepositoryLifecycle>(
            &cell.target,
            identity(),
            ReplaceRepositoryLifecycleInput {
                expected_version: 0,
                archived: true
            }
        ),
        changes
    );
    let archived = archived.unwrap();
    for ((request, input), outcome) in requests.into_iter().zip(changed) {
        let outcome = outcome.unwrap();
        if outcome.receipt.revision >= archived.receipt.revision {
            assert_eq!(outcome.output, UpdatePullOutcome::Archived);
        } else {
            assert!(matches!(
                outcome.output,
                UpdatePullOutcome::Updated(_) | UpdatePullOutcome::Conflict
            ));
        }
        assert_eq!(
            cell.client
                .command::<UpdatePull>(&cell.target, request, input)
                .await
                .unwrap(),
            outcome
        );
    }
    fixtures::close(&server).await;
}
