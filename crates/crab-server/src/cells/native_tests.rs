use super::{application::MutationIdentity, repository::*, *};
use cellar::sdk::{NodeOutcome, ShutdownMode};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

fn lose_responses(remaining: Arc<AtomicUsize>) -> Arc<dyn cellar::sdk::Application> {
    let application = application::compiled().unwrap();
    let cells = application.clone();
    let mut registry = cellar::sdk::Registry::new(move |context, invocation| {
        let application = application.clone();
        let remaining = remaining.clone();
        Box::pin(async move {
            // Lose the reply after the actual Cell and its output durability gate
            // succeed. A retry must recover the receipt, not apply another write.
            let output = application.invoke(context.clone(), invocation).await?;
            if remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(cellar::sdk::Output::Http(
                    cellar::sdk::http::Response::new(503, "response lost", vec![])
                        .into_http(&context)?,
                ));
            }
            Ok(output)
        })
    });
    for class in [repository::CLASS, directory::CLASS, organization::CLASS] {
        let cells = cells.clone();
        registry = registry
            .cell(class, move |context, identity| {
                cells.create_cell(context, identity)
            })
            .unwrap();
    }
    Arc::new(registry)
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_mutation_response_replays_the_durable_receipt_once() {
    for lost in [1, 3] {
        let local = tempfile::TempDir::new().unwrap();
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remaining = Arc::new(AtomicUsize::new(0));
        let started = runtime::test_node_with_application(
            local.path(),
            public.local_addr().unwrap(),
            lose_responses(remaining.clone()),
        )
        .await;
        let repository = Uuid::now_v7();
        started.client.initialize(repository).await.unwrap();
        let request = identity();
        remaining.store(lost, Ordering::SeqCst);
        let result = started
            .client
            .command::<CreateIssue>(&repository, request, issue())
            .await;
        if lost == 1 {
            assert!(result.is_ok(), "lost reply was not recovered: {result:?}");
        } else {
            assert!(matches!(&result, Err(Error::Pending { .. })), "{result:?}");
            assert_eq!(
                remaining.load(Ordering::SeqCst),
                1,
                "retry must stop after two attempts"
            );
        }
        remaining.store(0, Ordering::SeqCst);
        let replayed = started
            .client
            .command::<CreateIssue>(&repository, request, issue())
            .await
            .unwrap();
        if let Ok(created) = result {
            assert_eq!(created, replayed);
        }
        assert_eq!(replayed.receipt.revision, 1);
        assert!(
            matches!(replayed.output, CreateIssueOutcome::Created(ref record) if record.number == 1)
        );
        assert!(
            started
                .client
                .query::<GetIssue>(&repository, Some(replayed.receipt), 2)
                .await
                .unwrap()
                .output
                .is_none()
        );
        stop(started.node).await;
    }
}

fn identity() -> MutationIdentity {
    let now = unix_now_ms().unwrap();
    MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
    }
}

fn issue() -> CreateIssueInput {
    CreateIssueInput {
        metadata: Default::default(),
        submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "https://identity.test".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        title: "Durable Cellar issue".into(),
        body: "Survives a complete node restart".into(),
    }
}

async fn stop(node: cellar::sdk::Node) {
    let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap() else {
        panic!("Cellar did not stop");
    };
    assert!(
        report.cells_drained
            && report.ingress_drained
            && report.application_work_drained
            && !report.drain_timed_out,
        "{report:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repository_mutation_replay_survives_cellar_restart() {
    let local = tempfile::TempDir::new().unwrap();
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let repository = Uuid::now_v7();
    let started = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    let cell = started.client.repository(repository).unwrap();
    cell.client.initialize(repository).await.unwrap();
    let request = identity();
    let created = cell
        .client
        .command::<CreateIssue>(&repository, request, issue())
        .await
        .unwrap();
    let CreateIssueOutcome::Created(record) = &created.output else {
        panic!("issue was not created");
    };
    assert_eq!(record.number, 1);
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    let cell = restored.client.repository(repository).unwrap();
    let replayed = cell
        .client
        .command::<CreateIssue>(&repository, request, issue())
        .await
        .unwrap();
    assert_eq!(replayed.receipt, created.receipt);
    assert_eq!(replayed.output, created.output);
    let read = cell
        .client
        .query::<GetIssue>(&repository, Some(created.receipt), 1)
        .await
        .unwrap();
    assert_eq!(read.output.as_ref(), Some(record.as_ref()));
    stop(restored.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn request_identity_cannot_replay_another_payload() {
    let local = tempfile::TempDir::new().unwrap();
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let started = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    let repository = Uuid::now_v7();
    let cell = started.client.repository(repository).unwrap();
    cell.client.initialize(repository).await.unwrap();
    let request = identity();
    cell.client
        .command::<CreateIssue>(&repository, request, issue())
        .await
        .unwrap();
    let mut changed = issue();
    changed.title = "Different content".into();
    assert!(matches!(
        cell.client
            .command::<CreateIssue>(&repository, request, changed)
            .await,
        Err(Error::RequestConflict)
    ));
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_blocks_fresh_writes_but_preserves_receipt_replay() {
    let local = tempfile::TempDir::new().unwrap();
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let started = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    let repository = Uuid::now_v7();
    let cell = started.client.repository(repository).unwrap();
    cell.client.initialize(repository).await.unwrap();
    let request = identity();
    let created = cell
        .client
        .command::<CreateIssue>(&repository, request, issue())
        .await
        .unwrap();
    cell.client
        .command::<ReplaceRepositoryLifecycle>(
            &repository,
            identity(),
            ReplaceRepositoryLifecycleInput {
                expected_version: 0,
                archived: true,
            },
        )
        .await
        .unwrap();
    let replayed = cell
        .client
        .command::<CreateIssue>(&repository, request, issue())
        .await
        .unwrap();
    assert_eq!(replayed.output, created.output);
    let fresh = cell
        .client
        .command::<CreateIssue>(&repository, identity(), issue())
        .await
        .unwrap();
    assert_eq!(fresh.output, CreateIssueOutcome::Archived);
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_archive_and_duplicate_mutations_keep_their_decisions_after_restart() {
    let local = tempfile::TempDir::new().unwrap();
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let repository = Uuid::now_v7();
    let principal = crate::auth::Identity {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    };
    let started = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    started
        .client
        .repository(repository)
        .unwrap()
        .client
        .initialize(repository)
        .await
        .unwrap();
    let recorded = crate::test_support::archive::run(&started.client, repository, &principal).await;
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), public.local_addr().unwrap()).await;
    crate::test_support::archive::verify_recovered(&restored.client, repository, &recorded).await;
    stop(restored.node).await;
}
