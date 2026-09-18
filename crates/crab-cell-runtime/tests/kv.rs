use std::{sync::Arc, time::UNIX_EPOCH};

mod support;

use crab_cell_runtime::{
    ApplicationId, BuildDescriptor, CatalogEntry, CatalogRole, CellAuthority, CellCatalog,
    CellClient, CellModule, CellRuntime, CellTarget, Digest, IncarnationId, InvocationError,
    KvAtomicOutcome, KvAtomicRequest, KvCheck, KvCondition, KvListRequest, KvModule, KvMutation,
    KvNamespace, MigrationDescriptor, ModuleDescriptor, MutationIdentity, NamespaceDescriptor,
    NamespaceId, OperationDescriptor, Owner, RegistryBuilder, RequestId, SessionId, SqlWorkerPool,
    TenantId, install_kv_schema, install_runtime_schema, kv_atomic, kv_cleanup_expired, kv_get,
    kv_list, register_kv,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

const KV_MODULE: &str = "kv-test";
const KV_NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
const KV_MIGRATION: &str = include_str!("../src/migrations/kv.sql");

struct TestKv;

impl KvModule for TestKv {
    const MODULE: &'static str = KV_MODULE;
    const ATOMIC_COMMAND_ID: u32 = 1;
    const GET_QUERY_ID: u32 = 1;
    const LIST_QUERY_ID: u32 = 2;
}

impl CellModule for TestKv {
    const NAME: &'static str = KV_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<ModuleDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: KV_MODULE,
            source_digest: Digest::from_bytes([4; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: KV_MIGRATION,
                digest: Digest::from_bytes(*blake3::hash(KV_MIGRATION.as_bytes()).as_bytes()),
            }])),
            commands: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 1,
                input_limit: 1024 * 1024,
                output_limit: 1024 * 1024,
            }],
            queries: &[
                OperationDescriptor {
                    id: 1,
                    codec_version: 1,
                    schema_min: 1,
                    schema_max: 1,
                    input_limit: 4096,
                    output_limit: 70 * 1024,
                },
                OperationDescriptor {
                    id: 2,
                    codec_version: 1,
                    schema_min: 1,
                    schema_max: 1,
                    input_limit: 4096,
                    output_limit: 1024 * 1024,
                },
            ],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: KV_NAMESPACE,
                name: KV_MODULE,
                role: CatalogRole::Kv,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        register_kv::<Self>(registry)
    }
}

fn kv_registry() -> Arc<crab_cell_runtime::Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "kv-api-test".into(),
        cargo_lock_digest: Digest::from_bytes([5; 32]),
    });
    builder.register(TestKv).unwrap();
    Arc::new(builder.finish().unwrap())
}

