use std::sync::OnceLock;

use crab_cell_runtime::Error;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::executor::HandlerOutcome;
use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, WireValue};
use crab_cell_runtime::identity::{
    ApplicationId, CellId, CellTarget, Digest, NamespaceId, TenantId,
};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, Command, ModuleDescriptor, NamespaceDescriptor, Query, Registry,
    RegistryBuilder,
};
use crab_cell_runtime::registry::{
    CommandContext, CommandInvocation, CommandResult, MigrationDescriptor, OperationDescriptor,
    QueryContext, QueryInvocation,
};

const MIGRATION: &str = "CREATE TABLE items(value BLOB NOT NULL)";
const COMMAND: OperationDescriptor = OperationDescriptor {
    id: 1,
    codec_version: 1,
    schema_min: 1,
    schema_max: 1,
    input_limit: 16,
    output_limit: 16,
};
const QUERY: OperationDescriptor = OperationDescriptor {
    id: 1,
    codec_version: 1,
    schema_min: 1,
    schema_max: 1,
    input_limit: 1,
    output_limit: 16,
};

struct FirstModule;

impl CellModule for FirstModule {
    const NAME: &'static str = "first";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        first_descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<Insert>()?;
        registry.bind_query::<Read>()?;
        Ok(())
    }
}

struct SecondModule;

impl CellModule for SecondModule {
    const NAME: &'static str = "second";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        second_descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<SecondInsert>()?;
        registry.bind_query::<SecondRead>()?;
        Ok(())
    }
}

struct MissingBinding;

impl CellModule for MissingBinding {
    const NAME: &'static str = "first";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        first_descriptor()
    }

    fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

fn first_descriptor() -> &'static ModuleDescriptor {
    static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| descriptor("first", 1))
}

fn second_descriptor() -> &'static ModuleDescriptor {
    static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
    DESCRIPTOR.get_or_init(|| descriptor("second", 2))
}

fn descriptor(name: &'static str, namespace: u8) -> ModuleDescriptor {
    let migrations = Box::leak(Box::new([MigrationDescriptor {
        version: 1,
        sql: MIGRATION,
        digest: Digest::from_bytes(*blake3::hash(MIGRATION.as_bytes()).as_bytes()),
    }]));
    let namespaces = Box::leak(Box::new([NamespaceDescriptor {
        id: NamespaceId::from_bytes([namespace; 16]),
        name,
        role: CatalogRole::Repository,
        shards: 1,
        effect_targets: &[],
        dead_letter: None,
    }]));
    ModuleDescriptor {
        name,
        source_digest: Digest::from_bytes([namespace; 32]),
        retained_codes: &[],
        schema_min: 1,
        schema_max: 1,
        migrations,
        commands: &[COMMAND],
        queries: &[QUERY],
        workflow_definitions: &[],
        activity_types: &[],
        namespaces,
    }
}

fn build() -> BuildDescriptor {
    BuildDescriptor {
        source_revision: "0123456789abcdef".into(),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    }
}

fn build_registry(reverse: bool) -> Registry {
    let mut builder = RegistryBuilder::new(build());
    if reverse {
        builder.register(SecondModule).unwrap();
        builder.register(FirstModule).unwrap();
    } else {
        builder.register(FirstModule).unwrap();
        builder.register(SecondModule).unwrap();
    }
    builder.finish().unwrap()
}

/// A module whose descriptor a test mutates before registration.
struct DescriptorModule(&'static ModuleDescriptor);

impl CellModule for DescriptorModule {
    const NAME: &'static str = "first";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        self.0
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command::<Insert>()?;
        registry.bind_query::<Read>()?;
        Ok(())
    }
}

fn finish_with(descriptor: &'static ModuleDescriptor) -> crab_cell_runtime::Result<()> {
    let mut builder = RegistryBuilder::new(build());
    builder.register(DescriptorModule(descriptor))?;
    builder.finish().map(|_| ())
}

fn leaked(descriptor: ModuleDescriptor) -> &'static ModuleDescriptor {
    Box::leak(Box::new(descriptor))
}

struct Insert;

