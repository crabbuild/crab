//! Cell node lifecycle tests.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crab_cell_app::CompiledApplication;
use crab_cell_host::*;
use crab_cell_runtime::Error;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::follower::FollowerStore;
use crab_cell_runtime::identity::{ApplicationId, Digest, SessionId};
use crab_cell_runtime::ltx::{DiskBudget, Host as ReplicaHost, Limits as ReplicaLimits};
use crab_cell_runtime::node::durability::NodeDurabilityConfig;
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::qualification::{
    QualificationExecution, QualificationOperation, QualificationOperationExecutor,
    QualificationProfile, QualificationWorkload,
};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, RegistryBuilder,
};
use tokio_util::sync::CancellationToken;

/// Host admission bounds mirrored from the node contract.
const MAX_NODE_TASKS: usize = 256;
const MAX_NODE_FACILITIES: usize = 64;

struct Module;

impl CellModule for Module {
    const NAME: &'static str = "host-test";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
            name: "host-test",
            source_digest: Digest::from_bytes([1; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: &[crab_cell_runtime::registry::MigrationDescriptor {
                version: 1,
                sql: "-- host migration v1",
                digest: Digest::from_bytes([
                    0xd7, 0x41, 0xcb, 0x18, 0xae, 0xd4, 0x80, 0xb0, 0xe1, 0x55, 0x8e, 0x34, 0x5a,
                    0x6b, 0xef, 0xf5, 0xe1, 0x60, 0x80, 0x59, 0x06, 0xba, 0xfe, 0x75, 0xff, 0x9f,
                    0xa0, 0x7d, 0x10, 0xe7, 0x77, 0xbf,
                ]),
            }],
            commands: &[],
            queries: &[],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: crab_cell_runtime::NamespaceId::from_bytes([2; 16]),
                name: "host-test",
                role: CatalogRole::Sql,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        };
        &DESCRIPTOR
    }

    fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

fn application() -> Arc<CompiledApplication> {
    let mut builder = crab_cell_app::ApplicationBuilder::new(
        "host-test",
        BuildDescriptor {
            source_revision: "host-test".into(),
            cargo_lock_digest: Digest::from_bytes([7; 32]),
        },
    )
    .unwrap();
    builder.register(Module).unwrap();
    builder
        .cell_type(
            crab_cell_app::CellType::new(
                "host-test",
                "host-test",
                crab_cell_runtime::NamespaceId::from_bytes([2; 16]),
                CatalogRole::Sql,
                1,
            )
            .unwrap(),
        )
        .unwrap();
    Arc::new(builder.finish().unwrap())
}

#[test]
fn builder_rejects_missing_owners_before_starting() {
    let error = match CellNodeBuilder::new(application()).build() {
        Ok(_) => panic!("missing node owners must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Control(_)));
}

#[test]
fn builder_rejects_zero_node_session_before_starting() {
    let result = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([0; 16]))
        .build();
    let error = match result {
        Ok(_) => panic!("zero node sessions must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Control(_)));
}

#[derive(Clone)]
struct QualificationStub;

impl QualificationOperationExecutor for QualificationStub {
    type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        std::future::ready(Ok(
            QualificationExecution::acknowledged(true).with_case(operation.case())
        ))
    }
}

#[tokio::test]
async fn qualification_rejects_a_node_before_readiness() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([32; 16]))
        .build()
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let error = node
        .run_qualification(&workload, &mut QualificationStub)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CellDraining));
}

#[tokio::test]
async fn concurrent_qualification_is_readiness_gated_and_bounded() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([35; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let summary = node
        .run_qualification_concurrent(&workload, QualificationStub, 2)
        .await
        .unwrap();
    assert_eq!(summary.operations(), 56);
    node.shutdown().await.unwrap();
}

struct ObservedQualificationStub;

impl QualificationOperationExecutor for ObservedQualificationStub {
    type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, _operation: QualificationOperation) -> Self::Future<'a> {
        std::future::ready(Ok(QualificationExecution::acknowledged(true)))
    }
}

