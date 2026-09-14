use std::sync::OnceLock;

use crab_cell_runtime::{
    BuildDescriptor, CatalogRole, CellId, CellModule, CommandContext, CommandInvocation, Digest,
    Error, HandlerOutcome, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor, NamespaceId,
    OperationDescriptor, QueryContext, QueryInvocation, Registry, RegistryBuilder, SqlBatch,
    SqlStatement, SqlValue,
};

const MIGRATION: &str = "CREATE TABLE items(value BLOB NOT NULL)";
const COMMAND: OperationDescriptor = OperationDescriptor {
    id: 1,
    codec_version: 1,
    schema_min: 1,
    schema_max: 1,
    input_limit: 8,
    output_limit: 8,
};
const QUERY: OperationDescriptor = OperationDescriptor {
    id: 1,
    codec_version: 1,
    schema_min: 1,
    schema_max: 1,
    input_limit: 1,
    output_limit: 8,
};

struct FirstModule;

impl CellModule for FirstModule {
    const NAME: &'static str = "first";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        first_descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_command(Self::NAME, 1, 1, insert)?;
        registry.bind_query(Self::NAME, 1, 1, read)?;
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
        registry.bind_command(Self::NAME, 1, 1, insert)?;
        registry.bind_query(Self::NAME, 1, 1, read)?;
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

fn insert(
    context: &mut CommandContext<'_, '_>,
    input: &[u8],
) -> crab_cell_runtime::Result<HandlerOutcome> {
    let result = context.sql(&SqlBatch {
        statements: vec![SqlStatement {
            sql: "INSERT INTO items(value) VALUES (?)".into(),
            parameters: vec![SqlValue::Blob(input.to_vec())],
        }],
    })?;
    if result[0].rows_affected != 1 {
        return Err(Error::Command("test insert did not affect one row"));
    }
    Ok(HandlerOutcome::Success(input.to_vec()))
}

fn read(context: &mut QueryContext<'_>, _input: &[u8]) -> crab_cell_runtime::Result<Vec<u8>> {
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

#[test]
fn compiled_registry_is_canonical_and_executes_only_declared_bindings() {
    let registry = build_registry(false);
    let reversed = build_registry(true);
    assert_eq!(registry.release_bytes(), reversed.release_bytes());
    assert_eq!(registry.release_digest(), reversed.release_digest());
    assert!(registry.module_code("first").is_some());

    let mut connection = crab_ltx::rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch(MIGRATION).unwrap();
    let transaction = connection.transaction().unwrap();
    let outcome = registry
        .execute_command(
            &transaction,
            CommandInvocation {
                module: "first",
                operation_id: 1,
                codec_version: 1,
                schema: 1,
                cell: CellId::from_bytes([3; 32]),
                sequence: 1,
                now_ms: 10,
                input: b"value",
            },
        )
        .unwrap();
    assert!(matches!(outcome, HandlerOutcome::Success(ref value) if value == b"value"));
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
                    input: b"",
                },
            )
            .unwrap(),
        b"value"
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
                input: b"",
            },
        ),
        Err(Error::Command(
            "registered operation does not support schema"
        ))
    ));
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
    extra.bind_command("first", 2, 1, insert).unwrap();
    assert!(matches!(
        extra.finish(),
        Err(Error::Registry("descriptor and function bindings differ"))
    ));
}
