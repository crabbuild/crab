use super::*;
use crate::cells::repository::*;
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
    let now = crate::cells::unix_now_ms().unwrap();
    MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
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
        .command::<M>(&cell.target, identity(), input)
        .await
        .unwrap()
        .output
}
async fn create_issue(cell: &RepositoryCell) {
    let result = write::<CreateIssue>(
        cell,
        CreateIssueInput {
            metadata: Default::default(),
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("alice"),
            title: "Discuss".into(),
            body: String::new(),
        },
    )
    .await;
    assert!(matches!(result, CreateIssueOutcome::Created(_)));
}
fn selection(target: Target, kind: Kind, subject: &str, active: bool) -> Selection {
    Selection {
        target,
        kind,
        active,
        actor: actor(subject),
    }
}
fn applied(value: Outcome) -> Summary {
    let Outcome::Applied(value) = value else {
        panic!("reaction was not applied")
    };
    value
}
fn group(summary: &Summary, kind: Kind) -> &Group {
    summary.groups.get(&kind).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn desired_memberships_are_unique_across_retries_and_display_name_changes() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    create_issue(&cell).await;
    let target = Target::Issue { number: 1 };
    let input = selection(target, Kind::ThumbsUp, "alice", true);
    let request = identity();
    let first = cell
        .client
        .command::<SetReaction>(&cell.target, request, input.clone())
        .await
        .unwrap();
    let retry = cell
        .client
        .command::<SetReaction>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert_eq!(first, retry);
    let mut renamed = input;
    renamed.actor.name = "Alice renamed".into();
    let repeated = applied(write::<SetReaction>(&cell, renamed).await);
    assert_eq!(group(&repeated, Kind::ThumbsUp).count, 1);
    let bob =
        applied(write::<SetReaction>(&cell, selection(target, Kind::ThumbsUp, "bob", true)).await);
    assert_eq!(group(&bob, Kind::ThumbsUp).count, 2);
    for _ in 0..2 {
        let removed = applied(
            write::<SetReaction>(&cell, selection(target, Kind::ThumbsUp, "alice", false)).await,
        );
        assert_eq!(
            group(&removed, Kind::ThumbsUp),
            &Group {
                count: 1,
                reacted: false
            }
        );
    }
    let other = cell
        .client
        .query::<GetReactions>(
            &cell.target,
            None,
            Query {
                targets: vec![target],
                actor: actor("bob"),
            },
        )
        .await
        .unwrap()
        .output;
    assert!(group(other[0].as_ref().unwrap(), Kind::ThumbsUp).reacted);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_actors_cannot_lose_reaction_counts_or_underflow_on_removal() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    create_issue(&cell).await;
    let target = Target::Issue { number: 1 };
    for active in [true, false] {
        let operations = (0..20).map(|number| {
            let input = selection(target, Kind::Hooray, &format!("actor-{number}"), active);
            cell.client
                .command::<SetReaction>(&cell.target, identity(), input)
        });
        for result in futures_util::future::join_all(operations).await {
            assert!(matches!(result.unwrap().output, Outcome::Applied(_)));
        }
        let result = cell
            .client
            .query::<GetReactions>(
                &cell.target,
                None,
                Query {
                    targets: vec![target],
                    actor: actor("actor-0"),
                },
            )
            .await
            .unwrap()
            .output;
        let expected = if active { 20 } else { 0 };
        assert_eq!(
            group(result[0].as_ref().unwrap(), Kind::Hooray),
            &Group {
                count: expected,
                reacted: active
            }
        );
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_discussion_target_is_scoped_to_its_parent() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    create_issue(&cell).await;
    write::<CreateComment>(
        &cell,
        CreateCommentInput {
            issue: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("alice"),
            body: "Comment".into(),
        },
    )
    .await;
    write::<CreatePull>(
        &cell,
        CreatePullInput {
            metadata: Default::default(),
            draft: false,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("alice"),
            title: "Pull".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    write::<CreatePullComment>(
        &cell,
        CreatePullCommentInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("alice"),
            body: "Comment".into(),
        },
    )
    .await;
    write::<CreatePullReview>(
        &cell,
        CreatePullReviewInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("bob"),
            body: "Review".into(),
            state: ReviewState::Commented,
            commit_oid: "b".repeat(40),
        },
    )
    .await;
    write::<CreatePullReviewThread>(
        &cell,
        CreatePullReviewThreadInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("bob"),
            body: "Thread".into(),
            suggested_text: None,
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
            path: b"README.md".to_vec(),
            old_blob_oid: Some("c".repeat(40)),
            new_blob_oid: Some("d".repeat(40)),
            side: PullReviewThreadSide::New,
            start_line: 1,
            end_line: 1,
        },
    )
    .await;
    write::<CreatePullReviewReply>(
        &cell,
        CreatePullReviewReplyInput {
            pull: 1,
            thread: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("alice"),
            body: "Reply".into(),
        },
    )
    .await;
    let targets = vec![
        Target::Issue { number: 1 },
        Target::Pull { number: 1 },
        Target::IssueComment {
            issue: 1,
            number: 1,
        },
        Target::PullComment { pull: 1, number: 1 },
        Target::PullReview { pull: 1, number: 1 },
        Target::PullThread { pull: 1, number: 1 },
        Target::PullReply {
            pull: 1,
            thread: 1,
            number: 1,
        },
    ];
    for target in &targets {
        let result = applied(
            write::<SetReaction>(&cell, selection(*target, Kind::Rocket, "bob", true)).await,
        );
        assert_eq!(group(&result, Kind::Rocket).count, 1);
    }
    let results = cell
        .client
        .query::<GetReactions>(
            &cell.target,
            None,
            Query {
                targets,
                actor: actor("bob"),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(results.len(), 7);
    for result in results {
        assert!(group(result.as_ref().unwrap(), Kind::Rocket).reacted);
    }
    let absent = [
        Target::IssueComment {
            issue: 2,
            number: 1,
        },
        Target::PullComment { pull: 2, number: 1 },
        Target::PullReview { pull: 2, number: 1 },
        Target::PullThread { pull: 2, number: 1 },
        Target::PullReply {
            pull: 1,
            thread: 2,
            number: 1,
        },
    ];
    for target in absent {
        assert_eq!(
            write::<SetReaction>(&cell, selection(target, Kind::Rocket, "bob", true)).await,
            Outcome::NotFound
        );
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_reactions_are_readable_but_fresh_writes_are_rejected() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    create_issue(&cell).await;
    let target = Target::Issue { number: 1 };
    write::<SetReaction>(&cell, selection(target, Kind::Heart, "alice", true)).await;
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    assert_eq!(
        write::<SetReaction>(&cell, selection(target, Kind::Heart, "alice", false)).await,
        Outcome::Archived
    );
    let read = cell
        .client
        .query::<GetReactions>(
            &cell.target,
            None,
            Query {
                targets: vec![target],
                actor: actor("alice"),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(group(read[0].as_ref().unwrap(), Kind::Heart).count, 1);
    for count in [0, 51] {
        let result = cell
            .client
            .query::<GetReactions>(
                &cell.target,
                None,
                Query {
                    targets: vec![target; count],
                    actor: actor("alice"),
                },
            )
            .await;
        assert!(result.is_err());
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_restart_restores_actor_memberships_counts_and_command_receipts() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    started
        .client
        .command::<CreateIssue>(
            &repository,
            identity(),
            CreateIssueInput {
                metadata: Default::default(),
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("alice"),
                title: "Durable reactions".into(),
                body: String::new(),
            },
        )
        .await
        .unwrap();
    let target = Target::Issue { number: 1 };
    for active in [true, false] {
        started
            .client
            .command::<SetReaction>(
                &repository,
                identity(),
                selection(target, Kind::Rocket, "alice", active),
            )
            .await
            .unwrap();
    }
    let request = identity();
    let input = selection(target, Kind::Heart, "bob", true);
    let committed = started
        .client
        .command::<SetReaction>(&repository, request, input.clone())
        .await
        .unwrap();
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let replay = restored
        .client
        .command::<SetReaction>(&repository, request, input)
        .await
        .unwrap();
    assert_eq!(committed, replay);
    for subject in ["alice", "bob"] {
        let result = restored
            .client
            .query::<GetReactions>(
                &repository,
                Some(committed.receipt),
                Query {
                    targets: vec![target],
                    actor: actor(subject),
                },
            )
            .await
            .unwrap()
            .output;
        let summary = result[0].as_ref().unwrap();
        assert_eq!(
            group(summary, Kind::Heart),
            &Group {
                count: 1,
                reacted: subject == "bob"
            }
        );
        assert_eq!(
            group(summary, Kind::Rocket),
            &Group {
                count: 0,
                reacted: false
            }
        );
    }
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
