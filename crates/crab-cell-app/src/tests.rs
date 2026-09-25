use super::*;
use crab_cell_runtime::identity::Digest;
use crab_cell_runtime::registry::{CellModule, ModuleDescriptor, NamespaceDescriptor};

struct SqlModule;

impl CellModule for SqlModule {
    const NAME: &'static str = "app-sql";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: ModuleDescriptor = ModuleDescriptor {
            name: "app-sql",
            source_digest: Digest::from_bytes([1; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: &[crab_cell_runtime::MigrationDescriptor {
                version: 1,
                sql: "-- app-sql migration v1",
                digest: Digest::from_bytes([
                    0x43, 0x1d, 0x99, 0x74, 0x5c, 0x8f, 0x39, 0x00, 0x9d, 0x2e, 0xe2, 0x00, 0x7c,
                    0x75, 0x42, 0xe3, 0xa5, 0xf0, 0x0c, 0x92, 0x21, 0x8b, 0xf7, 0x3b, 0x8f, 0x6f,
                    0x6b, 0xc3, 0x38, 0xe4, 0xf5, 0xca,
                ]),
            }],
            commands: &[],
            queries: &[],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[
                NamespaceDescriptor {
                    id: NamespaceId::from_bytes([2; 16]),
                    name: "app-sql",
                    role: CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                },
                NamespaceDescriptor {
                    id: NamespaceId::from_bytes([3; 16]),
                    name: "app-sql-2",
                    role: CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                },
            ],
        };
        &DESCRIPTOR
    }

    fn register(self, _registry: &mut RegistryBuilder) -> Result<()> {
        Ok(())
    }
}

fn build(reverse: bool) -> CompiledApplication {
    let mut builder = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    builder.register(SqlModule).unwrap();
    let first = CellType::new(
        "app-sql",
        "orders",
        NamespaceId::from_bytes([2; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    let second = CellType::new(
        "app-sql",
        "inventory",
        NamespaceId::from_bytes([3; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    if reverse {
        builder.cell_type(second).unwrap();
        builder.cell_type(first).unwrap();
    } else {
        builder.cell_type(first).unwrap();
        builder.cell_type(second).unwrap();
    }
    builder.finish().unwrap()
}

#[test]
fn descriptor_is_stable_and_scope_is_checked() {
    let first = build(false);
    let second = build(true);
    assert_eq!(first.descriptor_bytes(), second.descriptor_bytes());
    assert_eq!(first.descriptor_digest(), second.descriptor_digest());
    assert_eq!(
        first.registry().release_digest(),
        first.registry().release_digest()
    );
}

#[test]
fn compiled_application_rejects_cross_module_invocation_targets() {
    let application = build(false);
    let sql_namespace = NamespaceId::from_bytes([2; 16]);

    assert!(
        application
            .validate_module(sql_namespace, "app-sql")
            .is_ok()
    );
    assert!(
        application
            .validate_module(sql_namespace, "unrelated-module")
            .is_err()
    );
    assert!(
        application
            .validate_module(NamespaceId::from_bytes([99; 16]), "app-sql")
            .is_err()
    );
}

#[test]
fn duplicate_cell_namespace_and_mismatched_module_fail_closed() {
    let mut builder = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    let cell_type = CellType::new(
        "app-sql",
        "orders",
        NamespaceId::from_bytes([2; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    builder.cell_type(cell_type).unwrap();
    assert!(builder.cell_type(cell_type).is_err());
    builder.register(SqlModule).unwrap();
    let mut wrong = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    wrong
        .cell_type(
            CellType::new(
                "other",
                "orders",
                NamespaceId::from_bytes([2; 16]),
                CatalogRole::Sql,
                1,
            )
            .unwrap(),
        )
        .unwrap();
    wrong.register(SqlModule).unwrap();
    assert!(wrong.finish().is_err());
}

#[test]
fn every_registered_namespace_requires_a_cell_type() {
    let mut builder = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    builder.register(SqlModule).unwrap();
    builder
        .cell_type(
            CellType::new(
                "app-sql",
                "orders",
                NamespaceId::from_bytes([2; 16]),
                CatalogRole::Sql,
                1,
            )
            .unwrap(),
        )
        .unwrap();

    assert!(builder.finish().is_err());
}

#[test]
fn cell_type_schema_range_must_match_compiled_module() {
    let mut builder = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    builder.register(SqlModule).unwrap();
    builder
        .cell_type(
            CellType::new(
                "app-sql",
                "orders",
                NamespaceId::from_bytes([2; 16]),
                CatalogRole::Sql,
                1,
            )
            .unwrap()
            .with_schema_range(1, 2)
            .unwrap(),
        )
        .unwrap();

    assert!(builder.finish().is_err());
}

#[test]
fn duplicate_cell_type_name_fails_closed() {
    let mut builder = ApplicationBuilder::new(
        "app",
        BuildDescriptor {
            source_revision: "source".into(),
            cargo_lock_digest: Digest::from_bytes([9; 32]),
        },
    )
    .unwrap();
    let first = CellType::new(
        "app-sql",
        "orders",
        NamespaceId::from_bytes([2; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    let second = CellType::new(
        "app-sql",
        "orders",
        NamespaceId::from_bytes([3; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    builder.cell_type(first).unwrap();
    assert!(builder.cell_type(second).is_err());
}

#[test]
fn cell_type_limits_and_partition_bounds_fail_closed() {
    for shards in [0, 3, 8_192] {
        assert!(
            CellType::new(
                "app-sql",
                "orders",
                NamespaceId::from_bytes([2; 16]),
                CatalogRole::Sql,
                shards,
            )
            .is_err()
        );
    }
    assert!(
        CellType::new(
            "app-sql",
            "orders",
            NamespaceId::from_bytes([0; 16]),
            CatalogRole::Sql,
            1,
        )
        .is_err()
    );

    let cell_type = CellType::new(
        "app-sql",
        "orders",
        NamespaceId::from_bytes([2; 16]),
        CatalogRole::Sql,
        1,
    )
    .unwrap();
    assert!(cell_type.with_limits(0, 1).is_err());
    assert!(cell_type.with_limits(1, 0).is_err());
    assert!(cell_type.with_limits(511, 128).is_err());
    assert!(cell_type.with_limits(512, 127).is_err());
    assert!(cell_type.with_schema_range(0, 1).is_err());
    assert!(cell_type.with_schema_range(2, 1).is_err());
}
