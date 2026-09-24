//! Cell node host integration tests.
//!
//! The suite is one test binary. Its modules live in `tests/node/`; the shared
//! application fixture stays in the suite module, so no target needs a `#[path]`
//! attribute.

mod node {
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
                        0xd7, 0x41, 0xcb, 0x18, 0xae, 0xd4, 0x80, 0xb0, 0xe1, 0x55, 0x8e, 0x34,
                        0x5a, 0x6b, 0xef, 0xf5, 0xe1, 0x60, 0x80, 0x59, 0x06, 0xba, 0xfe, 0x75,
                        0xff, 0x9f, 0xa0, 0x7d, 0x10, 0xe7, 0x77, 0xbf,
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

    pub mod builder;
    pub mod components;
    pub mod lifecycle;
    pub mod qualification;
    pub mod tasks;
}
