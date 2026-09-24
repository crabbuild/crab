//! Compiles one application the way an author does.
//!
//! Run with `cargo run -p crab-cell-app --example authoring`. A host takes the
//! finished `CompiledApplication`; building it here keeps the documented
//! authoring flow compiling against the published contract.

use std::sync::{Arc, OnceLock};

use crab_cell_app::{ApplicationBuilder, CellType, CompiledApplication};
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::identity::{Digest, NamespaceId};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    RegistryBuilder,
};

/// The module an application registers before a host can serve it.
struct Repository;

/// The namespace this module owns. A descriptor borrows its lists for the
/// process, so they live in `static` items rather than in the initializer.
static NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: NamespaceId::from_bytes([2; 16]),
    name: "repository",
    role: CatalogRole::Sql,
    shards: 1,
    effect_targets: &[],
    dead_letter: None,
}];

/// The single statement that installs the module's schema.
const MIGRATION_SQL: &str = "-- repository migration v1";

impl CellModule for Repository {
    const NAME: &'static str = "repository";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: "repository",
            source_digest: Digest::from_bytes([1; 32]),
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            // A module must install its own schema, and the registry pins each
            // migration to the exact statement: the digest is its BLAKE3 hash.
            migrations: Box::leak(
                vec![MigrationDescriptor {
                    version: 1,
                    sql: MIGRATION_SQL,
                    digest: Digest::from_bytes(*blake3::hash(MIGRATION_SQL.as_bytes()).as_bytes()),
                }]
                .into_boxed_slice(),
            ),
            commands: &[],
            queries: &[],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &NAMESPACES,
        })
    }

    /// Native bindings for this module's commands and queries live here. This
    /// example registers none, so the module declares its namespace only.
    fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        Ok(())
    }
}

fn main() -> crab_cell_runtime::Result<()> {
    let mut builder = ApplicationBuilder::new(
        "repository",
        BuildDescriptor {
            source_revision: "authoring-example".into(),
            cargo_lock_digest: Digest::from_bytes([3; 32]),
        },
    )?;
    builder.register(Repository)?;
    builder.cell_type(CellType::new(
        "repository",
        "repository",
        NamespaceId::from_bytes([2; 16]),
        CatalogRole::Sql,
        1,
    )?)?;

    let application: Arc<CompiledApplication> = Arc::new(builder.finish()?);
    println!(
        "compiled {} with {} cell type(s), digest {:?}",
        application.name(),
        application.cell_types().len(),
        application.descriptor_digest()
    );
    Ok(())
}
