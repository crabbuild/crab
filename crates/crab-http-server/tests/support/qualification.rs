use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_host::CellNode;
use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::identity::RequestId;
use crab_storage::{ObjectStoreCredentials, Store, build_explicit_store};
use object_store::path::Path;

static RUSTFS_RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn fixed_id(index: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&index.to_be_bytes());
    id
}

pub fn identity(index: u64, now_ms: i64) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(fixed_id(index)),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

pub fn assert_zero_reservations(node: &CellNode) {
    let stats = node.stats();
    assert!(
        stats.active_cells() == 0
            && stats.resident_bytes() == 0
            && stats.file_descriptors() == 0
            && stats.retained_bytes() == 0
            && stats.worker_jobs() == 0
            && stats.primitive_jobs() == 0
            && stats.hydration_jobs() == 0
            && stats.io_slots() == 0
            && stats.blocking_jobs() == 0
            && stats.recovery_jobs() == 0
            && stats.dirty_jobs() == 0
            && stats.scratch_units() == 0
            && stats.local_disk_reserved_bytes() == 0
            && stats.unpublished_node_log_bytes() == 0,
        "qualification left runtime reservations: {stats:?}"
    );
}

pub fn rustfs_public_store() -> (Store, Path) {
    let required = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let bucket = required("CRAB_CELL_TEST_BUCKET");
    let endpoint = required("CRAB_CELL_TEST_ENDPOINT");
    let configured_prefix = required("CRAB_CELL_TEST_PREFIX");
    let store = build_explicit_store(
        &bucket,
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        true,
    )
    .expect("RustFS object store");
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = Path::from(format!(
        "{configured_prefix}/public-typed-host-{}-{run_id}-{}",
        std::process::id(),
        RUSTFS_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    (store, root)
}
