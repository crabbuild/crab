use super::*;
use crate::{app, cells::RepositoryCell, test_support::http::Harness};

fn repository(h: &Harness, name: &str) -> RepositoryCell {
    let repo = h
        .server
        .repositories
        .get(&("team".into(), name.into()))
        .unwrap();
    h.server.cells().unwrap().repository(repo.id).unwrap()
}

fn author(h: &Harness, subject: &str, name: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: h.provider.issuer.clone(),
        subject: subject.into(),
        name: name.into(),
    }
}

async fn create(cell: &RepositoryCell, actor: RepositoryAuthor) -> IssueRecord {
    let output = cell
        .client
        .command::<CreateIssue>(
            cell.target,
            app::mutation_identity().unwrap(),
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor,
                title: "Move the release checklist".into(),
                body: "Keep the issue body and discussion.".into(),
                metadata: Default::default(),
            },
        )
        .await
        .unwrap()
        .output;
    let CreateIssueOutcome::Created(issue) = output else {
        panic!("{output:?}");
    };
    *issue
}

async fn comment(
    cell: &RepositoryCell,
    issue: u64,
    actor: RepositoryAuthor,
    body: String,
) -> CommentRecord {
    let output = cell
        .client
        .command::<CreateComment>(
            cell.target,
            app::mutation_identity().unwrap(),
            CreateCommentInput {
                can_post_locked: false,
                can_mark_duplicate: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                issue,
                author: actor,
                body,
            },
        )
        .await
        .unwrap()
        .output;
    let CreateCommentOutcome::Created(comment) = output else {
        panic!("{output:?}");
    };
    comment
}

async fn capture(cell: &RepositoryCell, input: CaptureInput) -> CaptureOutcome {
    cell.client
        .query::<Capture>(cell.target, None, input)
        .await
        .unwrap()
        .output
}

fn ready(outcome: CaptureOutcome) -> Frame {
    let CaptureOutcome::Ready { frame } = outcome else {
        panic!("{outcome:?}");
    };
    *frame
}

async fn stage(
    cell: &RepositoryCell,
    request: Uuid,
    actor: &IdentityKey,
    frame: Frame,
) -> StageOutcome {
    cell.client
        .command::<Stage>(
            cell.target,
            app::mutation_identity().unwrap(),
            StageInput {
                request,
                destination: cell.target,
                actor: actor.clone(),
                frame,
            },
        )
        .await
        .unwrap()
        .output
}

fn saved(outcome: StageOutcome) -> Progress {
    let StageOutcome::Saved { progress } = outcome else {
        panic!("{outcome:?}");
    };
    progress
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staging_preserves_order_and_replays_without_creating_an_issue() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let origin = repository(&h, "private");
    let destination = repository(&h, "library");
    let alice = author(&h, "alice-id", "Alice");
    let bob = author(&h, "bob-id", "Bob");
    let issue = create(&origin, alice.clone()).await;
    let mut original = Vec::new();
    for index in 1..=18 {
        original.push(
            comment(
                &origin,
                issue.number,
                bob.clone(),
                format!("Review note {index}"),
            )
            .await,
        );
    }
    original.reverse();

    let actor = IdentityKey {
        issuer: alice.issuer.clone(),
        subject: alice.subject.clone(),
    };
    let request = Uuid::now_v7();
    let head = || CaptureInput::Start {
        number: issue.number,
        expected_version: issue.version,
    };
    let first = ready(capture(&origin, head()).await);
    let source = first.source;
    let Records::Issue { header } = &first.records else {
        panic!("{first:?}")
    };
    assert_eq!(header.issue, issue);
    let comments = Cursor::Comments { before: None };
    let early = ready(
        capture(
            &origin,
            CaptureInput::Continue {
                source,
                cursor: comments,
            },
        )
        .await,
    );
    assert_eq!(
        stage(&destination, request, &actor, early).await,
        StageOutcome::OutOfOrder
    );
    let mut progress = saved(stage(&destination, request, &actor, first).await);
    assert_eq!((progress.frames, progress.next), (1, Some(comments)));

    let mut next = Some(comments);
    let mut staged_comments = Vec::new();
    let mut last_activity = u64::MAX;
    let mut activity_count = 0;
    let mut frames = 1;
    while let Some(cursor) = next {
        let frame = ready(capture(&origin, CaptureInput::Continue { source, cursor }).await);
        match &frame.records {
            Records::Comments { items, .. } => staged_comments.extend_from_slice(items),
            Records::Activity { items, .. } => {
                for event in items {
                    assert!(event.number < last_activity);
                    last_activity = event.number;
                    activity_count += 1;
                }
            }
            Records::Issue { .. } => panic!("expected an issue transfer page"),
        }
        next = frame.records.next();
        frames += 1;
        let previous = progress.commitment;
        progress = saved(stage(&destination, request, &actor, frame).await);
        assert_eq!((progress.frames, progress.next), (frames, next));
        assert_ne!(progress.commitment, previous);
    }
    assert_eq!(staged_comments, original);
    assert_eq!(activity_count, 19);
    assert_eq!(frames, 6);

    // A fresh transport identity exercises the permanent staging receipt.
    let replay = ready(capture(&origin, head()).await);
    assert_eq!(
        stage(&destination, request, &actor, replay).await,
        StageOutcome::Saved { progress }
    );
    let mut conflict = ready(capture(&origin, head()).await);
    let Records::Issue { header } = &mut conflict.records else {
        panic!("{conflict:?}")
    };
    header.issue.title.push('!');
    assert_eq!(
        stage(&destination, request, &actor, conflict).await,
        StageOutcome::Conflict
    );
    let destination_issue = create(&destination, alice).await;
    assert_eq!(destination_issue.number, 1);
    assert!(matches!(
        capture(
            &destination,
            CaptureInput::Continue {
                source,
                cursor: comments
            }
        )
        .await,
        CaptureOutcome::Changed
    ));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_invalidates_the_snapshot_when_a_comment_arrives() {
    let h = Harness::new(false).await;
    let origin = repository(&h, "private");
    let alice = author(&h, "alice-id", "Alice");
    let issue = create(&origin, alice.clone()).await;
    let frame = ready(
        capture(
            &origin,
            CaptureInput::Start {
                number: issue.number,
                expected_version: issue.version,
            },
        )
        .await,
    );
    comment(&origin, issue.number, alice, "A late review note.".into()).await;
    assert!(matches!(
        capture(
            &origin,
            CaptureInput::Continue {
                source: frame.source,
                cursor: Cursor::Comments { before: None },
            }
        )
        .await,
        CaptureOutcome::Changed
    ));
    let current = origin
        .client
        .query::<GetIssue>(origin.target, None, issue.number)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(current.version, issue.version);
    h.close().await;
}