fn current_identity(byte: u8) -> MutationIdentity {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

fn connection() -> crab_ltx::rusqlite::Connection {
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        crab_cell_runtime::CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    let transaction = connection.transaction().unwrap();
    install_kv_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

fn put(key: &[u8], value: &[u8], expires_at_ms: Option<i64>) -> KvMutation {
    KvMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
        expires_at_ms,
    }
}

#[test]
fn atomic_checks_precede_ordered_mutations_and_versions_never_repeat() {
    let mut connection = connection();
    let first = KvAtomicRequest {
        scope: b"repo".to_vec(),
        checks: vec![KvCheck {
            key: b"key".to_vec(),
            condition: KvCondition::Absent,
        }],
        mutations: vec![put(b"key", b"one", None)],
    };
    let transaction = connection.transaction().unwrap();
    let outcome = kv_atomic(&transaction, 10, &first).unwrap();
    let KvAtomicOutcome::Applied(results) = outcome else {
        panic!("expected applied KV mutation");
    };
    let first_version = results[0].version.unwrap();
    transaction
        .execute(
            "UPDATE sys_meta SET commit_sequence = 1 WHERE singleton = 1",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(&first_version[..16], &[2; 16]);
    assert_eq!(&first_version[16..24], &1_u64.to_be_bytes());
    assert_eq!(&first_version[24..], &0_u32.to_be_bytes());

    let failed = KvAtomicRequest {
        scope: b"repo".to_vec(),
        checks: vec![KvCheck {
            key: b"key".to_vec(),
            condition: KvCondition::Absent,
        }],
        mutations: vec![put(b"key", b"wrong", None)],
    };
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        kv_atomic(&transaction, 11, &failed).unwrap(),
        KvAtomicOutcome::PreconditionFailed {
            key: b"key".to_vec()
        }
    );
    transaction.rollback().unwrap();
    assert_eq!(
        kv_get(&connection, b"repo", b"key", 11)
            .unwrap()
            .unwrap()
            .value,
        b"one"
    );

    let replace = KvAtomicRequest {
        scope: b"repo".to_vec(),
        checks: vec![KvCheck {
            key: b"key".to_vec(),
            condition: KvCondition::Version(first_version),
        }],
        mutations: vec![KvMutation::Delete {
            key: b"key".to_vec(),
        }],
    };
    let transaction = connection.transaction().unwrap();
    assert!(matches!(
        kv_atomic(&transaction, 12, &replace).unwrap(),
        KvAtomicOutcome::Applied(_)
    ));
    transaction.commit().unwrap();
    assert!(kv_get(&connection, b"repo", b"key", 12).unwrap().is_none());
}

#[test]
fn ttl_cleanup_and_binary_prefix_pagination_are_bounded() {
    let mut connection = connection();
    let request = KvAtomicRequest {
        scope: b"scope".to_vec(),
        checks: Vec::new(),
        mutations: vec![
            put(&[0x10], b"a", Some(50)),
            put(&[0x10, 0xff], b"b", None),
            put(&[0x11], b"c", None),
        ],
    };
    let transaction = connection.transaction().unwrap();
    kv_atomic(&transaction, 10, &request).unwrap();
    transaction.commit().unwrap();

    let first = kv_list(&connection, b"scope", &[0x10], None, 1, 20).unwrap();
    assert_eq!(first.entries[0].key, vec![0x10]);
    assert_eq!(first.next_after, Some(vec![0x10]));
    let second = kv_list(
        &connection,
        b"scope",
        &[0x10],
        first.next_after.as_deref(),
        1,
        20,
    )
    .unwrap();
    assert_eq!(second.entries[0].key, vec![0x10, 0xff]);
    assert!(second.next_after.is_none());
    assert!(
        kv_get(&connection, b"scope", &[0x10], 50)
            .unwrap()
            .is_none()
    );

    let transaction = connection.transaction().unwrap();
    assert_eq!(kv_cleanup_expired(&transaction, 50).unwrap(), 1);
    transaction.commit().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM kv_entries", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );

    let transaction = connection.transaction().unwrap();
    for key in 1_u8..=20 {
        transaction
            .execute(
                "INSERT INTO kv_entries(scope, key, version, value, expires_at_ms) VALUES (?1, ?2, ?3, ?4, NULL)",
                (
                    b"large".as_slice(),
                    [key].as_slice(),
                    [0; 28].as_slice(),
                    vec![key; 60_000],
                ),
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    let bounded = kv_list(&connection, b"large", &[], None, 1_000, 20).unwrap();
    assert!(bounded.entries.len() < 20);
    assert!(bounded.next_after.is_some());
}

#[test]
fn duplicate_mutation_keys_and_expired_puts_fail_before_writes() {
    let mut connection = connection();
    let duplicate = KvAtomicRequest {
        scope: Vec::new(),
        checks: Vec::new(),
        mutations: vec![put(b"key", b"one", None), put(b"key", b"two", None)],
    };
    let transaction = connection.transaction().unwrap();
    assert!(kv_atomic(&transaction, 10, &duplicate).is_err());
    transaction.rollback().unwrap();

    let expired = KvAtomicRequest {
        scope: Vec::new(),
        checks: Vec::new(),
        mutations: vec![put(b"key", b"one", Some(10))],
    };
    let transaction = connection.transaction().unwrap();
    assert!(kv_atomic(&transaction, 10, &expired).is_err());
    transaction.rollback().unwrap();

    let oversized = KvAtomicRequest {
        scope: Vec::new(),
        checks: Vec::new(),
        mutations: (0_u8..17)
            .map(|key| put(&[key + 1], &vec![key; 65_536], None))
            .collect(),
    };
    let transaction = connection.transaction().unwrap();
    assert!(kv_atomic(&transaction, 10, &oversized).is_err());
    transaction.rollback().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM kv_entries", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn typed_kv_namespace_recovers_after_owner_loss() {
    let registry = kv_registry();
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        KV_NAMESPACE,
        &0_u32.to_be_bytes(),
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([2; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, Path::from("runtime"), [3; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Kv,
                registry.module_code(KV_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let first_session = SessionId::from_bytes([4; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://first.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            observed,
            directory.path().join("first.sqlite"),
            install_kv_schema,
        )
        .await
        .unwrap();
    let namespace = KvNamespace::<TestKv>::new(
        CellClient::local(registry.clone(), handle.clone()),
        target.tenant(),
        target.application(),
        KV_NAMESPACE,
    )
    .unwrap();
    let request = KvAtomicRequest {
        scope: b"repository".to_vec(),
        checks: Vec::new(),
        mutations: vec![put(b"branch", b"main", None)],
    };
    let committed = namespace
        .atomic(current_identity(7), request)
        .await
        .unwrap();
    assert!(matches!(committed.output, KvAtomicOutcome::Applied(_)));
    let entry = namespace
        .get(
            b"repository".to_vec(),
            b"branch".to_vec(),
            Some(committed.receipt),
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(entry.value, b"main");
    assert_eq!(
        namespace
            .list(
                KvListRequest {
                    scope: b"repository".to_vec(),
                    prefix: b"br".to_vec(),
                    after_key: None,
                    limit: 10,
                },
                Some(committed.receipt),
            )
            .await
            .unwrap()
            .output
            .entries,
        vec![entry]
    );
    let rejected = namespace
        .atomic(
            current_identity(8),
            KvAtomicRequest {
                scope: b"repository".to_vec(),
                checks: vec![KvCheck {
                    key: b"branch".to_vec(),
                    condition: KvCondition::Absent,
                }],
                mutations: vec![put(b"branch", b"wrong", None)],
            },
        )
        .await;
    assert!(matches!(
        rejected,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == KvAtomicOutcome::PreconditionFailed {
                key: b"branch".to_vec()
            }
    ));
    drop(namespace);
    drop(handle);
    drop(runtime);
    let stale = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([9; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            replica,
            authority.clone(),
            stale,
            support::fence_session(&layout, first_session, second_session)
                .await
                .direct_takeover()
                .unwrap(),
            crab_cell_runtime::RecoveryManifestStore::new(layout.clone(), Limits::default()),
            directory.path().join("second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_namespace = KvNamespace::<TestKv>::new(
        CellClient::local(registry, restored.clone()),
        target.tenant(),
        target.application(),
        KV_NAMESPACE,
    )
    .unwrap();
    assert_eq!(
        restored_namespace
            .get(
                b"repository".to_vec(),
                b"branch".to_vec(),
                Some(committed.receipt),
            )
            .await
            .unwrap()
            .output
            .unwrap()
            .value,
        b"main"
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