#[tokio::test]
async fn observed_qualification_preserves_unclaimed_case_coverage() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([36; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let summary = node
        .run_qualification_observed(&workload, &mut ObservedQualificationStub)
        .await
        .unwrap();
    assert!(summary.case_coverage().iter().all(|byte| *byte == 0));
    node.shutdown().await.unwrap();
}

struct NoopNodeDurabilityProvider;

impl NodeDurabilityProvider for NoopNodeDurabilityProvider {
    fn recruit(
        self: Arc<Self>,
        _limits: ReplicaLimits,
        _required_follower_bytes: u64,
        _live_node_limit: usize,
    ) -> Pin<Box<dyn Future<Output = FacilityResult<Option<NodeDurabilityConfig>>> + Send>> {
        Box::pin(async { Ok(None) })
    }
}

#[tokio::test]
async fn node_durability_supervisor_is_host_owned_and_joined() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([28; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    let provider = Arc::new(NoopNodeDurabilityProvider);
    node.install_node_durability_provider(
        Arc::clone(&provider),
        NodeDurabilitySupervisorConfig::new(
            ApplicationId::from_bytes([29; 16]),
            ReplicaLimits::default(),
            1,
            1,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
            1,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        node.owned_component::<NoopNodeDurabilityProvider>(NODE_DURABILITY_PROVIDER_COMPONENT)
            .is_some()
    );
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_node_task_removes_readiness_and_is_reported_during_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([30; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    task_group
        .spawn(async { Err::<(), _>(std::io::Error::other("supervisor failed")) })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert!(!node.is_ready());
    let error = node.drain().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Facility {
            name: "cell-coordination-tasks",
            ..
        }
    ));
}

#[tokio::test]
async fn completed_node_task_removes_readiness() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([33; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());

    task_group.spawn(async { Ok::<(), Error>(()) }).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while node.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn panicked_node_task_removes_readiness_and_is_reported_during_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([31; 16]))
        .build()
        .unwrap();
    let task_group = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    task_group
        .spawn_boxed(async {
            panic!("supervisor panicked");
        })
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!node.is_ready());
    let error = node.drain().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Facility {
            name: "cell-coordination-tasks",
            ..
        }
    ));
}

#[tokio::test]
async fn builder_retains_configured_follower_store_as_an_owned_component() {
    let data_dir = tempfile::tempdir().unwrap();
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([3; 16]))
        .with_follower_store(
            data_dir.path().join("followers"),
            ReplicaLimits::default(),
            DiskBudget::new(1 << 20),
        )
        .build()
        .unwrap();

    assert!(
        node.owned_component::<FollowerStore>(FOLLOWER_STORE_COMPONENT)
            .is_some()
    );
}

#[tokio::test]
async fn node_shutdown_is_idempotent_and_returns_stopped_state() {
    let pool = SqlWorkerPool::new(1, 1).unwrap();
    let host = ReplicaHost::default();
    let node = CellNodeBuilder::new(application())
        .with_runtime(pool, 16 * 1024 * 1024)
        .with_replica_host(host)
        .with_session(SessionId::from_bytes([4; 16]))
        .build()
        .unwrap();
    assert_eq!(node.state(), NodeState::Starting);
    assert!(!node.is_ready());
    let starting = node.status();
    assert_eq!(starting.state(), NodeState::Starting);
    assert!(!starting.is_shutting_down());
    assert_eq!(starting.stats(), node.stats());
    assert!(node.start().is_err());
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert_eq!(node.state(), NodeState::Ready);
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
    assert_eq!(node.state(), NodeState::Stopped);
    let stopped = node.status();
    assert_eq!(stopped.state(), NodeState::Stopped);
    assert!(stopped.is_shutting_down());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_lease_does_not_open_readiness_before_host_startup_finishes() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([19; 16]))
        .build()
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert_eq!(node.state(), NodeState::Starting);
    assert!(!node.is_ready());
    assert!(node.start().is_err());
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn readiness_requires_the_node_task_group() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([22; 16]))
        .build()
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();

    let error = node.start().unwrap_err();
    assert!(matches!(error, Error::Control(_)));
    assert_eq!(node.state(), NodeState::Starting);

    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_group_cancels_both_tokens_and_joins_tasks() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let finished = Arc::new(AtomicBool::new(false));
    let task_cancellation = cancellation.clone();
    let task_shutdown = node_shutdown.clone();
    let task_finished = Arc::clone(&finished);
    let tasks = CellNodeTaskGroup::new(cancellation.clone(), node_shutdown.clone());
    tasks
        .spawn(async move {
            task_cancellation.cancelled().await;
            task_shutdown.cancelled().await;
            task_finished.store(true, Ordering::Release);
            Ok::<(), Error>(())
        })
        .unwrap();

    tasks.drain().await.unwrap();

    assert!(cancellation.is_cancelled());
    assert!(node_shutdown.is_cancelled());
    assert!(finished.load(Ordering::Acquire));
}