impl Command for Insert {
    const MODULE: &'static str = "first";
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        let result = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO items(value) VALUES (?)".into(),
                parameters: vec![SqlValue::Blob(input.clone())],
            }],
        })?;
        if result[0].rows_affected != 1 {
            return Err(Error::Command("test insert did not affect one row"));
        }
        Ok(CommandResult::Success(input))
    }
}

struct Read;

impl Query for Read {
    const MODULE: &'static str = "first";
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = Vec<u8>;

    fn execute(
        context: &mut QueryContext<'_>,
        _input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT value FROM items ORDER BY rowid".into(),
                parameters: Vec::new(),
            }],
        })?;
        match result[0].rows.first().and_then(|row| row.first()) {
            Some(SqlValue::Blob(value)) => Ok(value.clone()),
            _ => Err(Error::Command("test query returned no blob")),
        }
    }
}

struct SecondInsert;

impl Command for SecondInsert {
    const MODULE: &'static str = "second";
    const ID: u32 = Insert::ID;
    const CODEC_VERSION: u32 = Insert::CODEC_VERSION;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        Insert::execute(context, input)
    }
}

struct SecondRead;

impl Query for SecondRead {
    const MODULE: &'static str = "second";
    const ID: u32 = Read::ID;
    const CODEC_VERSION: u32 = Read::CODEC_VERSION;
    type Input = ();
    type Output = Vec<u8>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        Read::execute(context, input)
    }
}

struct Extra;

impl Command for Extra {
    const MODULE: &'static str = "first";
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = ();

    fn execute(
        _context: &mut CommandContext<'_, '_>,
        _input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        Ok(CommandResult::Success(()))
    }
}

fn wire<T: WireValue>(value: &T, limit: u32) -> Vec<u8> {
    let mut encoder = BoundedEncoder::new(limit).unwrap();
    value.encode(&mut encoder).unwrap();
    encoder.finish()
}

fn decode<T: WireValue>(bytes: &[u8], limit: u32) -> T {
    let mut decoder = BoundedDecoder::new(bytes, limit).unwrap();
    let value = T::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    value
}

#[test]
fn compiled_registry_is_canonical_and_executes_only_declared_bindings() {
    let registry = build_registry(false);
    let reversed = build_registry(true);
    assert_eq!(registry.release_bytes(), reversed.release_bytes());
    assert_eq!(registry.release_digest(), reversed.release_digest());
    let first_code = registry.module_code("first").unwrap();
    assert!(registry.supports_cell(
        NamespaceId::from_bytes([1; 16]),
        CatalogRole::Repository,
        first_code,
        1,
    ));
    for (namespace, role, code, schema) in [
        (
            NamespaceId::from_bytes([9; 16]),
            CatalogRole::Repository,
            first_code,
            1,
        ),
        (
            NamespaceId::from_bytes([1; 16]),
            CatalogRole::Kv,
            first_code,
            1,
        ),
        (
            NamespaceId::from_bytes([1; 16]),
            CatalogRole::Repository,
            Digest::from_bytes([8; 32]),
            1,
        ),
        (
            NamespaceId::from_bytes([1; 16]),
            CatalogRole::Repository,
            first_code,
            2,
        ),
    ] {
        assert!(!registry.supports_cell(namespace, role, code, schema));
    }

    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch(MIGRATION).unwrap();
    let transaction = connection.transaction().unwrap();
    let input = wire(&b"value".to_vec(), 16);
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([1; 16]),
        b"registry-test",
    )
    .unwrap();
    let outcome = registry
        .execute_command(
            &transaction,
            CommandInvocation {
                module: "first",
                operation_id: 1,
                codec_version: 1,
                schema: 1,
                target,
                sequence: 1,
                now_ms: 10,
                input: &input,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, HandlerOutcome::Success(ref value) if decode::<Vec<u8>>(value, 16) == b"value")
    );
    transaction.commit().unwrap();
    assert_eq!(
        registry
            .execute_query(
                &connection,
                QueryInvocation {
                    module: "first",
                    operation_id: 1,
                    codec_version: 1,
                    schema: 1,
                    cell: CellId::from_bytes([3; 32]),
                    commit_sequence: 1,
                    now_ms: 10,
                    input: b"",
                },
            )
            .unwrap(),
        wire(&b"value".to_vec(), 16)
    );
    assert!(matches!(
        registry.execute_query(
            &connection,
            QueryInvocation {
                module: "first",
                operation_id: 1,
                codec_version: 1,
                schema: 2,
                cell: CellId::from_bytes([3; 32]),
                commit_sequence: 1,
                now_ms: 10,
                input: b"",
            },
        ),
        Err(Error::Command(
            "registered operation does not support schema"
        ))
    ));
}

