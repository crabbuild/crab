use super::*;
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn author(subject: &str) -> RepositoryAuthor {
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
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    cell.client
        .command::<M>(&cell.target, identity(), input)
        .await
        .unwrap()
        .output
}
async fn seed(cell: &RepositoryCell) {
    let input = CreatePullInput {
        draft: false,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: author("Alice"),
        title: "Review requests".into(),
        body: String::new(),
        base_ref: "refs/heads/main".into(),
        base_oid: "a".repeat(40),
        head_ref: "refs/heads/topic".into(),
        head_oid: "b".repeat(40),
    };
    assert!(matches!(
        write::<CreatePull>(cell, input).await,
        CreatePullOutcome::Created(_)
    ));
}
fn request(subject: &str, version: u64, requested: bool) -> Selection {
    Selection {
        pull: 1,
        version,
        actor: author("Alice"),
        can_manage: true,
        reviewer: author(subject),
        requested,
    }
}
async fn page(cell: &RepositoryCell) -> super::super::reviewers::Page {
    use super::super::reviewers::{ListReviewers, Query};
    cell.client
        .query::<ListReviewers>(
            &cell.target,
            None,
            Query {
                pull: 1,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
async fn stored(cell: &RepositoryCell, subject: &str) -> Request {
    cell.client
        .query::<GetReviewRequest>(
            &cell.target,
            None,
            Key {
                pull: 1,
                id: reviewer_id(&author(subject)),
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
fn review(subject: &str, state: ReviewState) -> CreatePullReviewInput {
    CreatePullReviewInput {
        pull: 1,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: author(subject),
        body: "Review feedback".into(),
        state,
        commit_oid: "b".repeat(40),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn qualified_requests_are_versioned_and_do_not_follow_display_names() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let input = request("Bob", 1, true);
    let id = identity();
    let committed = cell
        .client
        .command::<SetReviewRequest>(&cell.target, id, input.clone())
        .await
        .unwrap();
    assert_eq!(
        committed,
        cell.client
            .command::<SetReviewRequest>(&cell.target, id, input.clone())
            .await
            .unwrap()
    );
    let mut renamed = input.clone();
    renamed.reviewer.name = "Renamed Bob".into();
    assert_eq!(
        write::<SetReviewRequest>(&cell, renamed.clone()).await,
        committed.output
    );
    assert_eq!(page(&cell).await.version, 2);
    let mut other_issuer = input.clone();
    other_issuer.reviewer.issuer = "https://other.test".into();
    other_issuer.version = 2;
    assert!(matches!(
        write::<SetReviewRequest>(&cell, other_issuer).await,
        Outcome::Applied(_)
    ));
    assert_eq!(page(&cell).await.items.len(), 2);
    assert_eq!(
        write::<SetReviewRequest>(&cell, request("Bob", 1, false)).await,
        Outcome::Conflict
    );
    renamed.version = 3;
    renamed.requested = false;
    assert!(matches!(
        write::<SetReviewRequest>(&cell, renamed).await,
        Outcome::Applied(Request {
            state: State::Cancelled,
            ..
        })
    ));
    assert_eq!(stored(&cell, "Bob").await.state, State::Cancelled);
    assert!(matches!(
        write::<SetReviewRequest>(&cell, request("Bob", 4, true)).await,
        Outcome::Applied(_)
    ));
    assert_eq!(
        write::<SetReviewRequest>(&cell, request("Bob", 3, false)).await,
        Outcome::Conflict
    );
    let current = page(&cell).await;
    assert_eq!(current.version, 5);
    assert!(
        current
            .items
            .iter()
            .all(|reviewer| reviewer.request.as_ref().unwrap().state == State::Requested)
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_duplicate_requests_create_one_transition_and_one_event() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let (a, b) = tokio::join!(
        write::<SetReviewRequest>(&cell, request("Bob", 1, true)),
        write::<SetReviewRequest>(&cell, request("Bob", 1, true))
    );
    assert_eq!(a, b);
    assert_eq!(page(&cell).await.version, 2);
    let events = cell
        .client
        .query::<activity::ListActivity>(
            &cell.target,
            None,
            activity::Query {
                subject: None,
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        events
            .items
            .iter()
            .filter(|event| matches!(event.action, Action::ReviewRequested { .. }))
            .count(),
        1
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn submitted_reviews_fulfill_once_and_replays_cannot_fulfill_a_new_request() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    write::<SetReviewRequest>(&cell, request("Bob", 1, true)).await;
    let mut same_name = review("Bob", ReviewState::Commented);
    same_name.author.issuer = "https://other.test".into();
    write::<CreatePullReview>(&cell, same_name).await;
    assert_eq!(stored(&cell, "Bob").await.state, State::Requested);
    let input = review("Bob", ReviewState::Commented);
    let id = identity();
    let committed = cell
        .client
        .command::<CreatePullReview>(&cell.target, id, input.clone())
        .await
        .unwrap();
    let current = page(&cell).await;
    assert_eq!(
        (current.version, stored(&cell, "Bob").await.state),
        (3, State::Fulfilled { review: 2 })
    );
    write::<SetReviewRequest>(&cell, request("Bob", 3, true)).await;
    assert_eq!(
        committed,
        cell.client
            .command::<CreatePullReview>(&cell.target, id, input.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        write::<CreatePullReview>(&cell, input).await,
        committed.output
    );
    assert_eq!(stored(&cell, "Bob").await.state, State::Requested);
    write::<CreatePullReview>(&cell, review("Bob", ReviewState::Approved)).await;
    let current = page(&cell).await;
    assert_eq!(
        (current.version, stored(&cell, "Bob").await.state),
        (5, State::Fulfilled { review: 3 })
    );
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(pull.review_decisions[0].review, 3);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_require_triage_authority_and_an_open_unarchived_pull() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let mut input = request("Bob", 1, true);
    input.can_manage = false;
    assert_eq!(
        write::<SetReviewRequest>(&cell, input.clone()).await,
        Outcome::Forbidden
    );
    input.actor = author("Carol");
    assert_eq!(
        write::<SetReviewRequest>(&cell, input.clone()).await,
        Outcome::Forbidden
    );
    input.can_manage = true;
    assert!(matches!(
        write::<SetReviewRequest>(&cell, input).await,
        Outcome::Applied(_)
    ));
    assert_eq!(
        write::<SetReviewRequest>(&cell, request("Alice", 2, true)).await,
        Outcome::OwnReview
    );
    let mut missing = request("Bob", 2, true);
    missing.pull = 2;
    assert_eq!(
        write::<SetReviewRequest>(&cell, missing).await,
        Outcome::NotFound
    );
    let edit = UpdatePullInput {
        number: 1,
        actor: author("Alice"),
        can_triage: false,
        version: 2,
        title: None,
        body: None,
        state: Some(PullState::Closed),
        label_ids: None,
        assignees: None,
    };
    write::<UpdatePull>(&cell, edit).await;
    assert_eq!(
        write::<SetReviewRequest>(&cell, request("Bob", 3, false)).await,
        Outcome::Closed
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
        write::<SetReviewRequest>(&cell, request("Bob", 3, false)).await,
        Outcome::Archived
    );
    assert_eq!(stored(&cell, "Bob").await.state, State::Requested);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_restart_restores_requests_fulfillment_and_receipts() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    seed(&cell).await;
    write::<SetReviewRequest>(&cell, request("Bob", 1, true)).await;
    let input = review("Bob", ReviewState::Commented);
    let id = identity();
    let committed = cell
        .client
        .command::<CreatePullReview>(&cell.target, id, input.clone())
        .await
        .unwrap();
    let before = page(&cell).await;
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    let after = page(&cell).await;
    assert_eq!((after.version, after.items), (before.version, before.items));
    assert_eq!(
        committed,
        cell.client
            .command::<CreatePullReview>(&cell.target, id, input)
            .await
            .unwrap()
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
async fn request_changes_invalidate_merge_admission_and_pending_merges_freeze_requests() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: author("Alice"),
        method: MergeMethod::FastForward,
        pull_version: 1,
        base_oid: "a".repeat(40),
        head_oid: "b".repeat(40),
        commit_oid: "b".repeat(40),
        message: String::new(),
        created_at_ms: 1,
    };
    write::<SetReviewRequest>(&cell, request("Bob", 1, true)).await;
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
        TransitionPullMergeOutcome::Conflict
    );
    let merge = PullMerge {
        pull_version: 2,
        ..merge
    };
    assert!(matches!(
        write::<TransitionPullMerge>(
            &cell,
            TransitionPullMergeInput {
                pull: 1,
                merge,
                transition: PullMergeTransition::Begin
            }
        )
        .await,
        TransitionPullMergeOutcome::Applied(_)
    ));
    assert_eq!(
        write::<SetReviewRequest>(&cell, request("Bob", 3, false)).await,
        Outcome::MergePending
    );
    assert_eq!(
        write::<CreatePullReview>(&cell, review("Bob", ReviewState::Approved)).await,
        CreatePullReviewOutcome::MergePending
    );
    assert_eq!(stored(&cell, "Bob").await.state, State::Requested);
    fixtures::close(&server).await;
}