#[tokio::test]
async fn task_group_rejects_new_tasks_after_drain_starts() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let task_shutdown = node_shutdown.clone();
    let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown);
    tasks
        .spawn(async move {
            task_shutdown.cancelled().await;
            Ok::<(), Error>(())
        })
        .unwrap();
    tasks.drain().await.unwrap();

    assert!(matches!(
        tasks.spawn(async { Ok::<(), Error>(()) }),
        Err(Error::CellDraining)
    ));
    assert!(matches!(
        tasks.spawn_boxed(async { Ok(()) }),
        Err(Error::CellDraining)
    ));
}

#[tokio::test]
async fn task_group_deadline_aborts_unfinished_tasks() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::clone(&dropped);
    tasks
        .spawn(async move {
            let _probe = DropProbe(task_dropped);
            std::future::pending::<()>().await;
            Ok::<(), Error>(())
        })
        .unwrap();

    let result = tasks
        .drain_until(Some(Instant::now() + std::time::Duration::from_millis(10)))
        .await;

    assert!(result.is_err());
    tokio::task::yield_now().await;
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropping_task_group_aborts_unjoined_tasks() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    {
        let tasks = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
        let task_dropped = Arc::clone(&dropped);
        let task_started = Arc::clone(&started);
        tasks
            .spawn(async move {
                let _probe = DropProbe(task_dropped);
                task_started.notify_one();
                std::future::pending::<()>().await;
                Ok::<(), Error>(())
            })
            .unwrap();
        started.notified().await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn node_deadline_returns_after_a_stalled_facility() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([21; 16]))
        .build()
        .unwrap();
    node.install_facility(
        CellNodeFacility::new("stalled", || async {
            std::future::pending::<FacilityResult>().await
        })
        .unwrap(),
    )
    .unwrap();

    let result = node
        .shutdown_until(Instant::now() + std::time::Duration::from_millis(10))
        .await;

    assert!(result.is_err());
    assert_eq!(node.state(), NodeState::Draining);
}

#[tokio::test]
async fn node_deadline_bounds_a_stalled_coordination_task() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([23; 16]))
        .build()
        .unwrap();
    let tasks = node
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    tasks
        .spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), Error>(())
        })
        .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        node.shutdown_until(Instant::now() + std::time::Duration::from_millis(10)),
    )
    .await
    .expect("deadline-aware shutdown must return");

    assert!(result.is_err());
    assert_eq!(node.state(), NodeState::Draining);
}

#[tokio::test]
async fn node_owns_one_task_group_and_drains_it() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([20; 16]))
        .build()
        .unwrap();
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let tasks = node
        .install_task_group(cancellation.clone(), node_shutdown.clone())
        .unwrap();
    assert!(
        node.install_task_group(CancellationToken::new(), CancellationToken::new())
            .is_err()
    );
    let finished = Arc::new(AtomicBool::new(false));
    let task_finished = Arc::clone(&finished);
    let task_shutdown = node_shutdown.clone();
    tasks
        .spawn(async move {
            task_shutdown.cancelled().await;
            task_finished.store(true, Ordering::Release);
            Ok::<(), Error>(())
        })
        .unwrap();

    node.shutdown().await.unwrap();

    assert!(cancellation.is_cancelled());
    assert!(node_shutdown.is_cancelled());
    assert!(finished.load(Ordering::Acquire));
}

