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
                copied: Default::default(),
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
            Records::Comments { items, .. } => {
                staged_comments.extend(items.iter().map(|item| item.record.clone()));
            }
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

async fn mutate<C: Mutation>(cell: &RepositoryCell, input: C::Input) -> C::Output {
    cell.client
        .command::<C>(cell.target, app::mutation_identity().unwrap(), input)
        .await
        .unwrap()
        .output
}

async fn label(cell: &RepositoryCell, author: &RepositoryAuthor, name: &str) -> u64 {
    let CreateLabelOutcome::Created(label) = mutate::<CreateLabel>(
        cell,
        CreateLabelInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: author.clone(),
            name: name.into(),
            color: "f0f0f0".into(),
            description: None,
        },
    )
    .await
    else {
        panic!("label creation failed")
    };
    label.number
}

async fn milestone(
    cell: &RepositoryCell,
    author: &RepositoryAuthor,
    title: &str,
    due_on: &str,
) -> u64 {
    use collaboration::{Decision, milestones::*};
    let Decision::Applied(milestone) = mutate::<CreateMilestone>(
        cell,
        CreateMilestoneInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: author.clone(),
            title: title.into(),
            description: String::new(),
            due_on: Some(due_on.into()),
        },
    )
    .await
    else {
        panic!("milestone creation failed")
    };
    milestone.number
}

