#![cfg(unix)]

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cells::{application::MutationIdentity, repository::*};
use cellar::{
    bucket::Bucket,
    sdk::{NodeOutcome, ShutdownMode},
};

struct Host {
    cells: crate::cells::runtime::RunningCells,
    transport: crate::peer::PeerTransport,
    stop: CancellationToken,
    work: tokio_util::task::TaskTracker,
    management: tokio::task::JoinHandle<std::io::Result<()>>,
    _public: tokio::net::TcpListener,
}

impl Host {
    async fn start(
        directory: &std::path::Path,
        name: &str,
        files: &crate::test_support::tls::IdentityFiles,
        bucket: Bucket,
        durability: crate::CellDurabilityMode,
    ) -> Self {
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let management = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let internal = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cells = files.config(
            url::Url::parse(&format!("https://{}", management.local_addr().unwrap())).unwrap(),
        );
        cells.peer_tls_server_name = Some("localhost".into());
        cells.data_dir = directory.join(name);
        cells.durability = durability;
        let tls = crate::peer::tls::LoadedPeerTls::load(&cells).unwrap();
        let config = crate::Config {
            listen: public.local_addr().unwrap(),
            management_listen: management.local_addr().unwrap(),
            storage: crate::StorageConfig {
                url: "s3://test/repositories".into(),
            },
            cells,
            import: Default::default(),
            auth: None,
        };
        let transport = crate::peer::PeerTransport::start(&tls).await.unwrap();
        let work = tokio_util::task::TaskTracker::new();
        let stop = CancellationToken::new();
        let peers =
            crate::peer::incoming(internal.local_addr().unwrap(), work.clone(), stop.clone())
                .unwrap();
        let stopped = stop.clone();
        let management = tokio::spawn(async move {
            axum::serve(tls.listener(management), peers)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        });
        let cells = crate::cells::runtime::start_with_bucket(
            &config,
            public.local_addr().unwrap(),
            internal,
            name.into(),
            transport.http.clone(),
            transport.stream_http.clone(),
            16,
            bucket,
        )
        .await
        .unwrap();
        Self {
            cells,
            transport,
            stop,
            work,
            management,
            _public: public,
        }
    }

