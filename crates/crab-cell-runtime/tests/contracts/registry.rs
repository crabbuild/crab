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
