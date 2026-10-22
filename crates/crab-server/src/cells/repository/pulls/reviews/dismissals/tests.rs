use super::*;
use crate::cells::{
    RepositoryCell,
    repository::collaboration::{
        Subject, SubjectKind,
        activity::{ListActivity, Query},
    },
};
use crate::test_support::server as fixtures;
use uuid::Uuid;
fn actor(name: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: name.into(),
        name: name.into(),
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
async fn seed(cell: &RepositoryCell) {
    write::<CreatePull>(
        cell,
        CreatePullInput {
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            title: "Review lifecycle".into(),
            body: "Description".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
async fn review(cell: &RepositoryCell, name: &str, state: ReviewState) -> PullReviewRecord {
    let CreatePullReviewOutcome::Created(review) = write::<CreatePullReview>(
        cell,
        CreatePullReviewInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(name),
            state,
            body: "Review".into(),
            commit_oid: "b".repeat(40),
        },
    )
    .await
    else {
        panic!("review rejected")
    };
    review
}
fn input(review: &PullReviewRecord) -> Input {
    Input {
        submission_id: *Uuid::now_v7().as_bytes(),
        key: PullChildKey {
            pull: review.pull,
            number: review.number,
        },
        version: review.version,
        actor: actor("Writer"),
        body: "The concern has been addressed.".into(),
        can_write: true,
        can_admin: false,
    }
}
async fn events(
    cell: &RepositoryCell,
) -> Vec<crate::cells::repository::collaboration::activity::Event> {
    read::<ListActivity>(
        cell,
        Query {
            subject: Some(Subject {
                kind: SubjectKind::Pull,
                number: 1,
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
async fn dismissal_removes_only_the_exact_active_decision_and_preserves_review_history() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let first = review(&cell, "Bob", ReviewState::Approved).await;
    let newer = review(&cell, "Bob", ReviewState::ChangesRequested).await;
    let comment = review(&cell, "Bob", ReviewState::Commented).await;
    assert_eq!(
        write::<DismissReview>(&cell, input(&first)).await,
        Outcome::Dismissed
    );
    let pull = read::<GetPull>(&cell, 1).await.unwrap();
    assert_eq!(pull.review_decisions[0].review, newer.number);
    assert_eq!(
        write::<DismissReview>(&cell, input(&newer)).await,
        Outcome::Dismissed
    );
    assert!(
        read::<GetPull>(&cell, 1)
            .await
            .unwrap()
            .review_decisions
            .is_empty()
    );
    let details = read::<GetReviewDetail>(
        &cell,
        PullChildKey {
            pull: 1,
            number: first.number,
        },
    )
    .await
    .unwrap();
    assert_eq!(details.review.state, ReviewState::Approved);
    assert_eq!(
        details.dismissal.unwrap().body,
        "The concern has been addressed."
    );
    assert_eq!(
        read::<GetPullReview>(
            &cell,
            PullChildKey {
                pull: 1,
                number: comment.number
            }
        )
        .await
        .unwrap(),
        comment
    );
    assert_eq!(
        events(&cell)
            .await
            .iter()
            .filter(|event| event.action == Action::ReviewDismissed)
            .count(),
        2
    );
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn protected_branch_policy_versions_and_invalid_states_leave_no_dismissal() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let approval = review(&cell, "Bob", ReviewState::Approved).await;
    let comment = review(&cell, "Other", ReviewState::Commented).await;
    let mut denied = input(&approval);
    denied.can_write = false;
    assert_eq!(
        write::<DismissReview>(&cell, denied).await,
        Outcome::Forbidden
    );
    let mut stale = input(&approval);
    stale.version += 1;
    assert_eq!(
        write::<DismissReview>(&cell, stale).await,
        Outcome::Conflict
    );
    assert_eq!(
        write::<DismissReview>(&cell, input(&comment)).await,
        Outcome::InvalidState
    );
    let pull = read::<GetPull>(&cell, 1).await.unwrap();
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        method: MergeMethod::FastForward,
        pull_version: pull.version,
        base_oid: pull.base_oid.clone(),
        head_oid: pull.head_oid.clone(),
        commit_oid: pull.head_oid.clone(),
        message: String::new(),
        created_at_ms: 1,
    };
    write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge: merge.clone(),
            transition: PullMergeTransition::Begin,
        },
    )
    .await;
    assert_eq!(
        write::<DismissReview>(&cell, input(&approval)).await,
        Outcome::MergePending
    );
    assert!(
        read::<GetReviewDetail>(
            &cell,
            PullChildKey {
                pull: 1,
                number: approval.number
            }
        )
        .await
        .unwrap()
        .dismissal
        .is_none()
    );
    write::<TransitionPullMerge>(
        &cell,
        TransitionPullMergeInput {
            pull: 1,
            merge,
            transition: PullMergeTransition::Abort,
        },
    )
    .await;
    write::<ReplaceBranchProtections>(
        &cell,
        ReplaceBranchProtectionsInput {
            expected_version: 0,
            rules: vec![BranchProtectionRecord {
                branch: "main".into(),
                required_approvals: 1,
                required_checks: vec![],
            }],
        },
    )
    .await;
    assert_eq!(
        write::<DismissReview>(&cell, input(&approval)).await,
        Outcome::Forbidden
    );
    let detail = read::<GetReviewDetail>(
        &cell,
        PullChildKey {
            pull: 1,
            number: approval.number,
        },
    )
    .await
    .unwrap();
    assert!(detail.admin_required && detail.dismissal.is_none());
    let mut allowed = input(&approval);
    allowed.can_admin = true;
    assert_eq!(
        write::<DismissReview>(&cell, allowed).await,
        Outcome::Dismissed
    );
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn permanent_recovery_does_not_reapply_the_dismissal_after_a_new_review_or_archive() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let approval = review(&cell, "Bob", ReviewState::Approved).await;
    let original = input(&approval);
    assert_eq!(
        write::<DismissReview>(&cell, original.clone()).await,
        Outcome::Dismissed
    );
    let next = review(&cell, "Bob", ReviewState::Approved).await;
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    let before = read::<GetPull>(&cell, 1).await.unwrap();
    let history = events(&cell).await;
    let mut retry = original.clone();
    retry.actor.name = "Renamed writer".into();
    retry.can_write = false;
    retry.can_admin = false;
    assert_eq!(
        write::<DismissReview>(&cell, retry).await,
        Outcome::Dismissed
    );
    assert_eq!(read::<GetPull>(&cell, 1).await.unwrap(), before);
    assert_eq!(before.review_decisions[0].review, next.number);
    assert_eq!(events(&cell).await, history);
    let mut changed = original.clone();
    changed.body = "Different reason".into();
    assert_eq!(
        write::<DismissReview>(&cell, changed).await,
        Outcome::RequestConflict
    );
    let mut fresh = original;
    fresh.submission_id = *Uuid::now_v7().as_bytes();
    assert_eq!(
        write::<DismissReview>(&cell, fresh).await,
        Outcome::Archived
    );
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_516;
    type Input = Option<Input>;
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<()>> {
        if let Some(input) = input {
            DismissReview::execute(context, input)?;
            return Err(crate::cells::Error::Invalid("injected dismissal rollback"));
        }
        context.install_schema("DROP TABLE repository_pull_review_dismissals;")?;
        context.delete_document(SCHEMA)?;
        Ok(MutationResult::Success(()))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn native_rollback_and_explicit_empty_schema_preparation_preserve_canonical_records() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let approval = review(&cell, "Bob", ReviewState::Approved).await;
    let before = read::<GetPull>(&cell, 1).await.unwrap();
    let history = events(&cell).await;
    assert!(
        cell.client
            .command::<Historical>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                Some(input(&approval))
            )
            .await
            .is_err()
    );
    assert_eq!(read::<GetPull>(&cell, 1).await.unwrap(), before);
    assert_eq!(events(&cell).await, history);
    write::<Historical>(&cell, None).await;
    assert_eq!(
        read::<GetReviewState>(&cell, ()).await,
        LifecycleStatus::Outdated
    );
    assert!(
        cell.client
            .query::<GetReviewDetail>(
                &cell.target,
                None,
                PullChildKey {
                    pull: 1,
                    number: approval.number
                }
            )
            .await
            .is_err()
    );
    write::<PrepareReviewState>(&cell, PrepareLifecycleInput { version: 0 }).await;
    assert_eq!(
        read::<GetReviewState>(&cell, ()).await,
        LifecycleStatus::Ready
    );
    assert_eq!(
        read::<GetPullReview>(
            &cell,
            PullChildKey {
                pull: 1,
                number: approval.number
            }
        )
        .await
        .unwrap(),
        approval
    );
    assert!(
        read::<GetReviewDetail>(
            &cell,
            PullChildKey {
                pull: 1,
                number: approval.number
            }
        )
        .await
        .unwrap()
        .dismissal
        .is_none()
    );
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn bounded_review_pages_keep_full_reasons_without_skipping_large_dismissals() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    for index in 0..10 {
        let review = review(&cell, &format!("Reviewer{index}"), ReviewState::Approved).await;
        let mut change = input(&review);
        change.body = format!("Reason{}", "\t".repeat(65530));
        assert_eq!(
            write::<DismissReview>(&cell, change).await,
            Outcome::Dismissed
        );
    }
    let mut before = None;
    let mut numbers = Vec::new();
    loop {
        let page = read::<ListReviewDetails>(
            &cell,
            PullChildListInput {
                pull: 1,
                before,
                limit: 50,
            },
        )
        .await
        .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < MAX_LIST_OUTPUT_BYTES);
        for item in page.items {
            assert_eq!(item.dismissal.unwrap().body.len(), 65536);
            numbers.push(item.review.number);
        }
        before = page.next;
        if before.is_none() {
            break;
        }
    }
    assert_eq!(numbers, (1..=10).rev().collect::<Vec<_>>());
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn original_review_receipts_replay_without_restoring_a_dismissed_decision() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    seed(&cell).await;
    let input = CreatePullReviewInput {
        pull: 1,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor("Bob"),
        state: ReviewState::Approved,
        body: "Original approval".into(),
        commit_oid: "b".repeat(40),
    };
    let identity = crate::app::mutation_identity().unwrap();
    let created = cell
        .client
        .command::<CreatePullReview>(&cell.target, identity, input.clone())
        .await
        .unwrap();
    let CreatePullReviewOutcome::Created(review) = &created.output else {
        panic!("review rejected")
    };
    let dismissal = self::input(review);
    assert_eq!(
        write::<DismissReview>(&cell, dismissal).await,
        Outcome::Dismissed
    );
    let replay = cell
        .client
        .command::<CreatePullReview>(&cell.target, identity, input.clone())
        .await
        .unwrap();
    assert_eq!(replay, created);
    let CreatePullReviewOutcome::Created(current) = write::<CreatePullReview>(&cell, input).await
    else {
        panic!("review recovery rejected")
    };
    assert_eq!(current.version, 2);
    assert!(
        read::<GetReviewDetail>(
            &cell,
            PullChildKey {
                pull: 1,
                number: current.number
            }
        )
        .await
        .unwrap()
        .dismissal
        .is_some()
    );
    assert!(
        read::<GetPull>(&cell, 1)
            .await
            .unwrap()
            .review_decisions
            .is_empty()
    );
    fixtures::close(&server).await;
}
