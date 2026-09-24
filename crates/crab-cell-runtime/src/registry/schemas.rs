//! Module, migration, and operation descriptors with their validation rules.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::*;
use crate::registry::builder::{BindingKey, CronBinding, PrimitiveBinding, QueueBinding};
use crate::registry::handlers::{
    ActivityFuture, ActivityRunFuture, EffectFuture, MaintenanceFuture,
};

pub(super) const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;
pub(super) const MAX_MODULES: usize = 128;
pub(super) const MAX_NAMESPACES: usize = 128;
pub(super) const MAX_MIGRATION_BYTES: usize = 1024 * 1024;
pub(super) const MAX_OPERATION_BYTES: u32 = crate::codec::MAX_WIRE_BYTES as u32;
pub(super) const CODE_ONLY_MIGRATION_BYTES: usize = 2 * 32;

/// Registry construction error returned before server readiness.
pub type RegistryError = Error;

/// Build evidence included in the canonical release descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildDescriptor {
    /// Source revision the image was built from.
    pub source_revision: String,
    /// Digest of the Cargo lockfile the image was built with.
    pub cargo_lock_digest: Digest,
}

/// One exact application migration and its independently verified digest.
#[derive(Clone, Copy, Debug)]
pub struct MigrationDescriptor {
    /// Schema version this migration installs.
    pub version: u32,
    /// Migration SQL, digest-bound to `digest`.
    pub sql: &'static str,
    /// BLAKE3 digest of `sql`.
    pub digest: Digest,
}

/// One predecessor module code intentionally retained by the current binary.
#[derive(Clone, Copy, Debug)]
pub struct RetainedCodeDescriptor {
    /// Code digest the module retains.
    pub code: Digest,
    /// Oldest schema version the retained code serves.
    pub schema_min: u32,
    /// Newest schema version the retained code serves.
    pub schema_max: u32,
}

/// One registry-verified schema or code migration for an active Cell.
#[derive(Clone, Copy, Debug)]
pub struct MigrationPlan {
    pub(super) module: &'static str,
    pub(super) from_code: Digest,
    pub(super) to_code: Digest,
    pub(super) from_schema: u32,
    pub(super) to_schema: u32,
    pub(super) migration: Option<MigrationDescriptor>,
}

impl MigrationPlan {
    /// Returns the module the plan migrates.
    #[must_use]
    pub const fn module(&self) -> &'static str {
        self.module
    }

    /// Returns the code digest the Cell migrates from.
    #[must_use]
    pub const fn from_code(&self) -> Digest {
        self.from_code
    }

    /// Returns the code digest the Cell migrates to.
    #[must_use]
    pub const fn to_code(&self) -> Digest {
        self.to_code
    }

    /// Returns the schema version the Cell migrates from.
    #[must_use]
    pub const fn from_schema(&self) -> u32 {
        self.from_schema
    }

    /// Returns the schema version the Cell migrates to.
    #[must_use]
    pub const fn to_schema(&self) -> u32 {
        self.to_schema
    }

    /// Returns the plan digest, when the plan declares one.
    #[must_use]
    pub const fn digest(&self) -> Option<Digest> {
        match self.migration {
            Some(migration) => Some(migration.digest),
            None => None,
        }
    }

    /// Returns the migration SQL, when the plan carries any.
    #[must_use]
    pub const fn sql(&self) -> Option<&'static str> {
        match self.migration {
            Some(migration) => Some(migration.sql),
            None => None,
        }
    }

    /// Returns the plan's operation byte budget.
    #[must_use]
    pub const fn operation_bytes(&self) -> usize {
        match self.migration {
            Some(migration) => migration.sql.len(),
            None => CODE_ONLY_MIGRATION_BYTES,
        }
    }
}

/// One registered command or query codec and its schema compatibility range.
#[derive(Clone, Copy, Debug)]
pub struct OperationDescriptor {
    /// Command or query id within its module.
    pub id: u32,
    /// Input codec version the operation accepts.
    pub codec_version: u32,
    /// Oldest schema version the operation serves.
    pub schema_min: u32,
    /// Newest schema version the operation serves.
    pub schema_max: u32,
    /// Largest input the operation accepts, in bytes.
    pub input_limit: u32,
    /// Largest output the operation returns, in bytes.
    pub output_limit: u32,
}

/// One stable Cell namespace compiled into a module.
#[derive(Clone, Copy, Debug)]
pub struct NamespaceDescriptor {
    /// Namespace identity.
    pub id: NamespaceId,
    /// Stable namespace name.
    pub name: &'static str,
    /// Catalog role the namespace serves.
    pub role: CatalogRole,
    /// Power-of-two shard count.
    pub shards: u32,
    /// Namespaces this one may emit effects to.
    pub effect_targets: &'static [NamespaceId],
    /// Queue namespace that receives this namespace's dead letters.
    pub dead_letter: Option<NamespaceId>,
}

/// Static module inventory that must match its compiled function bindings.
#[derive(Clone, Copy, Debug)]
pub struct ModuleDescriptor {
    /// Module name, matched against the compiled function bindings.
    pub name: &'static str,
    /// Digest of the module's source.
    pub source_digest: Digest,
    /// Code digests the module retains for older schemas.
    pub retained_codes: &'static [RetainedCodeDescriptor],
    /// Oldest schema version the module serves.
    pub schema_min: u32,
    /// Newest schema version the module serves.
    pub schema_max: u32,
    /// Contiguous migrations covering `schema_min` through `schema_max`.
    pub migrations: &'static [MigrationDescriptor],
    /// Commands the module declares.
    pub commands: &'static [OperationDescriptor],
    /// Queries the module declares.
    pub queries: &'static [OperationDescriptor],
    /// Workflow definition digests the module serves.
    pub workflow_definitions: &'static [Digest],
    /// Activity types the module registers.
    pub activity_types: &'static [&'static str],
    /// Namespaces the module owns.
    pub namespaces: &'static [NamespaceDescriptor],
}

mod binding;
mod validation;

pub(in crate::registry) use binding::*;
pub(in crate::registry) use validation::*;

pub(super) fn operation_descriptors(
    modules: &[&ModuleDescriptor],
    select: impl Fn(&ModuleDescriptor) -> &[OperationDescriptor],
) -> BTreeMap<BindingKey, OperationDescriptor> {
    modules
        .iter()
        .flat_map(|module| {
            select(module).iter().map(|operation| {
                (
                    BindingKey {
                        module: module.name.to_owned(),
                        id: operation.id,
                        codec_version: operation.codec_version,
                    },
                    *operation,
                )
            })
        })
        .collect()
}

pub(super) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
