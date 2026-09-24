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
    pub source_revision: String,
    pub cargo_lock_digest: Digest,
}

/// One exact application migration and its independently verified digest.
#[derive(Clone, Copy, Debug)]
pub struct MigrationDescriptor {
    pub version: u32,
    pub sql: &'static str,
    pub digest: Digest,
}

/// One predecessor module code intentionally retained by the current binary.
#[derive(Clone, Copy, Debug)]
pub struct RetainedCodeDescriptor {
    pub code: Digest,
    pub schema_min: u32,
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
    #[must_use]
    pub const fn module(&self) -> &'static str {
        self.module
    }

    #[must_use]
    pub const fn from_code(&self) -> Digest {
        self.from_code
    }

    #[must_use]
    pub const fn to_code(&self) -> Digest {
        self.to_code
    }

    #[must_use]
    pub const fn from_schema(&self) -> u32 {
        self.from_schema
    }

    #[must_use]
    pub const fn to_schema(&self) -> u32 {
        self.to_schema
    }

    #[must_use]
    pub const fn digest(&self) -> Option<Digest> {
        match self.migration {
            Some(migration) => Some(migration.digest),
            None => None,
        }
    }

    #[must_use]
    pub const fn sql(&self) -> Option<&'static str> {
        match self.migration {
            Some(migration) => Some(migration.sql),
            None => None,
        }
    }

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
    pub id: u32,
    pub codec_version: u32,
    pub schema_min: u32,
    pub schema_max: u32,
    pub input_limit: u32,
    pub output_limit: u32,
}

/// One stable Cell namespace compiled into a module.
#[derive(Clone, Copy, Debug)]
pub struct NamespaceDescriptor {
    pub id: NamespaceId,
    pub name: &'static str,
    pub role: CatalogRole,
    pub shards: u32,
    pub effect_targets: &'static [NamespaceId],
    pub dead_letter: Option<NamespaceId>,
}

/// Static module inventory that must match its compiled function bindings.
#[derive(Clone, Copy, Debug)]
pub struct ModuleDescriptor {
    pub name: &'static str,
    pub source_digest: Digest,
    pub retained_codes: &'static [RetainedCodeDescriptor],
    pub schema_min: u32,
    pub schema_max: u32,
    pub migrations: &'static [MigrationDescriptor],
    pub commands: &'static [OperationDescriptor],
    pub queries: &'static [OperationDescriptor],
    pub workflow_definitions: &'static [Digest],
    pub activity_types: &'static [&'static str],
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
