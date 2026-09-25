use std::{future::Future, pin::Pin, sync::Arc, sync::OnceLock, time::UNIX_EPOCH};

use crab_cell_app::{ApplicationBuilder, ApplicationHandle, CellApplication, CellType};
use crab_cell_runtime::cell::actor::CellHandle;
use crab_cell_runtime::cell::actor::CellRuntime;
use crab_cell_runtime::cell::catalog::CatalogEntry;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::cell::worker::SqlWorkerPool;
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, TenantId, partition_for_shard,
};
use crab_cell_runtime::identity::{IncarnationId, NodeId, RequestId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::{
    FencedNodeSession, NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain,
};
use crab_cell_runtime::primitives::blob::{BlobArtifactStore, BlobModule};
use crab_cell_runtime::primitives::blob::{
    BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery, BlobQueryResult,
    install_blob_schema, register_blob,
};
use crab_cell_runtime::primitives::cron::CronModule;
use crab_cell_runtime::primitives::cron::{
    CronInvocation, CronMutation, CronQueryResult, CronTarget, install_cron_schema, register_cron,
};
use crab_cell_runtime::primitives::effects::EffectModule;
use crab_cell_runtime::primitives::effects::{
    EffectClaimRequest, EffectLeaseOutcome, register_effect_delivery,
};
use crab_cell_runtime::primitives::kv::KvModule;
use crab_cell_runtime::primitives::kv::{
    KvAtomicCommand, KvAtomicRequest, KvGetQuery, KvGetRequest, KvMutation, install_kv_schema,
    register_kv,
};
use crab_cell_runtime::primitives::maintenance::{MaintenanceModule, register_maintenance};
use crab_cell_runtime::primitives::queue::QueueModule;
use crab_cell_runtime::primitives::queue::{
    QueueClaimRequest, QueueDeadLetterTarget, QueueLeaseOutcome, QueueSendRequest,
    install_queue_schema, register_queue,
};
use crab_cell_runtime::primitives::sql::SqlModule;
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue, register_sql};
use crab_cell_runtime::primitives::workflow::{
    ActivityContext, ActivityExecution, ActivityHandler, ActivityRunOutcome, WorkflowAction,
    WorkflowContext, WorkflowDecision, WorkflowDefinition, WorkflowStatus, install_workflow_schema,
    register_activity, register_workflow, register_workflow_activities,
};
use crab_cell_runtime::primitives::workflow::{WorkflowActivityModule, WorkflowModule};
use crab_cell_runtime::qualification::{
    QualificationExecution, QualificationOperation, QualificationOperationExecutor,
    QualificationProfile, QualificationWorkload,
};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, Command, ModuleDescriptor, NamespaceDescriptor, Registry,
    RegistryBuilder,
};
use crab_cell_runtime::registry::{CommandContext, CommandResult, OperationDescriptor};
use crab_cell_runtime::{Error, Result};
use crab_ltx::{CellReplica, DiskBudget, Host, Limits};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::memory::InMemory;

mod reference_application {
    pub mod application;
    pub mod commit;
    pub mod fleet;
    pub mod harness;
    pub mod performance;
    pub mod performance_fixture;
    pub mod primitives;
    pub mod process_performance;
    pub mod public_host;
}

pub(crate) use reference_application::application::*;
pub(crate) use reference_application::harness::*;

#[test]
fn application_descriptor_is_stable_when_modules_register_in_reverse_order() {
    let forward = compile_reference_in_order(false);
    let reverse = compile_reference_in_order(true);
    assert_eq!(forward.descriptor_bytes(), reverse.descriptor_bytes());
    assert_eq!(forward.descriptor_digest(), reverse.descriptor_digest());
}

#[test]
fn generated_client_release_descriptor_matches_independent_stable_ids() {
    fn push_name(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    let application = compiled();
    let mut registry = RegistryBuilder::new(BuildDescriptor {
        source_revision: "reference-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    });
    registry.register(ReferenceSql).unwrap();
    registry.register(ReferenceKv).unwrap();
    registry.register(ReferenceBlob).unwrap();
    registry.register(ReferenceQueue).unwrap();
    registry.register(ReferenceDeadLetter).unwrap();
    registry.register(ReferenceCron).unwrap();
    registry.register(ReferenceWorkflow).unwrap();
    let registry = registry.finish().unwrap();

    let mut expected = b"crab.application.v1\0".to_vec();
    push_name(&mut expected, "reference-application");
    expected.extend_from_slice(registry.release_digest().as_bytes());
    expected.extend_from_slice(&7_u16.to_be_bytes());
    for (module, cell_name, namespace, role) in [
        (SQL_MODULE, "sql", SQL_NAMESPACE, 1_u8),
        (KV_MODULE, "kv", KV_NAMESPACE, 2),
        (BLOB_MODULE, "blob", BLOB_NAMESPACE, 5),
        (QUEUE_MODULE, "queue", QUEUE_NAMESPACE, 3),
        (DEAD_LETTER_MODULE, "dead-letter", DEAD_LETTER_NAMESPACE, 3),
        (CRON_MODULE, "cron", CRON_NAMESPACE, 6),
        (WORKFLOW_MODULE, "workflow", WORKFLOW_NAMESPACE, 4),
    ] {
        push_name(&mut expected, module);
        push_name(&mut expected, cell_name);
        expected.extend_from_slice(namespace.as_bytes());
        expected.push(role);
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend_from_slice(&(64_u64 * 1024 * 1024).to_be_bytes());
        expected.extend_from_slice(&(16_u64 * 1024 * 1024).to_be_bytes());
    }
    assert_eq!(application.descriptor_bytes(), expected);
}

#[test]
fn reference_application_registers_every_primitive_and_relationship() {
    let application = compiled();
    assert_eq!(application.cell_types().len(), 7);
    assert!(application.registry().has_effect_runner(SQL_NAMESPACE));
    assert_eq!(
        application
            .registry()
            .namespace_contract(QUEUE_NAMESPACE)
            .unwrap()
            .1
            .dead_letter,
        Some(DEAD_LETTER_NAMESPACE)
    );
    assert_eq!(
        application
            .registry()
            .namespace_contract(WORKFLOW_NAMESPACE)
            .unwrap()
            .1
            .effect_targets,
        &[SQL_NAMESPACE]
    );
    assert!(!application.descriptor_bytes().is_empty());
}

#[test]
fn descriptor_digest_changes_when_build_identity_changes() {
    let first = compiled();
    let second = ReferenceApplication::compile(BuildDescriptor {
        source_revision: "different-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    })
    .unwrap();
    assert_ne!(first.descriptor_digest(), second.descriptor_digest());
}
