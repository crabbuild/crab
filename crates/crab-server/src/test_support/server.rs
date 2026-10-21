use std::{collections::BTreeMap, sync::Arc};

use crab_remote_git::RepositoryIdentity;
use crab_storage::{Store, StoreLayout};

use crate::{RepositoryConfig, repository::Repository, server::Server};

pub(crate) async fn fixture_without_cells() -> Arc<Server> {
    fixture_with_store(Store::new(Arc::new(object_store::memory::InMemory::new()))).await
}

pub(crate) async fn fixture_with_store(store: Store) -> Arc<Server> {
    let admission_store = store.clone();
    let layout = StoreLayout::new(store.clone(), "maintenance".into());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    Arc::new({
        Server::for_test(
            BTreeMap::from([(
                ("team".into(), "repo".into()),
                Repository::new(
                    uuid::Uuid::from_bytes([1; 16]),
                    RepositoryConfig {
                        owner: "team".into(),
                        name: "repo".into(),
                        bucket: "memory".into(),
                        prefix: "maintenance".into(),
                        default_branch: "main".into(),
                        description: String::new(),
                        members: vec![],
                        protected_branches: vec![],
                    },
                    layout,
                    RepositoryIdentity::new("memory", "maintenance", 1).unwrap(),
                ),
            )])
            .into(),
            crate::transfer_admission::TransferAdmission::new(
                admission_store,
                "test/.crab/http-server/v1/admission".into(),
                4,
            ),
        )
    })
}

/// Surfaces Cell runtime warnings in this crate's test output.
///
/// Test binaries install no subscriber, so a fail-closed library warning (for
/// example a host outliving its Cell runtime) would otherwise be invisible in
/// the very runs that hit it.
pub(crate) fn init_test_tracing() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("cellar=warn,crab_server=warn")
            .with_test_writer()
            .try_init();
    });
}

pub(crate) async fn fixture() -> Arc<Server> {
    init_test_tracing();
    let mut server = fixture_without_cells().await;
    let cells = crate::cells::runtime::TestRuntime::start("127.0.0.1:8788".parse().unwrap()).await;
    for repository in server.repositories.values() {
        cells
            .client
            .repository(repository.id)
            .unwrap()
            .client
            .initialize(repository.id)
            .await
            .unwrap();
    }
    let mutable = Arc::get_mut(&mut server).unwrap();
    mutable.cells = Some(cells.client.clone());
    mutable.cell_test_runtime = Some(cells);
    server
}

pub(crate) async fn close(server: &Server) {
    server.cancellation.cancel();
    server.finish_maintenance().await.unwrap();
    server.shutdown_runtimes().await.unwrap();
}
