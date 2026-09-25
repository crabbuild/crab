use std::{sync::Arc, time::Duration};

use crab_cell_app::ApplicationHandle;
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_runtime::Error;
use crab_cell_runtime::cell::actor::CellHandle;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::identity::{ApplicationId, TenantId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::primitives::blob::BlobArtifactStore;
use crab_cell_runtime::primitives::blob::install_blob_schema;
use crab_cell_runtime::primitives::cron::install_cron_schema;
use crab_cell_runtime::primitives::kv::install_kv_schema;
use crab_cell_runtime::primitives::queue::install_queue_schema;
use crab_cell_runtime::primitives::workflow::install_workflow_schema;
use crab_cell_runtime::registry::Registry;
use crab_storage::Store;
use object_store::path::Path;
use tokio_util::sync::CancellationToken;

use crate::fixture;

pub type PublicHostFixture = (
    CellNode,
    ApplicationHandle<fixture::ReferenceApplication>,
    TenantId,
    ApplicationId,
    tempfile::TempDir,
    Arc<Registry>,
    Vec<CellHandle>,
    Store,
);

pub async fn public_host_fixture_with_store(store: Store, root: Path) -> PublicHostFixture {
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
    let directory = tempfile::tempdir().expect("qualification directory");
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let node = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(4, 32).expect("qualification pool"),
            64 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(session)
        .build()
        .expect("qualification node");
    let cancellation = CancellationToken::new();
    let tasks = node
        .install_task_group(cancellation.clone(), CancellationToken::new())
        .expect("qualification task group");
    let lease = NodeLeaseGuard::new(0, 60_000).expect("qualification lease");
    node.install_node_lease(lease.clone())
        .expect("qualification readiness");
    // This smoke has no authority publisher; keep its test lease live until
    // the node-owned task group cancels and joins the renewal loop on drain.
    tasks
        .spawn(async move {
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return Ok::<(), Error>(()),
                    () = tokio::time::sleep(Duration::from_secs(20)) => lease.renew(0, 60_000)?,
                }
            }
        })
        .expect("qualification lease renewal task");
    assert!(node.is_ready());

    let runtime = node.runtime();
    let registry = application.registry();
    let handles = vec![
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::SQL_NAMESPACE,
            CatalogRole::Sql,
            fixture::SQL_MODULE,
            0,
            40,
            fixture::install_reference_sql_schema,
        )
        .await
        .expect("SQL Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::KV_NAMESPACE,
            CatalogRole::Kv,
            fixture::KV_MODULE,
            0,
            41,
            install_kv_schema,
        )
        .await
        .expect("KV Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::BLOB_NAMESPACE,
            CatalogRole::Blob,
            fixture::BLOB_MODULE,
            0,
            42,
            install_blob_schema,
        )
        .await
        .expect("Blob Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::QUEUE_NAMESPACE,
            CatalogRole::Queue,
            fixture::QUEUE_MODULE,
            0,
            43,
            install_queue_schema,
        )
        .await
        .expect("Queue Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
            fixture::DEAD_LETTER_MODULE,
            0,
            44,
            install_queue_schema,
        )
        .await
        .expect("dead-letter Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::CRON_NAMESPACE,
            CatalogRole::Cron,
            fixture::CRON_MODULE,
            0,
            45,
            install_cron_schema,
        )
        .await
        .expect("Cron Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            fixture::WORKFLOW_MODULE,
            0,
            46,
            install_workflow_schema,
        )
        .await
        .expect("Workflow Cell"),
    ];
    let client = CellClient::local_many(Arc::clone(&registry), handles.clone())
        .expect("qualification client");
    let typed = node
        .application_handle::<fixture::ReferenceApplication>(client, tenant, application_id)
        .with_blob_artifact_store(BlobArtifactStore::new(store.clone()));
    (
        node,
        typed,
        tenant,
        application_id,
        directory,
        registry,
        handles,
        store,
    )
}
