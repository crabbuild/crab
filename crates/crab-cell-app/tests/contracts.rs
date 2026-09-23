use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::identity::{Digest, NamespaceId};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, ModuleDescriptor, NamespaceDescriptor, Registry, RegistryBuilder,
};
use crab_cell_runtime::registry::{MigrationDescriptor, OperationDescriptor};

const MIGRATION_SQL: &str = "-- contract migration v1";
const WORKFLOW_DEFINITION: Digest = Digest::from_bytes([41; 32]);

fn build() -> BuildDescriptor {
    BuildDescriptor {
        source_revision: "contract-validation".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    }
}

fn migration(version: u32, sql: &'static str, digest: Digest) -> &'static [MigrationDescriptor] {
    Box::leak(Box::new([MigrationDescriptor {
        version,
        sql,
        digest,
    }]))
}

fn namespace(
    namespace: u8,
    role: CatalogRole,
    shards: u32,
    effect_targets: &'static [NamespaceId],
    dead_letter: Option<NamespaceId>,
) -> &'static [NamespaceDescriptor] {
    Box::leak(Box::new([NamespaceDescriptor {
        id: NamespaceId::from_bytes([namespace; 16]),
        name: "contract-namespace",
        role,
        shards,
        effect_targets,
        dead_letter,
    }]))
}

fn one_namespace_id(namespace: u8) -> &'static [NamespaceId] {
    Box::leak(Box::new([NamespaceId::from_bytes([namespace; 16])]))
}

fn descriptor(
    name: &'static str,
    namespace: &'static [NamespaceDescriptor],
    migrations: &'static [MigrationDescriptor],
    commands: &'static [OperationDescriptor],
    workflow_definitions: &'static [Digest],
    activity_types: &'static [&'static str],
) -> &'static ModuleDescriptor {
    Box::leak(Box::new(ModuleDescriptor {
        name,
        source_digest: Digest::from_bytes([7; 32]),
        retained_codes: &[],
        schema_min: 1,
        schema_max: 1,
        migrations,
        commands,
        queries: &[],
        workflow_definitions,
        activity_types,
        namespaces: namespace,
    }))
}

macro_rules! module {
    ($type:ident, $name:literal, $descriptor:ident) => {
        struct $type;

        impl CellModule for $type {
            const NAME: &'static str = $name;

            fn descriptor(&self) -> &'static ModuleDescriptor {
                $descriptor()
            }

            fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
                Ok(())
            }
        }
    };
}

fn valid_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "valid",
        namespace(1, CatalogRole::Repository, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(ValidModule, "valid", valid_descriptor);

fn finishes_with_error<M: CellModule>(module: M) -> bool {
    let mut builder = RegistryBuilder::new(build());
    builder.register(module).is_ok() && builder.finish().is_err()
}

fn invalid_migration_digest_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "invalid-migration-digest",
        namespace(2, CatalogRole::Repository, 1, &[], None),
        migration(1, MIGRATION_SQL, Digest::from_bytes([8; 32])),
        &[],
        &[],
        &[],
    )
}

module!(
    InvalidMigrationDigest,
    "invalid-migration-digest",
    invalid_migration_digest_descriptor
);

fn invalid_migration_order_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "invalid-migration-order",
        namespace(3, CatalogRole::Repository, 1, &[], None),
        migration(
            2,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(
    InvalidMigrationOrder,
    "invalid-migration-order",
    invalid_migration_order_descriptor
);

fn invalid_effect_target_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "invalid-effect-target",
        namespace(4, CatalogRole::Repository, 1, one_namespace_id(99), None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(
    InvalidEffectTarget,
    "invalid-effect-target",
    invalid_effect_target_descriptor
);

fn invalid_dead_letter_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "invalid-dead-letter",
        namespace(
            5,
            CatalogRole::Repository,
            1,
            &[],
            Some(NamespaceId::from_bytes([5; 16])),
        ),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(
    InvalidDeadLetter,
    "invalid-dead-letter",
    invalid_dead_letter_descriptor
);

fn missing_workflow_binding_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "missing-workflow-binding",
        namespace(6, CatalogRole::Workflow, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[WORKFLOW_DEFINITION],
        &[],
    )
}

module!(
    MissingWorkflowBinding,
    "missing-workflow-binding",
    missing_workflow_binding_descriptor
);

fn activity_without_workflow_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "activity-without-workflow",
        namespace(7, CatalogRole::Workflow, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &["contract-activity"],
    )
}

module!(
    ActivityWithoutWorkflow,
    "activity-without-workflow",
    activity_without_workflow_descriptor
);

fn invalid_shards_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "invalid-shards",
        namespace(8, CatalogRole::Repository, 3, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(InvalidShards, "invalid-shards", invalid_shards_descriptor);

fn invalid_operation_limits_descriptor() -> &'static ModuleDescriptor {
    const COMMAND: OperationDescriptor = OperationDescriptor {
        id: 1,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: 0,
        output_limit: 1,
    };
    descriptor(
        "invalid-operation-limits",
        namespace(9, CatalogRole::Repository, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[COMMAND],
        &[],
        &[],
    )
}

module!(
    InvalidOperationLimits,
    "invalid-operation-limits",
    invalid_operation_limits_descriptor
);

fn duplicate_a_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "duplicate-module",
        namespace(10, CatalogRole::Repository, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

fn duplicate_b_descriptor() -> &'static ModuleDescriptor {
    descriptor(
        "duplicate-module",
        namespace(11, CatalogRole::Repository, 1, &[], None),
        migration(
            1,
            MIGRATION_SQL,
            Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
        ),
        &[],
        &[],
        &[],
    )
}

module!(DuplicateModuleA, "duplicate-module", duplicate_a_descriptor);
module!(DuplicateModuleB, "duplicate-module", duplicate_b_descriptor);

type ContractCase = (&'static str, fn() -> bool);

#[test]
fn descriptor_contract_rejects_each_declared_relationship_and_limit() {
    let cases: [ContractCase; 8] = [
        ("migration digest", || {
            finishes_with_error(InvalidMigrationDigest)
        }),
        ("migration ordering", || {
            finishes_with_error(InvalidMigrationOrder)
        }),
        ("effect target", || finishes_with_error(InvalidEffectTarget)),
        ("dead-letter target", || {
            finishes_with_error(InvalidDeadLetter)
        }),
        ("workflow binding", || {
            finishes_with_error(MissingWorkflowBinding)
        }),
        ("activity inventory", || {
            finishes_with_error(ActivityWithoutWorkflow)
        }),
        ("namespace shards", || finishes_with_error(InvalidShards)),
        ("operation limits", || {
            finishes_with_error(InvalidOperationLimits)
        }),
    ];
    for (name, rejects) in cases {
        assert!(rejects(), "invalid {name} descriptor was accepted");
    }
}

#[test]
fn descriptor_contract_rejects_duplicate_module_names() {
    let mut builder = RegistryBuilder::new(build());
    builder.register(DuplicateModuleA).unwrap();
    builder.register(DuplicateModuleB).unwrap();
    assert!(builder.finish().is_err());
}

#[test]
fn valid_descriptor_still_compiles_after_contract_fixtures() {
    let mut builder = RegistryBuilder::new(build());
    builder.register(ValidModule).unwrap();
    let registry: Registry = builder.finish().unwrap();
    assert_eq!(registry.namespace_count(), 1);
}