#[tokio::test]
async fn node_cancels_admission_before_draining_provider_facilities() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([27; 16]))
        .build()
        .unwrap();
    let cancellation = CancellationToken::new();
    node.install_task_group(cancellation.clone(), CancellationToken::new())
        .unwrap();
    let observed = Arc::new(AtomicBool::new(false));
    let facility_observed = Arc::clone(&observed);
    node.install_facility(
        CellNodeFacility::new("provider", move || {
            let cancellation = cancellation.clone();
            let facility_observed = facility_observed.clone();
            async move {
                cancellation.cancelled().await;
                facility_observed.store(true, Ordering::Release);
                Ok(())
            }
        })
        .unwrap(),
    )
    .unwrap();

    node.shutdown_until(Instant::now() + std::time::Duration::from_secs(1))
        .await
        .unwrap();

    assert!(observed.load(Ordering::Acquire));
}

#[tokio::test]
async fn node_retains_typed_components_without_duplicate_names() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([26; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    let component = Arc::new(7_u64);
    let weak = Arc::downgrade(&component);
    node.install_facility(CellNodeFacility::new("unowned", || async { Ok(()) }).unwrap())
        .unwrap();
    assert!(
        node.install_owned_component("unowned", Arc::new(6_u64))
            .is_err()
    );
    node.install_owned_component("fixture-component", Arc::clone(&component))
        .unwrap();
    drop(component);
    assert_eq!(
        node.owned_component::<u64>("fixture-component").as_deref(),
        Some(&7)
    );
    assert!(
        node.install_owned_component("fixture-component", Arc::new(8_u64))
            .is_err()
    );
    node.shutdown().await.unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn scale_down_stops_acquisition_without_stopping_the_host() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([97; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.start().unwrap();
    let status = node
        .drain_for_scale_down(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(status.ready_to_stop());
    assert_eq!(node.state(), NodeState::Stopped);
    assert!(!node.is_ready());
    assert!(!node.runtime().is_acquiring());
    let retry = node
        .drain_for_scale_down(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(retry.ready_to_stop());
    node.drain().await.unwrap();
}

#[tokio::test]
async fn facility_batch_installation_is_atomic_on_name_conflict() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([30; 16]))
        .build()
        .unwrap();
    node.install_owned_component("existing", Arc::new(1_u64))
        .unwrap();
    let first = CellNodeFacility::owned("first", Arc::new(2_u64), || async { Ok(()) }).unwrap();
    let duplicate =
        CellNodeFacility::owned("existing", Arc::new(3_u64), || async { Ok(()) }).unwrap();

    assert!(node.install_facilities([first, duplicate]).is_err());
    assert!(node.owned_component::<u64>("first").is_none());
    assert_eq!(node.owned_component::<u64>("existing").as_deref(), Some(&1));
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn readiness_requires_declared_owned_components() {
    let node = CellNodeBuilder::new(application())
        .with_required_owned_components(["catalog", "router"])
        .unwrap()
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([28; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    node.install_owned_component("catalog", Arc::new(1_u64))
        .unwrap();

    assert!(matches!(node.start(), Err(Error::Control(_))));
    node.install_owned_component("router", Arc::new(2_u64))
        .unwrap();
    node.start().unwrap();
    assert!(node.is_ready());
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn facility_registration_is_frozen_after_readiness() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([34; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    assert!(node.is_ready());

    assert!(matches!(
        node.install_facility(CellNodeFacility::new("late", || async { Ok(()) }).unwrap()),
        Err(Error::CellDraining)
    ));
    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn required_component_declaration_rejects_duplicates_and_late_changes() {
    assert!(
        CellNodeBuilder::new(application())
            .with_required_owned_components(["catalog", "catalog"])
            .is_err()
    );
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([29; 16]))
        .build()
        .unwrap();
    assert!(
        node.require_owned_components(["catalog", "catalog"])
            .is_err()
    );
    node.require_owned_components(["catalog"]).unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap_err();
    assert!(node.require_owned_components(["router"]).is_ok());
    node.shutdown().await.unwrap();
    assert!(node.require_owned_components(["late"]).is_err());
}

#[tokio::test]
async fn task_group_rejects_tasks_above_bound() {
    let cancellation = CancellationToken::new();
    let node_shutdown = CancellationToken::new();
    let tasks = CellNodeTaskGroup::new(cancellation, node_shutdown.clone());
    for _ in 0..MAX_NODE_TASKS {
        let node_shutdown = node_shutdown.clone();
        tasks
            .spawn(async move {
                node_shutdown.cancelled().await;
                Ok::<(), Error>(())
            })
            .unwrap();
    }
    let node_shutdown = node_shutdown.clone();
    let error = tasks
        .spawn(async move {
            node_shutdown.cancelled().await;
            Ok::<(), Error>(())
        })
        .unwrap_err();
    assert!(matches!(error, Error::Capacity(_)));

    tasks.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_waits_for_the_single_runtime_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([5; 16]))
        .build()
        .unwrap();
    let first = node.shutdown();
    let second = node.shutdown();
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_eq!(node.state(), NodeState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_nodes_have_independent_lifecycle_and_resource_ledgers() {
    let mut nodes = Vec::new();
    for index in 0..3_u8 {
        let node = CellNodeBuilder::new(application())
            .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
            .with_replica_host(ReplicaHost::default())
            .with_session(SessionId::from_bytes([index + 10; 16]))
            .build()
            .unwrap();
        assert_eq!(node.state(), NodeState::Starting);
        assert!(!node.is_ready());
        nodes.push(node);
    }

    for node in &nodes {
        node.shutdown().await.unwrap();
        assert_eq!(node.state(), NodeState::Stopped);
        assert!(node.is_shutting_down());
    }
}

#[tokio::test]
async fn facilities_drain_in_reverse_registration_order() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([16; 16]))
        .build()
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    for name in ["storage", "transport", "scheduler"] {
        let events = Arc::clone(&events);
        node.install_facility(
            CellNodeFacility::new(name, move || {
                let events = Arc::clone(&events);
                async move {
                    events
                        .lock()
                        .map_err(|_| {
                            Box::new(std::io::Error::other("event lock poisoned"))
                                as Box<dyn std::error::Error + Send + Sync>
                        })?
                        .push(name);
                    Ok(())
                }
            })
            .unwrap(),
        )
        .unwrap();
    }

    node.shutdown().await.unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec!["scheduler", "transport", "storage"]
    );
}

#[tokio::test]
async fn facility_failure_is_reported_after_all_facilities_attempt_and_runtime_drains() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([17; 16]))
        .build()
        .unwrap();
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);
    node.install_facility(
        CellNodeFacility::new("healthy", move || {
            let completed = Arc::clone(&completed_clone);
            async move {
                completed.store(true, Ordering::Release);
                Ok(())
            }
        })
        .unwrap(),
    )
    .unwrap();
    node.install_facility(
        CellNodeFacility::new("broken", || async {
            Err(Box::new(std::io::Error::other("drain failed"))
                as Box<dyn std::error::Error + Send + Sync>)
        })
        .unwrap(),
    )
    .unwrap();

    let error = node.shutdown().await.unwrap_err();
    assert!(matches!(error, Error::Facility { name: "broken", .. }));
    assert!(completed.load(Ordering::Acquire));
    assert!(node.is_shutting_down());
    assert_eq!(node.state(), NodeState::Draining);
}

#[tokio::test]
async fn facility_registration_is_bounded_and_rejected_after_drain() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([18; 16]))
        .build()
        .unwrap();
    assert!(CellNodeFacility::new("", || async { Ok(()) }).is_err());
    for index in 0..MAX_NODE_FACILITIES {
        let name = Box::leak(format!("facility-{index}").into_boxed_str());
        node.install_facility(CellNodeFacility::new(name, || async { Ok(()) }).unwrap())
            .unwrap();
    }
    assert!(matches!(
        node.install_facility(CellNodeFacility::new("overflow", || async { Ok(()) }).unwrap()),
        Err(Error::Capacity(_))
    ));
    node.shutdown().await.unwrap();
    assert!(matches!(
        node.install_facility(CellNodeFacility::new("late", || async { Ok(()) }).unwrap()),
        Err(Error::CellDraining)
    ));
}
