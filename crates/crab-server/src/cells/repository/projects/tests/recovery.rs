use super::*;
use crate::cells::planning::{Actor, Target};

async fn history(
    cell: &RepositoryCell,
    project: Uuid,
    after: Option<u64>,
) -> crate::cells::planning::History {
    cell.client
        .query::<GetHistory>(
            &cell.target,
            None,
            HistoryQuery {
                project,
                before: None,
                after,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn same_item_conflicts_preserve_the_winner_and_deletion_fences_stale_creates() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let project = Uuid::now_v7();
    command(&cell, created(project)).await;
    let draft = Item {
        subject: None,
        title: "Original".into(),
        body: String::new(),
        ..entry(1)
    };
    command(
        &cell,
        Command::Item {
            project,
            version: 0,
            item: draft.clone(),
        },
    )
    .await;
    let requests = ["Alice", "Bob"].map(|name| Request {
        request_id: Uuid::now_v7(),
        actor: RepositoryAuthor {
            subject: name.into(),
            name: name.into(),
            ..actor()
        },
        can_write: true,
        cancel: false,
        command: Command::Item {
            project,
            version: 1,
            item: Item {
                title: format!("{name}'s edit"),
                ..draft.clone()
            },
        },
    });
    let outputs = futures_util::future::join_all(requests.iter().map(|request| {
        cell.client
            .command::<Change>(&cell.target, identity(), request.clone())
    }))
    .await
    .into_iter()
    .map(|result| result.unwrap().output)
    .collect::<Vec<_>>();
    assert_eq!(
        outputs
            .iter()
            .filter(|output| matches!(output, Outcome::Applied { .. }))
            .count(),
        1
    );
    let lost = outputs
        .iter()
        .position(|output| matches!(output, Outcome::ItemConflict { version: 2, .. }))
        .unwrap();
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), requests[lost].clone())
            .await
            .unwrap()
            .output,
        outputs[lost]
    );
    let winner = page(&cell, project).await.items.remove(0);
    assert_eq!(winner.version, 2);
    assert_ne!(
        winner.item.title,
        if lost == 0 {
            "Alice's edit"
        } else {
            "Bob's edit"
        }
    );
    let events = history(&cell, project, Some(0)).await;
    assert_eq!(events.items.len(), 3);
    let event = &events.items[2];
    assert!(matches!(event.target, Target::Item { id, .. } if id == draft.id));
    assert!(
        matches!(&event.actor, Actor::Repository { author } if author.name == if lost == 0 { "Bob" } else { "Alice" })
    );
    assert_eq!(
        event
            .changes
            .iter()
            .find(|change| change.field == "/title")
            .unwrap()
            .before,
        Some(serde_json::json!("Original"))
    );
    assert_eq!(
        event
            .changes
            .iter()
            .find(|change| change.field == "/title")
            .unwrap()
            .after,
        Some(serde_json::json!(winner.item.title))
    );
    command(
        &cell,
        Command::Remove {
            project,
            version: 2,
            item: draft.id,
        },
    )
    .await;
    for version in [0, 1, 2, 3] {
        assert!(matches!(
            command(
                &cell,
                Command::Item {
                    project,
                    version,
                    item: draft.clone()
                }
            )
            .await,
            Outcome::ItemConflict { version: 3, .. }
        ));
    }
    assert!(page(&cell, project).await.items.is_empty());
    assert_eq!(history(&cell, project, Some(0)).await.items.len(), 4);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn history_cursors_page_forward_and_backward_through_committed_revisions() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let project = Uuid::now_v7();
    command(&cell, created(project)).await;
    for i in 0..12 {
        command(
            &cell,
            Command::Item {
                project,
                version: 0,
                item: Item {
                    subject: None,
                    title: format!("Work {i}"),
                    body: String::new(),
                    ..entry(1)
                },
            },
        )
        .await;
    }
    let first = history(&cell, project, Some(0)).await;
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.version)
            .collect::<Vec<_>>(),
        (1..=10).collect::<Vec<_>>()
    );
    assert_eq!(first.next, Some(10));
    let next = history(&cell, project, first.next).await;
    assert_eq!(
        next.items
            .iter()
            .map(|item| item.version)
            .collect::<Vec<_>>(),
        vec![11, 12, 13]
    );
    let newest = history(&cell, project, None).await;
    assert_eq!(newest.items.first().unwrap().version, 13);
    assert_eq!(newest.next, Some(4));
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn item_revision_preparation_resumes_after_native_restart() {
    use crate::cells::repository::collaboration::lifecycle::{
        LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
    };
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    use integration::{Historical, HistoricalState};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    let project = Uuid::now_v7();
    command(&cell, created(project)).await;
    for i in 0..65 {
        command(
            &cell,
            Command::Item {
                project,
                version: 0,
                item: Item {
                    subject: None,
                    title: format!("Existing {i}"),
                    body: String::new(),
                    ..entry(1)
                },
            },
        )
        .await;
    }
    cell.client
        .command::<Historical>(&cell.target, identity(), HistoricalState::NoItemRevisions)
        .await
        .unwrap();
    let migrating = cell
        .client
        .command::<preparation::Prepare>(
            &cell.target,
            identity(),
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        migrating,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
            version: 1,
            copied: 64
        })
    );
    assert!(
        cell.client
            .query::<GetProject>(&cell.target, None, project)
            .await
            .is_err()
    );
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("native node did not stop");
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(
        cell.client
            .query::<preparation::GetState>(&cell.target, None, ())
            .await
            .unwrap()
            .output,
        LifecycleStatus::Migrating {
            version: 1,
            copied: 64
        }
    );
    cell.client
        .command::<preparation::Prepare>(
            &cell.target,
            identity(),
            PrepareLifecycleInput { version: 1 },
        )
        .await
        .unwrap();
    let saved = page(&cell, project).await;
    assert_eq!((saved.total, saved.project.version), (65, 66));
    assert!(saved.items.iter().all(|row| row.version == 1));
    let NodeOutcome::Stopped(report) = restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap()
    else {
        panic!("restored node did not stop");
    };
    assert!(report.cells_drained && !report.drain_timed_out);
}
