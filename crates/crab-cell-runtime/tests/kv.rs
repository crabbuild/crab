use std::sync::Arc;

use crab_cell_runtime::{
    ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellCatalog, CellRuntime, CellTarget,
    Digest, HandlerOutcome, IncarnationId, KvAtomicOutcome, KvAtomicRequest, KvCheck, KvCondition,
    KvMutation, MutationIdentity, NamespaceId, Owner, RequestId, SessionId, SqlWorkerPool,
    TenantId, install_kv_schema, install_runtime_schema, kv_atomic, kv_cleanup_expired, kv_get,
    kv_list,
};
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

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
async fn kv_command_publishes_and_survives_idle_owner_restore() {
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([3; 16]),
        NamespaceId::from_bytes([6; 16]),
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
            CatalogEntry::new(&target, CatalogRole::Kv, Digest::from_bytes([5; 32]), 1).unwrap(),
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
    let request = KvAtomicRequest {
        scope: b"repository".to_vec(),
        checks: Vec::new(),
        mutations: vec![put(b"branch", b"main", None)],
    };
    handle
        .execute(
            MutationIdentity {
                request_id: RequestId::from_bytes([7; 16]),
                issued_at_ms: 10,
                expires_at_ms: 10_000,
            },
            Digest::from_bytes([8; 32]),
            20,
            128,
            128,
            move |transaction| match kv_atomic(transaction, 20, &request)? {
                KvAtomicOutcome::Applied(_) => Ok(HandlerOutcome::Success(b"applied".to_vec())),
                KvAtomicOutcome::PreconditionFailed { .. } => {
                    Ok(HandlerOutcome::Rejected(b"precondition".to_vec()))
                }
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();

    let idle = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([9; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            replica,
            authority,
            idle,
            directory.path().join("second.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://second.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                Ok(kv_get(connection, b"repository", b"branch", 21)?
                    .ok_or(crab_cell_runtime::Error::Command(
                        "missing restored KV entry",
                    ))?
                    .value)
            })
            .await
            .unwrap(),
        b"main"
    );
    restored.drain().await.unwrap();
}
