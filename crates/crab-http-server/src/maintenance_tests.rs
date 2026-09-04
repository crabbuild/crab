use super::*;
use axum::body::Body;
use crab_coordination::{
    CoordinationError, GIT_GENERATION_OWNER_RESOURCE, GIT_MANIFEST_RESOURCE, GcFenceLease,
    PushLock, internal_lock_path,
};
use crab_metadata::{manifest_store, manifests::Manifest, ref_journal::RefJournalEdit};
use crab_write::WriteError;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TTL: Duration = Duration::from_secs(60);

async fn fixture() -> Arc<Server> {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "maintenance".into());
    manifest_store::create_manifest(
        &store,
        &layout,
        &Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    Arc::new(Server {
        repositories: BTreeMap::from([(
            ("team".into(), "repo".into()),
            Repository {
                config: RepositoryConfig {
                    owner: "team".into(),
                    name: "repo".into(),
                    bucket: "memory".into(),
                    prefix: "maintenance".into(),
                    description: String::new(),
                    members: vec![],
                },
                identity: RepositoryIdentity::new("memory", "maintenance", 1).unwrap(),
                store,
                layout,
                pinned: Mutex::new(None),
                maintenance: Mutex::new(None),
            },
        )]),
        runtime: Arc::new(RemoteGitRuntime::default()),
        options: RepositoryOptions::default(),
        cursor_key: [0; 32],
        admission: Semaphore::new(16),
        git_admission: Arc::new(Semaphore::new(4)),
        app_admission: Semaphore::new(8),
        maintenance_admission: Arc::new(Semaphore::new(2)),
        cancellation: CancellationToken::new(),
        port: 8788,
        auth: None,
    })
}

fn repository(server: &Server) -> &Repository {
    &server.repositories[&("team".into(), "repo".into())]
}

pub(super) async fn commit_without_proof(repo: &Repository) -> PushLock {
    let lease = PushLock::acquire_ref(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        "refs/heads/main",
        TTL,
    )
    .await
    .unwrap();
    let snapshot = manifest_store::read_repository_snapshot(&repo.store, &repo.layout)
        .await
        .unwrap();
    crab_write::journal::commit_edits(
        &repo.store,
        &repo.layout,
        &snapshot,
        vec![RefJournalEdit {
            ref_name: "refs/heads/main".into(),
            old_oid: None,
            new_oid: Some("a".repeat(40)),
            peeled_oid: None,
            lock_holder: Some(lease.holder().to_owned()),
            visibility_evidence_hash: None,
        }],
        None,
        vec![],
        vec![],
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    lease
}

async fn assert_released(repo: &Repository) {
    let owner = PushLock::acquire_internal(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        GIT_GENERATION_OWNER_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    owner.release().await.unwrap();
    for domain in [repo.layout.global_prefix(), repo.layout.repo_prefix()] {
        let sweep = GcFenceLease::acquire_sweep(repo.store.inner(), domain, TTL)
            .await
            .unwrap();
        sweep.release().await.unwrap();
    }
}

async fn close(server: &Server) {
    server.cancellation.cancel();
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn expired_browser_cache_observes_journal_and_reports_missing_proof_without_rollback() {
    let server = fixture().await;
    let repo = repository(&server);
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    repo.pinned.lock().await.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(3);
    let before = manifest_store::read_manifest(&repo.store, &repo.layout)
        .await
        .unwrap()
        .1;
    let lease = commit_without_proof(repo).await;
    assert_eq!(
        before,
        manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap()
            .1
    );

    let response = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/api/repos/team/repo/refs")
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "indexing_failed");
    let after = manifest_store::read_repository_snapshot(&repo.store, &repo.layout)
        .await
        .unwrap();
    assert_eq!(after.manifest.refs["refs/heads/main"], "a".repeat(40));
    assert!(after.journal.transactions.is_empty());
    assert_released(repo).await;
    lease.release().await.unwrap();
    close(&server).await;
}

#[tokio::test]
async fn another_generation_owner_keeps_publication_authority() {
    let server = fixture().await;
    let repo = repository(&server);
    let lease = commit_without_proof(repo).await;
    let mut owner = PushLock::acquire_internal(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        GIT_GENERATION_OWNER_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    let before = manifest_store::read_manifest(&repo.store, &repo.layout)
        .await
        .unwrap();
    let result = repo
        .open_current(&server, server.options, &CancellationToken::new())
        .await;
    assert!(matches!(
        result,
        Err(crate::Error::Remote(
            crab_remote_git::Error::RepositoryIndexing { .. }
        ))
    ));
    assert_eq!(
        before,
        manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap()
    );
    owner.renew().await.unwrap();
    owner.release().await.unwrap();
    lease.release().await.unwrap();
    close(&server).await;
}

#[tokio::test]
async fn gc_sweep_blocks_publication_and_releases_preceding_leases() {
    for global in [true, false] {
        let server = fixture().await;
        let repo = repository(&server);
        let lease = commit_without_proof(repo).await;
        let domain = if global {
            repo.layout.global_prefix()
        } else {
            repo.layout.repo_prefix()
        };
        let sweep = GcFenceLease::acquire_sweep(repo.store.inner(), domain, TTL)
            .await
            .unwrap();
        let before = manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap();
        let result = repo
            .open_current(&server, server.options, &CancellationToken::new())
            .await;
        assert!(matches!(
            result,
            Err(crate::Error::Maintenance(WriteError::Coordination(
                CoordinationError::GcFenceHeld { .. }
            )))
        ));
        assert_eq!(
            before,
            manifest_store::read_manifest(&repo.store, &repo.layout)
                .await
                .unwrap()
        );
        sweep.renew().await.unwrap();
        sweep.release().await.unwrap();
        assert_released(repo).await;
        lease.release().await.unwrap();
        close(&server).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnected_reader_retains_publication_until_retry_or_shutdown_drains_it() {
    for shutdown in [false, true] {
        let server = fixture().await;
        let repo = repository(&server);
        let lease = commit_without_proof(repo).await;
        let manifest = PushLock::acquire_internal(
            repo.store.inner(),
            repo.layout.repo_prefix(),
            GIT_MANIFEST_RESOURCE,
            TTL,
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let request_server = Arc::clone(&server);
        let request_cancel = cancel.clone();
        let request = tokio::spawn(async move {
            repository(&request_server)
                .open_current(&request_server, request_server.options, &request_cancel)
                .await
        });
        let owner_path =
            internal_lock_path(repo.layout.repo_prefix(), GIT_GENERATION_OWNER_RESOURCE).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while repo.store.head(&owner_path.as_str().into()).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(matches!(
            request.await.unwrap(),
            Err(crate::Error::Remote(crab_remote_git::Error::Cancelled))
        ));
        assert!(
            repo.maintenance
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        );
        assert_eq!(server.maintenance_admission.available_permits(), 1);
        if shutdown {
            tokio::time::timeout(Duration::from_secs(5), close(&server))
                .await
                .unwrap();
            manifest.release().await.unwrap();
        } else {
            manifest.release().await.unwrap();
            let result = repo
                .open_current(&server, server.options, &CancellationToken::new())
                .await;
            assert!(matches!(
                result,
                Err(crate::Error::Maintenance(
                    WriteError::VisibilityUnavailable { .. }
                ))
            ));
            close(&server).await;
        }
        assert_eq!(server.maintenance_admission.available_permits(), 2);
        assert!(repo.maintenance.lock().await.is_none());
        assert_released(repo).await;
        lease.release().await.unwrap();
    }
}
