use std::sync::OnceLock;

use super::*;

const CODE_ONLY_MODULE: &str = "code-only-test";
const CODE_ONLY_NAMESPACE: crate::NamespaceId = crate::NamespaceId::from_bytes([45; 16]);
const PREDECESSOR_CODE: Digest = Digest::from_bytes([43; 32]);

struct CodeOnlyModule;

impl crate::CellModule for CodeOnlyModule {
    const NAME: &'static str = CODE_ONLY_MODULE;

    fn descriptor(&self) -> &'static crate::ModuleDescriptor {
        static DESCRIPTOR: OnceLock<crate::ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            let sql = "SELECT 1";
            crate::ModuleDescriptor {
                name: CODE_ONLY_MODULE,
                source_digest: Digest::from_bytes([46; 32]),
                retained_codes: &[crate::registry::RetainedCodeDescriptor {
                    code: PREDECESSOR_CODE,
                    schema_min: 1,
                    schema_max: 1,
                }],
                schema_min: 1,
                schema_max: 1,
                migrations: Box::leak(Box::new([crate::registry::MigrationDescriptor {
                    version: 1,
                    sql,
                    digest: Digest::from_bytes(*blake3::hash(sql.as_bytes()).as_bytes()),
                }])),
                commands: &[],
                queries: &[],
                workflow_definitions: &[],
                activity_types: &[],
                namespaces: &[crate::NamespaceDescriptor {
                    id: CODE_ONLY_NAMESPACE,
                    name: CODE_ONLY_MODULE,
                    role: crate::CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                }],
            }
        })
    }

    fn register(self, _registry: &mut crate::RegistryBuilder) -> Result<()> {
        Ok(())
    }
}

fn code_only_registry() -> crate::Registry {
    let mut registry = crate::RegistryBuilder::new(crate::BuildDescriptor {
        source_revision: CODE_ONLY_MODULE.into(),
        cargo_lock_digest: Digest::from_bytes([47; 32]),
    });
    registry.register(CodeOnlyModule).unwrap();
    registry.finish().unwrap()
}

#[test]
fn full_pending_publication_budget_refuses_new_commands() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let cell = CellId::from_bytes([61; 32]);
    let incarnation = IncarnationId::from_bytes([62; 16]);
    let mut connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    crate::cell::schema::install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
    drop(connection);
    let db = Db::open(&path, crab_ltx::Limits::default()).unwrap();
    let mut executor = CellExecutor::new(db, cell, incarnation, 1);

    // Each command commits locally, becomes pending, and is released by its
    // fleet proof, so object publication may lag behind the actor. The budget
    // that stops that lag from growing without bound is the admission under
    // test.
    for sequence in 1_u64..=MAX_PENDING_PUBLICATIONS as u64 {
        let identity = MutationIdentity {
            request_id: RequestId::from_bytes([sequence as u8; 16]),
            issued_at_ms: 10,
            expires_at_ms: 10_000,
        };
        let execution = executor
            .execute(
                identity,
                Digest::from_bytes([sequence as u8; 32]),
                20,
                1 << 20,
                |transaction| {
                    transaction.execute(
                        "UPDATE sys_meta SET logical_time_ms = logical_time_ms + 1",
                        [],
                    )?;
                    Ok(HandlerOutcome::Success(vec![sequence as u8]))
                },
            )
            .unwrap();
        assert!(
            matches!(execution, CommandExecution::Pending),
            "sequence {sequence}"
        );
        executor.confirm_durable(sequence).unwrap();
    }

    // The budget is full: the next command is refused instead of queueing more
    // unpublished work behind a provider that is already behind.
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([201; 16]),
        issued_at_ms: 10,
        expires_at_ms: 10_000,
    };
    assert!(matches!(
        executor.execute(identity, Digest::from_bytes([9; 32]), 20, 1 << 20, |_| {
            Ok(HandlerOutcome::Success(Vec::new()))
        }),
        Err(Error::PendingPublication)
    ));
}

#[test]
fn restored_executor_rejects_root_sequence_ahead_of_sqlite_metadata() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let cell = CellId::from_bytes([31; 32]);
    let incarnation = IncarnationId::from_bytes([32; 16]);
    let mut connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    crate::cell::schema::install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
    drop(connection);
    let mut db = Db::open(&path, crab_ltx::Limits::default()).unwrap();
    db.transaction(|transaction| {
        transaction.execute("UPDATE sys_meta SET logical_time_ms = 1", [])?;
        Ok(())
    })
    .unwrap();
    db.capture().unwrap();
    let root = crab_ltx::RootRef {
        cell: *cell.as_bytes(),
        incarnation: *incarnation.as_bytes(),
        digest: [33; 32],
        position: db.position(),
        commit_sequence: 1,
    };
    assert!(matches!(
        CellExecutor::from_restored(db, cell, incarnation, 1, root),
        Err(Error::Control(
            "restored SQLite metadata does not match authoritative root"
        ))
    ));
}

#[test]
fn code_only_migration_commits_a_captured_system_cut() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("cell.sqlite");
    let cell = CellId::from_bytes([41; 32]);
    let incarnation = IncarnationId::from_bytes([42; 16]);
    let mut connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
    crate::cell::schema::install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
    drop(connection);
    let db = Db::open(&path, crab_ltx::Limits::default()).unwrap();
    let mut executor = CellExecutor::new(db, cell, incarnation, 1);
    let registry = code_only_registry();
    let target_code = registry.module_code(CODE_ONLY_MODULE).unwrap();
    let plan = registry
        .next_migration(CODE_ONLY_NAMESPACE, PREDECESSOR_CODE, 1)
        .unwrap()
        .unwrap();
    executor.migrate(plan, 10).unwrap();
    let pending = executor.pending_migration().unwrap();
    assert_eq!(pending.code(), target_code);
    assert_eq!(pending.from_schema(), 1);
    assert_eq!(pending.to_schema(), 1);
    assert_eq!(pending.digest(), None);
    assert_eq!(pending.commit_sequence(), 1);
    assert!(!pending.cuts().segments.is_empty());
}

#[test]
fn declared_admission_limits_become_capacity_errors() {
    let disk = admission_error(crab_ltx::CrabError::Limit(
        crab_ltx::LimitKind::LocalDiskBytes,
    ));
    assert!(matches!(disk, Error::Capacity("local disk bytes")));

    let database = crab_ltx::CrabError::Limit(crab_ltx::LimitKind::DatabaseBytes);
    assert!(matches!(
        admission_error(database),
        Error::Capacity("database bytes")
    ));

    // A fenced session is not a capacity refusal: the caller must restore
    // authoritative state instead of retrying the same request.
    let fenced = admission_error(crab_ltx::CrabError::Fenced);
    assert!(matches!(fenced, Error::Ltx(crab_ltx::CrabError::Fenced)));
}