#[test]
fn command_execution_rejects_a_module_targeting_another_namespace_owner() {
    let registry = build_registry(false);
    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch(MIGRATION).unwrap();
    let transaction = connection.transaction().unwrap();
    let input = wire(&b"value".to_vec(), 16);
    let target = CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([2; 16]),
        b"registry-test",
    )
    .unwrap();

    assert!(matches!(
        registry.execute_command(
            &transaction,
            CommandInvocation {
                module: "first",
                operation_id: 1,
                codec_version: 1,
                schema: 1,
                target,
                sequence: 1,
                now_ms: 10,
                input: &input,
            },
        ),
        Err(Error::Registry("operation module does not own namespace"))
    ));
    let count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    transaction.rollback().unwrap();
}

#[test]
fn compiled_registry_rejects_descriptor_binding_drift() {
    let mut missing = RegistryBuilder::new(build());
    missing.register(MissingBinding).unwrap();
    assert!(matches!(
        missing.finish(),
        Err(Error::Registry("descriptor and function bindings differ"))
    ));

    let mut extra = RegistryBuilder::new(build());
    extra.register(FirstModule).unwrap();
    extra.bind_command::<Extra>().unwrap();
    assert!(matches!(
        extra.finish(),
        Err(Error::Registry("descriptor and function bindings differ"))
    ));
}

#[test]
fn build_descriptor_bounds_are_enforced() {
    let mut longest = RegistryBuilder::new(BuildDescriptor {
        source_revision: "x".repeat(128),
        cargo_lock_digest: Digest::from_bytes([9; 32]),
    });
    longest.register(FirstModule).unwrap();
    assert!(longest.finish().is_ok());

    for (revision, digest) in [
        (String::new(), [9; 32]),
        ("x".repeat(129), [9; 32]),
        ("0123456789abcdef".into(), [0; 32]),
    ] {
        let mut builder = RegistryBuilder::new(BuildDescriptor {
            source_revision: revision,
            cargo_lock_digest: Digest::from_bytes(digest),
        });
        builder.register(FirstModule).unwrap();
        assert!(matches!(
            builder.finish(),
            Err(Error::Registry("invalid build descriptor"))
        ));
    }
}

#[test]
fn module_descriptor_requires_a_live_schema_range() {
    for descriptor in [
        ModuleDescriptor {
            schema_min: 0,
            ..*first_descriptor()
        },
        ModuleDescriptor {
            schema_max: 0,
            ..*first_descriptor()
        },
        ModuleDescriptor {
            source_digest: Digest::from_bytes([0; 32]),
            ..*first_descriptor()
        },
    ] {
        assert!(matches!(
            finish_with(leaked(descriptor)),
            Err(Error::Registry("invalid module descriptor"))
        ));
    }
}

#[test]
fn migrations_must_be_contiguous_and_digest_bound() {
    let digest = |sql: &str| Digest::from_bytes(*blake3::hash(sql.as_bytes()).as_bytes());
    let migration = |version| MigrationDescriptor {
        version,
        sql: MIGRATION,
        digest: digest(MIGRATION),
    };
    for descriptor in [
        ModuleDescriptor {
            migrations: Box::leak(Box::new([migration(2)])),
            ..*first_descriptor()
        },
        ModuleDescriptor {
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: MIGRATION,
                digest: Digest::from_bytes([7; 32]),
            }])),
            ..*first_descriptor()
        },
        ModuleDescriptor {
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: "",
                digest: digest(""),
            }])),
            ..*first_descriptor()
        },
    ] {
        assert!(matches!(
            finish_with(leaked(descriptor)),
            Err(Error::Registry("invalid migration inventory"))
        ));
    }

    let uncovered = ModuleDescriptor {
        schema_max: 2,
        ..*first_descriptor()
    };
    assert!(matches!(
        finish_with(leaked(uncovered)),
        Err(Error::Registry(
            "migration range does not cover module schema"
        ))
    ));
}