async fn snapshot(
    origin: &RepositoryCell,
    destination: &RepositoryCell,
    issue: &IssueRecord,
    request: Uuid,
    actor: &IdentityKey,
) -> (Source, Progress) {
    let mut frame = ready(
        capture(
            origin,
            CaptureInput::Start {
                number: issue.number,
                expected_version: issue.version,
            },
        )
        .await,
    );
    let source = frame.source;
    loop {
        let progress = saved(stage(destination, request, actor, frame).await);
        let Some(cursor) = progress.next else {
            return (source, progress);
        };
        frame = ready(capture(origin, CaptureInput::Continue { source, cursor }).await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publication_replays_a_retired_issue_with_its_comments_and_moderation() {
    use collaboration::{Decision, milestones::*};
    use publication::*;

    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let origin = repository(&h, "private");
    let destination = repository(&h, "library");
    let alice = author(&h, "alice-id", "Alice");
    let bob = author(&h, "bob-id", "Bob");
    let actor = IdentityKey {
        issuer: alice.issuer.clone(),
        subject: alice.subject.clone(),
    };
    let submission = CreateIssueInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        author: alice.clone(),
        title: "Move the release checklist".into(),
        body: "Keep the issue body and discussion.".into(),
        metadata: Default::default(),
    };
    let CreateIssueOutcome::Created(issue) =
        mutate::<CreateIssue>(&origin, submission.clone()).await
    else {
        panic!("source did not create the issue")
    };
    let beta = label(&origin, &alice, "beta").await;
    let alpha = label(&origin, &alice, "alpha").await;
    let UpdateIssueOutcome::Updated(issue) = mutate::<UpdateIssue>(
        &origin,
        UpdateIssueInput {
            number: issue.number,
            actor: alice.clone(),
            can_triage: true,
            can_moderate: false,
            version: issue.version,
            title: None,
            body: None,
            state: None,
            state_reason: None,
            label_ids: Some(vec![beta, alpha]),
            assignees: None,
        },
    )
    .await
    else {
        panic!("labels could not be applied")
    };
    let issue = *issue;
    let subject = Subject {
        kind: SubjectKind::Issue,
        number: issue.number,
    };
    let release = milestone(&origin, &alice, "Release", "2030-03-04").await;
    assert!(matches!(
        mutate::<AssignMilestone>(
            &origin,
            AssignMilestoneInput {
                subject,
                milestone: Some(release),
                version: 0,
                actor: alice.clone(),
            }
        )
        .await,
        Decision::Applied(_)
    ));
    label(&destination, &alice, "unrelated").await;
    let target_beta = label(&destination, &alice, "Beta").await;
    let target_alpha = label(&destination, &alice, "alpha").await;
    milestone(&destination, &alice, "Planning", "2030-03-04").await;
    let target_release = milestone(&destination, &alice, "Release", "2030-03-04").await;
    let prior = create(&destination, alice.clone()).await;
    assert!(matches!(
        mutate::<pins::SetPins>(
            &origin,
            pins::Input {
                submission_id: *Uuid::now_v7().as_bytes(),
                actor: alice.clone(),
                can_write: true,
                version: 0,
                numbers: vec![issue.number],
            }
        )
        .await,
        pins::Outcome::Applied(1)
    ));
    let hidden = comment(
        &origin,
        issue.number,
        bob.clone(),
        "Original hidden note.".into(),
    )
    .await;
    for index in 2..=131 {
        comment(
            &origin,
            issue.number,
            bob.clone(),
            format!("Release finding {index}"),
        )
        .await;
    }
    let outcome = mutate::<minimization::Set>(
        &origin,
        minimization::Input {
            target: Target::IssueComment {
                issue: issue.number,
                number: hidden.number,
            },
            actor: alice,
            version: 0,
            reason: Some(minimization::Reason::Outdated),
        },
    )
    .await;
    assert!(matches!(outcome, minimization::Outcome::Saved(_)));

    let request = Uuid::now_v7();
    let (source, progress) = snapshot(&origin, &destination, &issue, request, &actor).await;
    let reserve = || PrepareInput {
        request,
        source,
        actor: actor.clone(),
        commitment: progress.commitment,
        ownership_epoch: 1,
    };
    let PrepareOutcome::Reserved { reservation } = mutate::<Prepare>(&destination, reserve()).await
    else {
        panic!("destination did not reserve the issue")
    };
    let target = reservation.destination;
    assert_eq!(target.number, prior.number + 1);
    assert_eq!(
        mutate::<Prepare>(&destination, reserve()).await,
        PrepareOutcome::Reserved {
            reservation: reservation.clone()
        }
    );
    assert!(
        destination
            .client
            .query::<GetIssue>(destination.target, None, target.number)
            .await
            .unwrap()
            .output
            .is_none()
    );

    let mut stale = reservation.clone();
    stale.source.version += 1;
    assert_eq!(
        mutate::<Retire>(&origin, stale).await,
        RetireOutcome::Changed
    );
    let RetireOutcome::Committed { decision } =
        mutate::<Retire>(&origin, reservation.clone()).await
    else {
        panic!("source did not commit the issue move")
    };
    assert!(
        origin
            .client
            .query::<GetIssue>(origin.target, None, issue.number)
            .await
            .unwrap()
            .output
            .is_none()
    );
    assert_eq!(
        origin
            .client
            .query::<GetMove>(origin.target, None, issue.number)
            .await
            .unwrap()
            .output,
        Some(decision.clone())
    );
    assert_eq!(
        mutate::<Retire>(&origin, reservation).await,
        RetireOutcome::Committed {
            decision: decision.clone()
        }
    );
    assert_eq!(
        mutate::<CreateIssue>(&origin, submission).await,
        CreateIssueOutcome::Transferred(issue.number)
    );
    assert!(
        origin
            .client
            .query::<pins::GetPins>(origin.target, None, ())
            .await
            .unwrap()
            .output
            .items
            .is_empty()
    );
    let source_link = format!("/api/repos/team/private/issues/{}", issue.number);
    let alice_session = h.login().await;
    assert_eq!(
        h.json(&source_link, &alice_session).await,
        serde_json::json!({"transfer":{"state":"pending"}})
    );
    for _ in 0..2 {
        assert_eq!(
            mutate::<Publish>(&destination, decision.clone()).await,
            PublishOutcome::Published {
                destination: target
            }
        );
    }
    assert_eq!(
        h.json(&source_link, &alice_session).await,
        serde_json::json!({"transfer": {
            "state":"ready", "number":target.number, "repository":{"id": target.repository, "owner":"team", "name":"library"}
        }})
    );
    *h.provider.mode.lock().await = "member".into();
    let bob_session = h.login().await;
    let catalog = h.server.catalog().unwrap();
    let mut members = catalog.membership("team", "library").await.unwrap().members;
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "library", members, true)
        .await
        .unwrap();
    assert_eq!(
        h.json(&source_link, &bob_session).await,
        serde_json::json!({"transfer":{"state":"unavailable"}})
    );
    let moved_issue = destination
        .client
        .query::<GetIssue>(destination.target, None, target.number)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        moved_issue,
        IssueRecord {
            number: target.number,
            label_ids: vec![target_beta, target_alpha],
            ..issue
        }
    );
    let assignment = destination
        .client
        .query::<GetAssignment>(
            destination.target,
            None,
            Subject {
                kind: SubjectKind::Issue,
                number: target.number,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(assignment.milestone.unwrap().number, target_release);

    let restored = destination
        .client
        .query::<GetComment>(
            &destination.target,
            None,
            CommentKey {
                issue: target.number,
                number: hidden.number,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        restored,
        CommentRecord {
            issue: target.number,
            ..hidden.clone()
        }
    );
    let frame = ready(
        capture(
            &destination,
            CaptureInput::Start {
                number: target.number,
                expected_version: moved_issue.version,
            },
        )
        .await,
    );
    let page = ready(
        capture(
            &destination,
            CaptureInput::Continue {
                source: frame.source,
                cursor: Cursor::Comments { before: Some(2) },
            },
        )
        .await,
    );
    let Records::Comments { items, .. } = page.records else {
        panic!("expected a comment page")
    };
    assert_eq!(
        items[0].visibility.unwrap().reason,
        Some(minimization::Reason::Outdated)
    );
    let following = comment(
        &destination,
        target.number,
        bob,
        "Continue the conversation.".into(),
    )
    .await;
    assert_eq!(following.number, 132);
    h.close().await;
}
