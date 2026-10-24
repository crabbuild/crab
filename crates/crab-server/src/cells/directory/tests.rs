use super::*;
use crate::cells::{Address, repository::GetRepositoryLifecycle, runtime};
use cellar::sdk::{NodeOutcome, ShutdownMode};

async fn stop(node: cellar::sdk::Node) {
    let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap() else {
        panic!("Cellar did not stop");
    };
    assert!(
        report.cells_drained
            && report.ingress_drained
            && report.application_work_drained
            && !report.drain_timed_out
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_directory_initialization_and_receipts_are_isolated_from_repository_authority() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    assert!(matches!(
        client
            .query::<GetDirectoryRevision>(Address::Directory, None, ())
            .await,
        Err(Error::NotInitialized)
    ));
    let repository = Uuid::now_v7();
    client.initialize(repository).await.unwrap();
    let repository_read = client
        .query::<GetRepositoryLifecycle>(&repository, None, ())
        .await
        .unwrap();
    client.initialize(Address::Directory).await.unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let revision = client
        .query::<GetDirectoryRevision>(Address::Directory, None, ())
        .await
        .unwrap();
    assert_eq!(revision.output, 0);
    assert!(matches!(
        client
            .query::<GetDirectoryRevision>(Address::Directory, Some(repository_read.receipt), ())
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        client
            .query::<GetDirectoryRevision>(&repository, None, ())
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        client
            .query::<GetRepositoryLifecycle>(Address::Directory, None, ())
            .await,
        Err(Error::Invalid(_))
    ));
    let after = client
        .query::<GetRepositoryLifecycle>(&repository, Some(repository_read.receipt), ())
        .await
        .unwrap();
    assert_eq!(after.receipt, repository_read.receipt);
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn directory_profiles_and_command_receipts_survive_native_restart() {
    let local = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:8788".parse().unwrap();
    let started = runtime::test_node(local.path(), address).await;
    started.client.initialize(Address::Directory).await.unwrap();
    let input = CreateProfileInput {
        submission_id: Uuid::now_v7(),
        identity: IdentityKey {
            issuer: "https://identity.test".into(),
            subject: "alice".into(),
        },
        handle: "alice".into(),
        name: "Alice".into(),
    };
    let request = crate::app::mutation_identity().unwrap();
    let created = started
        .client
        .command::<CreateProfile>(Address::Directory, request, input.clone())
        .await
        .unwrap();
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    let replay = restored
        .client
        .command::<CreateProfile>(Address::Directory, request, input.clone())
        .await
        .unwrap();
    assert_eq!(replay, created);
    let ProfileOutcome::Saved(profile) = created.output else {
        panic!("profile was not saved")
    };
    let own = restored
        .client
        .query::<GetOwnProfile>(Address::Directory, Some(created.receipt), input.identity)
        .await
        .unwrap();
    assert_eq!(own.output, Some(profile));
    stop(restored.node).await;
}