#[test]
fn operation_limits_must_fit_the_wire_bound() {
    const WIRE_BOUND: u32 = 4 * 1024 * 1024 + 64 * 1024;
    for operation in [
        OperationDescriptor {
            input_limit: 0,
            ..COMMAND
        },
        OperationDescriptor {
            output_limit: 0,
            ..COMMAND
        },
        OperationDescriptor {
            input_limit: WIRE_BOUND + 1,
            ..COMMAND
        },
    ] {
        let descriptor = ModuleDescriptor {
            commands: Box::leak(Box::new([operation])),
            ..*first_descriptor()
        };
        assert!(matches!(
            finish_with(leaked(descriptor)),
            Err(Error::Registry("invalid operation inventory"))
        ));
    }
}

#[test]
fn namespace_ids_and_names_are_unique() {
    let namespace = |id: u8, name: &'static str| NamespaceDescriptor {
        id: NamespaceId::from_bytes([id; 16]),
        name,
        role: CatalogRole::Repository,
        shards: 1,
        effect_targets: &[],
        dead_letter: None,
    };
    let duplicate_id = ModuleDescriptor {
        namespaces: Box::leak(Box::new([namespace(1, "first"), namespace(1, "other")])),
        ..*first_descriptor()
    };
    assert!(matches!(
        finish_with(leaked(duplicate_id)),
        Err(Error::Registry("duplicate namespace ID"))
    ));

    let duplicate_name = ModuleDescriptor {
        namespaces: Box::leak(Box::new([namespace(1, "first"), namespace(2, "first")])),
        ..*first_descriptor()
    };
    assert!(matches!(
        finish_with(leaked(duplicate_name)),
        Err(Error::Registry("invalid namespace inventory"))
    ));
}

#[test]
fn namespace_count_is_bounded() {
    let namespaces = (0..129_u8)
        .map(|index| NamespaceDescriptor {
            id: NamespaceId::from_bytes([index; 16]),
            name: Box::leak(format!("namespace-{index}").into_boxed_str()),
            role: CatalogRole::Repository,
            shards: 1,
            effect_targets: &[],
            dead_letter: None,
        })
        .collect::<Vec<_>>();
    let descriptor = ModuleDescriptor {
        namespaces: Box::leak(namespaces.into_boxed_slice()),
        ..*first_descriptor()
    };
    assert!(matches!(
        finish_with(leaked(descriptor)),
        Err(Error::Registry("namespace count exceeds 128"))
    ));
}

#[test]
fn namespace_shards_must_be_a_bounded_power_of_two() {
    for shards in [0, 3, 8192] {
        let descriptor = ModuleDescriptor {
            namespaces: Box::leak(Box::new([NamespaceDescriptor {
                id: NamespaceId::from_bytes([1; 16]),
                name: "first",
                role: CatalogRole::Repository,
                shards,
                effect_targets: &[],
                dead_letter: None,
            }])),
            ..*first_descriptor()
        };
        assert!(matches!(
            finish_with(leaked(descriptor)),
            Err(Error::Registry("invalid namespace inventory"))
        ));
    }
}

#[test]
fn dead_letter_cycles_are_rejected() {
    let first = NamespaceId::from_bytes([11; 16]);
    let second = NamespaceId::from_bytes([12; 16]);
    let namespace = |id, name: &'static str, target| NamespaceDescriptor {
        id,
        name,
        role: CatalogRole::Queue,
        shards: 1,
        effect_targets: Box::leak(Box::new([target])),
        dead_letter: Some(target),
    };
    let descriptor = ModuleDescriptor {
        namespaces: Box::leak(Box::new([
            namespace(first, "first-queue", second),
            namespace(second, "second-queue", first),
        ])),
        ..*first_descriptor()
    };
    assert!(matches!(
        finish_with(leaked(descriptor)),
        Err(Error::Registry("dead-letter cycle"))
    ));
}
