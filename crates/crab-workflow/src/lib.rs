//! Workflow contracts, planning, execution, caching, and experiments for Crab.

pub mod artifact;
mod atomic;
pub mod cache;
pub mod checkpoint;
pub mod discover;
pub mod dvc_inventory;
pub mod dvc_migration;
pub mod env;
pub mod error;
pub mod executor;
pub mod exp_queue;
pub mod exp_range;
pub mod exp_worktree;
pub mod experiment;
pub mod experiment_id;
pub mod external_hash;
pub mod gc;
pub mod gitignore;
pub mod graph;
pub mod hasher;
pub mod hydra;
pub mod journal;
pub mod lockfile;
pub mod lockfile_split;
pub mod materialize;
mod metrics;
pub mod param_ref;
pub mod params;
mod params_runtime;
pub mod plot_config;
pub mod resume;
pub mod retry;
pub mod run_state;
pub mod sandbox;
pub mod scheduler;
pub mod scheduler_lock;
pub mod signals;
pub mod source;
pub mod stage;
pub mod stage_cache_entry;
pub mod stage_cmd;
pub mod stage_condition;
pub mod stage_dep;
pub mod stage_name;
pub mod stage_out;
mod stage_runtime;
pub mod stage_state;
pub mod stage_types;
pub mod status;
mod store;
pub mod template;
#[cfg(feature = "watch")]
pub mod watcher;
pub mod workflow_doc;
pub mod yaml;

pub use artifact::{
    ARTIFACT_REF_PREFIX, ARTIFACT_SCHEMA_VERSION, ArtifactCatalog, ArtifactDecl, ArtifactManifest,
    ArtifactPromotion, ArtifactRegistry, artifact_stage_ref, artifact_version_ref,
    manifest_from_path, snapshot_payload, validate_artifact_name, verify_payload,
};
pub use checkpoint::{
    CHECKPOINT_PROTOCOL_VERSION, CHECKPOINT_SCHEMA_VERSION, CheckpointLineage, CheckpointRecord,
};
pub use dvc_inventory::{
    DVC_INVENTORY_SCHEMA_VERSION, DVC_MIGRATION_JOURNAL_SCHEMA_VERSION, DvcFinding,
    DvcImportProvenance, DvcInventory, DvcJournalEntry, DvcMigrationJournal, DvcOutputRecord,
    DvcRemoteDescriptor, VerificationState, inventory_project, materialize_cached_directory,
};
pub use dvc_migration::{MigrationReport, MigrationWarning, convert_dvc_to_crab};
pub use error::{Result, WorkflowError};
pub use exp_queue::{ExpQueue, ExpQueueEntry, ExpStatus};
pub use exp_worktree::{
    EXP_WORKTREE_PARENT_REL, ExperimentWorktree, sweep_orphan_experiment_tmpdirs,
};
pub use experiment::{
    EXP_META_REF_PREFIX, EXP_REF_PREFIX, EXPERIMENT_METADATA_MAX_SUPPORTED_SCHEMA,
    EXPERIMENT_METADATA_SCHEMA_VERSION, Experiment, ExperimentMetaRead, ExperimentMetadata,
    RefCasOp, STAGE_REF_PREFIX, build_exp_meta_ref_cas, build_exp_ref_cas, exp_meta_object_path,
    exp_meta_ref, exp_ref, exp_stage_refs_object_path, read_experiment_metadata,
    stage_entry_object_path, stage_ref,
};
pub use experiment_id::ExperimentId;
pub use external_hash::{
    EXTERNAL_HASH_INDEX_SCHEMA_VERSION, ExternalHashIndex, ExternalHashRecord, record_key,
};
pub use gc::{LocalWorkflowLiveSet, collect_local_workflow_live_set};
pub use graph::Graph;
pub use lockfile::{
    ExplainMissDiff, LOCKFILE_HASH_ALGO, LOCKFILE_SCHEMA_VERSION, LockedDep, LockedMetric,
    LockedOut, LockedStage, Lockfile, ResolveOutcome, ResolveStrategy, resolve, resolve_from_bytes,
};
pub use metrics::WorkflowMetrics;
pub use param_ref::ParamRef;
pub use params::{PythonLiteral, PythonParseError, Scalar, ScalarMap};
pub use plot_config::PlotConfig;
pub use retry::{FailureKind, RetryDecision, should_retry};
pub use run_state::RunState;
pub use source::{
    SOURCE_DESCRIPTOR_SCHEMA_VERSION, SourceDescriptor, load_source_descriptor,
    save_source_descriptor,
};
pub use stage::Stage;
pub use stage_cache_entry::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_MAX_SUPPORTED, ENTRY_SCHEMA_VERSION, StageCacheEntry,
    TreeManifestEntry, cached_artifacts,
};
pub use stage_cmd::Cmd;
pub use stage_condition::{StageCondition, evaluate_expr};
pub use stage_dep::{Dep, is_url_dep};
pub use stage_name::StageName;
pub use stage_out::{Out, is_external_url_out, is_external_url_out_path, validate_wdir};
pub use stage_state::StageState;
pub use stage_types::{EnvSpec, OutKind, Resources, RetryPolicy};
pub use status::{
    EXIT_CONFIG_ERROR, EXIT_OUTDATED, EXIT_UP_TO_DATE, PipelineStatus, PipelineSummary,
    StageInputError, StageInputs, StageStatus, StageStatusEntry, StatusChange, classify_stage,
    compute_pipeline_status, format_json, format_text,
};
pub use store::WorkflowStore;
pub use template::{TemplateContext, expand_foreach, expand_matrix, substitute, substitute_cmd};
pub use workflow_doc::{ArtifactMetadata, Defaults, Workflow};
pub use yaml::{parse as parse_yaml, parse_at, parse_with_base_dir, parse_with_context};