    async fn stop(self, mode: ShutdownMode) {
        let NodeOutcome::Stopped(report) = self.cells.node.shutdown(mode).await.unwrap() else {
            panic!("node was fenced");
        };
        assert!(
            report.cells_drained && report.application_work_drained && !report.drain_timed_out,
            "{report:?}"
        );
        self.stop.cancel();
        self.management.await.unwrap().unwrap();
        self.work.close();
        self.work.wait().await;
        self.transport.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cellar_routes_repository_commands_over_mutual_tls_and_restores_after_owner_stop() {
    let local = tempfile::TempDir::new().unwrap();
    let bucket = Bucket::open_dev(&local.path().join("objects.sqlite")).unwrap();
    qualify_handoff(local.path(), bucket, crate::CellDurabilityMode::Object).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated RustFS test bucket and CRAB_HTTP_CELL_TEST_* settings"]
async fn rustfs_cellar_routes_over_mutual_tls_and_recovers_after_handoff() {
    let bucket = std::env::var("CRAB_HTTP_CELL_TEST_BUCKET").unwrap();
    let endpoint = std::env::var("CRAB_HTTP_CELL_TEST_ENDPOINT").unwrap();
    let prefix = std::env::var("CRAB_HTTP_CELL_TEST_PREFIX").unwrap();
    assert!(!prefix.trim_matches('/').is_empty());
    let specification = format!("s3://{bucket}/{prefix}/cellar-{}", Uuid::now_v7());
    let bucket = Bucket::open(
        &specification,
        Some(&endpoint),
        "us-east-1",
        None,
        Some("crab-server-test"),
    )
    .unwrap();
    let local = tempfile::TempDir::new().unwrap();
    qualify_handoff(local.path(), bucket, crate::CellDurabilityMode::Fleet).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cellar_default_fleet_durability_supports_peer_routing_and_handoff() {
    let local = tempfile::TempDir::new().unwrap();
    let bucket = Bucket::open_dev(&local.path().join("objects.sqlite")).unwrap();
    qualify_handoff(local.path(), bucket, crate::CellDurabilityMode::Fleet).await;
}

async fn qualify_handoff(
    local: &std::path::Path,
    bucket: Bucket,
    durability: crate::CellDurabilityMode,
) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let files = crate::test_support::tls::IdentityFiles::generate();
    let first = Host::start(local, "crab-peer-one", &files, bucket.clone(), durability).await;
    let second = Host::start(local, "crab-peer-two", &files, bucket.clone(), durability).await;
    let third = if durability == crate::CellDurabilityMode::Fleet {
        Some(Host::start(local, "crab-peer-three", &files, bucket, durability).await)
    } else {
        None
    };
    let repository = Uuid::now_v7();
    let now = crate::cells::unix_now_ms().unwrap();
    let identity = MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
    };
    let cell = first.cells.client.repository(repository).unwrap();
    cell.client.initialize(repository).await.unwrap();
    let input = CreateIssueInput {
        metadata: Default::default(),
        submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "https://identity.test".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        title: "Remote Cellar issue".into(),
        body: "Published through the mTLS fleet transport".into(),
    };
    use crate::cells::{
        Address,
        directory::{
            CompleteHandles, CreateProfile, CreateProfileInput, GetActor, IdentityKey,
            LookupActors, ProfileOutcome, ResolveHandles,
        },
    };
    first
        .cells
        .client
        .initialize(Address::Directory)
        .await
        .unwrap();
    let profile_input = CreateProfileInput {
        submission_id: Uuid::now_v7(),
        identity: IdentityKey {
            issuer: "https://identity.test".into(),
            subject: "alice".into(),
        },
        handle: "alice".into(),
        name: "Alice".into(),
    };
    let lookup_identity = profile_input.identity.clone();
    // A command ID is scoped to its authority. Reusing it for the repository
    // must neither conflict with nor replay the directory's decision.
    let actor = first
        .cells
        .client
        .command::<CreateProfile>(Address::Directory, identity, profile_input.clone())
        .await
        .unwrap();
    let ProfileOutcome::Saved(profile) = &actor.output else {
        panic!("profile was rejected")
    };
    use crate::cells::organization::{
        self, Bootstrap, BootstrapInput, Caller, ChangeOutcome, GetOrganization,
    };
    let organization = Address::Organization(Uuid::now_v7());
    first.cells.client.initialize(organization).await.unwrap();
    let organization_input = BootstrapInput {
        actor: profile.id,
        name: "Peer organization".into(),
        description: "Independent native authority".into(),
    };
    let bootstrapped = first
        .cells
        .client
        .command::<Bootstrap>(organization, identity, organization_input.clone())
        .await
        .unwrap();
    let ChangeOutcome::Applied(organization_record) = &bootstrapped.output else {
        panic!("organization rejected");
    };
    let created = cell
        .client
        .command::<CreateIssue>(&repository, identity, input.clone())
        .await
        .unwrap();
    let CreateIssueOutcome::Created(record) = created.output else {
        panic!("issue was rejected");
    };
    let observer = third.as_ref().unwrap_or(&second);
    let remote = observer.cells.client.repository(repository).unwrap();
    let observed = remote
        .client
        .query::<GetIssue>(&repository, Some(created.receipt), record.number)
        .await
        .unwrap();
    assert_eq!(observed.output.as_ref(), Some(record.as_ref()));
    let actor_read = observer
        .cells
        .client
        .query::<GetActor>(Address::Directory, Some(actor.receipt), profile.id)
        .await
        .unwrap();
    assert_eq!(actor_read.output.as_ref(), Some(profile));
    let organization_read = observer
        .cells
        .client
        .query::<GetOrganization>(
            organization,
            Some(bootstrapped.receipt),
            Caller { actor: profile.id },
        )
        .await
        .unwrap();
    assert_eq!(
        organization_read.output.as_ref().unwrap().organization,
        *organization_record
    );
    first.stop(ShutdownMode::Handoff).await;
    let organization_replay = remote
        .client
        .command::<Bootstrap>(organization, identity, organization_input)
        .await
        .unwrap();
    assert_eq!(organization_replay, bootstrapped);
    let organization_restored = remote
        .client
        .query::<GetOrganization>(
            organization,
            Some(organization_read.receipt),
            Caller { actor: profile.id },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(organization_restored.role, organization::Role::Owner);
    assert_eq!(organization_restored.organization, *organization_record);
    let actor_replay = remote
        .client
        .command::<CreateProfile>(Address::Directory, identity, profile_input)
        .await
        .unwrap();
    assert_eq!(actor_replay, actor);
    let restored_actor = remote
        .client
        .query::<GetActor>(Address::Directory, Some(actor.receipt), profile.id)
        .await
        .unwrap();
    assert_eq!(restored_actor.output.as_ref(), Some(profile));
    let lookup = remote
        .client
        .query::<LookupActors>(
            Address::Directory,
            Some(actor.receipt),
            vec![lookup_identity.clone()],
        )
        .await
        .unwrap();
    assert_eq!(lookup.output, vec![Some(profile.clone().into())]);
    let resolved = remote
        .client
        .query::<ResolveHandles>(
            Address::Directory,
            Some(actor.receipt),
            vec![profile.handle.clone()],
        )
        .await
        .unwrap();
    assert_eq!(resolved.output, vec![Some(lookup_identity.clone())]);
    let completed = remote
        .client
        .query::<CompleteHandles>(Address::Directory, Some(actor.receipt), "al".into())
        .await
        .unwrap();
    assert_eq!(completed.output.items, vec![profile.clone().into()]);
    assert!(!completed.output.truncated);
    use crate::cells::repository::{ListDiscussions, SearchInput, search::people};
    let authored = remote
        .client
        .query::<ListDiscussions>(
            repository,
            Some(created.receipt),
            SearchInput {
                query: Some("author:@me".into()),
                people: vec![people::Binding {
                    name: "@me".into(),
                    identity: Some(
                        people::identity(&lookup_identity.issuer, &lookup_identity.subject)
                            .unwrap(),
                    ),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(authored.output.matching, 1);
    assert!(authored.output.authored_by_me);

    let replayed = remote
        .client
        .command::<CreateIssue>(&repository, identity, input)
        .await
        .unwrap();
    assert_eq!(replayed.receipt, created.receipt);

    let restored = remote
        .client
        .query::<GetIssue>(&repository, Some(created.receipt), record.number)
        .await
        .unwrap();
    assert_eq!(restored.output.as_ref(), Some(record.as_ref()));
    second.stop(ShutdownMode::Preserve).await;
    if let Some(third) = third {
        third.stop(ShutdownMode::Preserve).await;
    }
}
