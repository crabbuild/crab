//! Process-kill qualification for a sparse owner after exact-root publication.

use super::*;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const CHILD_PATH: &str = "runtime::lifecycle::ownership::sparse_process::rustfs_sparse_owner_child";
const ACTIVE_PATH_ENV: &str = "CRAB_LTX_SPARSE_OWNER_ACTIVE";
const READY_PATH_ENV: &str = "CRAB_LTX_SPARSE_OWNER_READY";

fn rustfs_store() -> Store {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    build_explicit_store(
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
    .unwrap()
}

fn fixture() -> (CellTarget, CellStorageLayout, CellReplica, CellAuthority) {
    let target = CellTarget::new(
        TenantId::from_bytes([61; 16]),
        ApplicationId::from_bytes([62; 16]),
        NamespaceId::from_bytes([63; 16]),
        b"sparse-process-owner",
    )
    .unwrap();
    let prefix = std::env::var("CRAB_CELL_TEST_PREFIX").unwrap();
    let layout = CellStorageLayout::new(
        rustfs_store(),
        Path::from(format!("{prefix}/sparse-process-owner")),
        [62; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        [64; 16],
        Limits::default(),
    )
    .unwrap();
    let authority = CellAuthority::new(layout.clone());
    (target, layout, replica, authority)
}

fn recovering_owner(control: &mut crab_cell_runtime::control::Control, session: SessionId) {
    control.epoch += 1;
    control.revision += 1;
    control.progress += 1;
    control.state = ControlState::Recovering;
    control.owner = Some(Owner {
        session,
        endpoint: "https://sparse-owner.internal:8081".into(),
    });
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rustfs_sparse_owner_child() {
    let Ok(active) = std::env::var(ACTIVE_PATH_ENV) else {
        return;
    };
    let ready = PathBuf::from(std::env::var(READY_PATH_ENV).unwrap());
    let active = PathBuf::from(active);
    let (target, layout, replica, authority) = fixture();
    let catalog =
        crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog.lookup(target.cell_id()).await.unwrap().unwrap();
    let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        observed.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([66; 16])
    );
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        SessionId::from_bytes([66; 16]),
    )
    .unwrap();
    // This activation always enters through the paged root path on a fresh
    // destination; its local checksum base is file-backed.
    let handle = runtime
        .activate_restored(
            proof,
            replica,
            authority.clone(),
            observed,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout,
                Limits::default(),
            ),
            active.clone(),
        )
        .await
        .unwrap();
    let mut sidecar = active.as_os_str().to_owned();
    sidecar.push(".crab-ltx-checksums");
    assert!(PathBuf::from(sidecar).is_file());
    let outcome = handle
        .execute(
            mutation_identity_window(67, 10, 10_000),
            Digest::from_bytes([68; 32]),
            20,
            1_024,
            1_024,
            |tx| {
                tx.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"published".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));
    let published = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(published.value().root.as_ref().unwrap().commit_sequence, 1);
    std::fs::write(ready, b"published").unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_process_killed_sparse_owner_restores_published_root_and_continues() {
    let (target, layout, replica, authority) = fixture();
    let catalog =
        crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                Digest::from_bytes([65; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let first_session = SessionId::from_bytes([69; 16]);
    let initial = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([64; 16]),
            Owner {
                session: first_session,
                endpoint: "https://bootstrap.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let bootstrap_dir = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let bootstrap = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            initial,
            bootstrap_dir.path().join("cell.sqlite"),
            |tx| {
                tx.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES(0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    bootstrap.drain().await.unwrap();
    drop(runtime);
    bootstrap_dir.close().unwrap();

    let current = authority.load(target.cell_id()).await.unwrap().unwrap();
    let mut child_control = current.value().clone();
    recovering_owner(&mut child_control, SessionId::from_bytes([66; 16]));
    authority
        .transition(&current, child_control, Transition::Takeover)
        .await
        .unwrap();

    let child_dir = tempfile::TempDir::new().unwrap();
    let ready = child_dir.path().join("ready");
    let active = child_dir.path().join("active.sqlite");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_PATH, "--nocapture"])
        .env(ACTIVE_PATH_ENV, &active)
        .env(READY_PATH_ENV, &ready)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "sparse owner exited before publication"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "sparse owner publication timed out"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    child_dir.close().unwrap();

    let current = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(current.value().root.as_ref().unwrap().commit_sequence, 1);
    let successor_session = SessionId::from_bytes([70; 16]);
    let mut successor_control = current.value().clone();
    recovering_owner(&mut successor_control, successor_session);
    let takeover = authority
        .transition(&current, successor_control, Transition::Takeover)
        .await
        .unwrap();
    let successor_dir = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        successor_session,
    )
    .unwrap();
    let successor = runtime
        .activate_restored(
            proof,
            replica,
            authority.clone(),
            takeover,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout,
                Limits::default(),
            ),
            successor_dir.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    let outcome = successor
        .execute(
            mutation_identity_window(71, 10, 10_000),
            Digest::from_bytes([72; 32]),
            21,
            1_024,
            1_024,
            |tx| {
                let value =
                    tx.query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                tx.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        StoredOutcome::Success { ref result, commit_sequence: 2 } if result == &1_i64.to_be_bytes()
    ));
    successor.drain().await.unwrap();
    assert_eq!(
        authority
            .load(target.cell_id())
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
