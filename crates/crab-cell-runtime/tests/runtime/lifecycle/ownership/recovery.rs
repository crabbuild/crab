//! Recovery overlays, pinned tails, and source-loss takeover.

use super::*;

#[tokio::test]
async fn takeover_resumes_pinned_recovery_before_serving() {
    recover_retained_tail(false).await;
}
#[tokio::test]
async fn recovery_seals_already_rooted_tail_without_an_empty_manifest() {
    recover_retained_tail(true).await;
}
async fn recover_retained_tail(rooted: bool) {
    let fixture = fixture_for(b"recovered-takeover");
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    drop(handle);

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let predecessor = stale.value().ltx_root().unwrap();
    let leader = stale.value().owner.as_ref().unwrap().session;

    let tail_directory = tempfile::TempDir::new().unwrap();
    let tail_path = tail_directory.path().join("tail.sqlite");
    let writable = fixture
        .replica
        .open_root(&predecessor)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&tail_path)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&tail_path).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            transaction.execute(
                "UPDATE sys_meta SET commit_sequence = commit_sequence + 1, logical_time_ms = logical_time_ms + 1 WHERE singleton = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let capture = writer.capture().unwrap();
    let mut frames = Vec::with_capacity(capture.segments.len());
    for (index, segment) in capture.segments.iter().enumerate() {
        frames.push(
            crab_ltx::encode_node_frame(
                crab_ltx::NodeFrameScope {
                    leader_session: *leader.as_bytes(),
                    log_epoch: 1,
                    node_sequence: u64::try_from(index).unwrap() + 1,
                    application: *fixture.layout.application_id(),
                    cell: *fixture.target.cell_id().as_bytes(),
                    incarnation: *stale.value().incarnation.as_bytes(),
                    cell_epoch: stale.value().epoch,
                    commit_sequence: predecessor.commit_sequence + 1,
                },
                segment.info().clone(),
                Bytes::from(std::fs::read(segment.path()).unwrap()),
                Limits::default(),
            )
            .unwrap()
            .encoded()
            .clone(),
        );
    }
    if rooted {
        // Crash after the exact Cell root CAS but before shared node coverage.
        let prepared = fixture
            .replica
            .prepare(
                Some(&predecessor),
                &capture,
                predecessor.commit_sequence + 1,
                stale.value().schema,
            )
            .await
            .unwrap();
        authority
            .transition(
                &stale,
                stale.value().publish_prepared(&prepared, None).unwrap(),
                Transition::Publish,
            )
            .await
            .unwrap();
    }
    writer.close().unwrap();
    let follower = SessionId::from_bytes([43; 16]);
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        crab_cell_runtime::ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn crab_cell_runtime::node::log_transport::NodeLogTransport> = Arc::new(
        crab_cell_runtime::node::log_transport::LocalFollowerTransport::new(
            crab_cell_runtime::identity::NodeId::from_bytes(*follower.as_bytes()),
            follower_store,
        ),
    );
    transport
        .append(
            crab_cell_runtime::identity::NodeId::from_bytes(*follower.as_bytes()),
            crab_cell_runtime::node::log_transport::AppendRequest {
                leader_session: leader,
                log_epoch: 1,
                frames,
                covered_through: 0,
            },
        )
        .await
        .unwrap();
    let manifests = crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
        fixture.layout.clone(),
        Limits::default(),
    );
    let successor = SessionId::from_bytes([42; 16]);
    let fenced = fence_log_session(&fixture.layout, leader, successor, follower, 0).await;
    assert!(matches!(
        fenced.direct_takeover(),
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
    let recovery = crab_cell_runtime::node::log_recovery::NodeLogRecovery::from_fenced(
        Arc::clone(&transport),
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let coordinator = crab_cell_runtime::node::log_recovery::RecoveryCoordinator::new(
        recovery,
        manifests.clone(),
    );
    let inventory =
        crab_cell_runtime::node::log_recovery::recoverable_cells(&catalog, &authority, leader, 10)
            .await
            .unwrap();
    assert_eq!(inventory.len(), 1);
    if rooted {
        let directory = crab_cell_runtime::node::NodeDirectory::new(
            fixture.layout.clone(),
            Digest::from_bytes([90; 32]),
            Digest::from_bytes([91; 32]),
            Digest::from_bytes([92; 32]),
        );
        let completed = coordinator
            .recover_and_seal(&directory, fenced, inventory, 10_002)
            .await
            .unwrap();
        assert!(completed.controls.is_empty());
        assert_eq!(
            completed.sealed.log().phase(),
            crab_cell_runtime::node::log_state::NodeLogPhase::Sealed
        );
        return;
    }
    let attached = coordinator
        .recover(fenced.clone(), inventory)
        .await
        .unwrap();
    assert_eq!(attached.len(), 1);
    drop(coordinator);
    let resumed_recovery = crab_cell_runtime::node::log_recovery::NodeLogRecovery::from_fenced(
        transport,
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let resumed = crab_cell_runtime::node::log_recovery::RecoveryCoordinator::new(
        resumed_recovery,
        manifests.clone(),
    );
    let directory = crab_cell_runtime::node::NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let completed = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::node::log_recovery::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: attached[0].clone(),
            }],
            10_002,
        )
        .await
        .unwrap();
    assert_eq!(completed.controls[0].value(), attached[0].value());
    assert_eq!(
        completed.sealed.log().phase(),
        crab_cell_runtime::node::log_state::NodeLogPhase::Sealed
    );
    let repeated = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::node::log_recovery::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: completed.controls[0].clone(),
            }],
            10_003,
        )
        .await
        .unwrap();
    assert_eq!(repeated.sealed, completed.sealed);
    let attached = repeated.controls.into_iter().next().unwrap();
    let takeover = repeated.takeover;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            attached,
            takeover,
            manifests,
            fixture._directory.path().join("recovered-takeover.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://recovered-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        1_i64.to_be_bytes()
    );
    let serving = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serving.value().state, ControlState::Serving);
    assert!(serving.value().recovery.is_none());
    assert_eq!(
        serving.value().root.as_ref().unwrap().commit_sequence,
        predecessor.commit_sequence + 1
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn unchanged_unpublished_owner_is_taken_over_then_bootstrapped() {
    let fixture = fixture();
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://stopped-import.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let fenced = fence_session(
        &fixture.layout,
        stale.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([42; 16]),
    )
    .await;
    let takeover = fenced.direct_takeover().unwrap();
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .takeover_unpublished(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            takeover,
            fixture
                ._directory
                .path()
                .join("takeover-unpublished.sqlite"),
            Owner {
                session,
                endpoint: "https://import-successor.internal:8081".into(),
            },
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (7)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        7_i64.to_be_bytes()
    );
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}
#[tokio::test]
async fn slow_bootstrap_renews_unpublished_ownership_before_publication() {
    let fixture = fixture();
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let session = SessionId::from_bytes([43; 16]);
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://slow-bootstrap.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            observed,
            fixture._directory.path().join("slow-bootstrap.sqlite"),
            |transaction| {
                std::thread::sleep(std::time::Duration::from_secs(4));
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let published = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.value().state, ControlState::Serving);
    assert!(published.value().revision >= 3);
    handle.drain().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn source_loss_takeover_restores_exact_root_and_continues_publication() {
    source_loss_takeover(
        Store::new(Arc::new(InMemory::new())),
        Path::from("cold-runtime"),
    )
    .await;
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_source_loss_takeover_restores_exact_root_and_continues_publication() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let prefix = Path::from(format!(
        "{}/cold-runtime",
        required("CRAB_CELL_TEST_PREFIX")
    ));
    source_loss_takeover(store, prefix).await;
}
async fn source_loss_takeover(store: Store, prefix: Path) {
    let target = CellTarget::new(
        TenantId::from_bytes([41; 16]),
        ApplicationId::from_bytes([42; 16]),
        NamespaceId::from_bytes([43; 16]),
        b"repository-cold-start",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([44; 16]);
    let layout = CellStorageLayout::new(store, prefix, [42; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog =
        crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                Digest::from_bytes([45; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();

    let first_session = SessionId::from_bytes([46; 16]);
    let authority = CellAuthority::new(layout.clone());
    let recovering = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://node-one.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let first_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let first = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            recovering,
            first_local.path().join("cell.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let first_identity = mutation_identity_window(47, 10, 10_000);
    let first_digest = Digest::from_bytes([48; 32]);
    let first_outcome = first
        .execute(
            first_identity,
            first_digest,
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"first".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        first_outcome,
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));
    first.drain().await.unwrap();
    drop(runtime);
    first_local.close().unwrap();

    let current = authority.load(cell).await.unwrap().unwrap();
    eprintln!(
        "fault_seed=47 schedule=source_loss_before_owner_takeover request={first_identity:?} operation={first_digest:?} committed_sequence={} selected_follower_tickets=[] root={:?} observed_control={:?}",
        first_outcome.commit_sequence(),
        current.value().ltx_root(),
        current.value(),
    );
    let second_session = SessionId::from_bytes([49; 16]);
    let mut takeover = current.value().clone();
    takeover.epoch += 1;
    takeover.revision += 1;
    takeover.progress += 1;
    takeover.state = ControlState::Recovering;
    takeover.owner = Some(Owner {
        session: second_session,
        endpoint: "https://node-two.internal:8081".into(),
    });
    let takeover = authority
        .transition(&current, takeover, Transition::Takeover)
        .await
        .unwrap();

    let second_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let second = runtime
        .activate_restored(
            proof,
            replica,
            authority.clone(),
            takeover,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout.clone(),
                Limits::default(),
            ),
            second_local.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    assert_eq!(
        authority.load(cell).await.unwrap().unwrap().value().state,
        ControlState::Serving
    );
    assert_eq!(
        second
            .resolve(first_identity, first_digest, 21, 1_024)
            .await
            .unwrap(),
        Resolution::Committed(first_outcome)
    );
    assert!(matches!(
        second
            .execute(
                mutation_identity_window(50, 10, 10_000),
                Digest::from_bytes([51; 32]),
                21,
                1_024,
                1_024,
                |transaction| {
                    let value = transaction
                        .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 2 }
            if result == &1_i64.to_be_bytes()
    ));
    second.drain().await.unwrap();
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        2
    );
}
