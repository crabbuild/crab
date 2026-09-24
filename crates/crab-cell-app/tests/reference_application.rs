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
