//! Workflow-domain errors.

use std::path::PathBuf;

/// Result alias for workflow contract and planning modules.
pub type Result<T> = std::result::Result<T, WorkflowError>;

/// Errors returned by workflow contract and planning modules.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// A transient object-store failure exhausted its retry budget.
    #[error("network transient error: {0}")]
    NetworkTransient(#[source] object_store::Error),

    /// A compare-and-swap update observed a conflicting value.
    #[error("CAS conflict on {path}")]
    CasConflict {
        /// Object path that conflicted.
        path: String,
        /// ETag the caller expected, when known.
        expected_etag: Option<String>,
    },

    /// A requested workflow object or ref was not found.
    #[error("not found: {path}")]
    NotFound {
        /// Missing path or ref.
        path: String,
    },

    /// Runtime workflow configuration is invalid.
    #[error("configuration error in {origin}: {key}")]
    Configuration {
        /// Invalid key or value.
        key: String,
        /// Configuration source or subsystem.
        origin: String,
    },

    /// A raw object-store operation failed.
    #[error("object store error: {0}")]
    Storage(#[from] object_store::Error),

    /// A storage-domain error that has no narrower workflow classification.
    #[error("storage error: {0}")]
    StorageDomain(#[source] crab_storage::StorageError),

    /// GC writer admission failed before a remote workflow publication.
    #[error("GC writer admission failed: {0}")]
    GcFence(#[source] crab_coordination::CoordinationError),

    /// An internal workflow invariant failed.
    #[error("internal workflow error: {0}")]
    Internal(String),

    /// Experiment id string is not the canonical UUIDv7 form.
    #[error("experiment id {raw:?} is invalid: {reason}")]
    ExperimentIdInvalid {
        /// Raw id string supplied by the caller.
        raw: String,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Stage name string is not part of the supported workflow grammar.
    #[error("invalid workflow stage name {name:?}: {reason}")]
    StageNameInvalid {
        /// Raw stage name supplied by the caller.
        name: String,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Parameter reference is not part of the supported workflow grammar.
    #[error("invalid workflow parameter reference {key:?}: {reason}")]
    ParamRefInvalid {
        /// Reference key or file label supplied by the caller.
        key: String,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Stage output declaration is not part of the supported workflow contract.
    #[error("stage {stage:?} output {path} is malformed: {reason}")]
    StageOutMalformed {
        /// Stage that declared the output.
        stage: String,
        /// Output path or URL.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Stage working directory is not part of the supported workflow contract.
    #[error("stage {stage:?} working directory {path} is invalid: {reason}")]
    WdirInvalid {
        /// Stage that declared the working directory.
        stage: String,
        /// Working directory path.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// Workflow DAG contains a dependency cycle.
    #[error("workflow has a dependency cycle: {stages:?}")]
    WorkflowCycle {
        /// Cycle members in discovered order.
        stages: Vec<String>,
    },

    /// A stage references an output that no stage declares.
    #[error("workflow stage {consumer:?} references undefined out {out:?}")]
    WorkflowUndefinedOut {
        /// Stage that referenced the missing output.
        consumer: String,
        /// Referenced output.
        out: String,
    },

    /// Two stages declare the same output path.
    #[error("workflow stages {first:?} and {second:?} both declare out {path}")]
    WorkflowDuplicateOutput {
        /// First stage declaring the output.
        first: String,
        /// Second stage declaring the output.
        second: String,
        /// Colliding output path.
        path: PathBuf,
    },

    /// A graph invariant was lost inside the planner.
    #[error("workflow graph invariant failed: {message}")]
    GraphInvariant {
        /// Diagnostic message.
        message: String,
    },

    /// Queue entry file does not exist.
    #[error("queue entry not found: {path}")]
    QueueEntryNotFound {
        /// Entry path that was expected to exist.
        path: String,
    },

    /// Queue entry could not be serialized.
    #[error("failed to serialize queue entry {id}: {source}")]
    QueueEntrySerialize {
        /// Queue entry id.
        id: String,
        /// Serde source error.
        #[source]
        source: serde_json::Error,
    },

    /// Queue entry JSON could not be parsed.
    #[error("malformed queue entry at {path}: {source}")]
    QueueEntryMalformed {
        /// Entry path that contained malformed JSON.
        path: String,
        /// Serde source error.
        #[source]
        source: serde_json::Error,
    },

    /// Destination path for an atomic queue write has no parent.
    #[error("queue entry path has no parent: {path}")]
    QueueEntryPathNoParent {
        /// Destination path.
        path: String,
    },

    /// Atomic queue write failed while persisting the temp file.
    #[error("failed to persist queue entry to {path}: {source}")]
    QueueEntryPersist {
        /// Destination path.
        path: String,
        /// Tempfile persist source error.
        #[source]
        source: tempfile::PersistError,
    },

    /// Lockfile bytes could not be parsed or normalized into the canonical form.
    #[error("lockfile canonicalization failed at {path}: {source}")]
    LockfileCanonicalizationFailed {
        /// Lockfile path being parsed or written.
        path: PathBuf,
        /// YAML parse or validation source error.
        #[source]
        source: serde_yaml::Error,
    },

    /// A lockfile resolve was requested for bytes that are not a git conflict.
    #[error("lockfile merge conflict at {path}")]
    LockfileMergeConflict {
        /// Lockfile path being resolved.
        path: PathBuf,
    },

    /// Destination path for an atomic lockfile write has no parent.
    #[error("lockfile path has no parent: {path}")]
    LockfilePathNoParent {
        /// Destination path.
        path: PathBuf,
    },

    /// A lockfile or cache record carried a malformed `b3:` hash.
    #[error("lockfile hash {hash:?} is malformed: {reason}")]
    LockfileHashMalformed {
        /// Raw hash string supplied by the caller.
        hash: String,
        /// Stable validation reason.
        reason: String,
    },

    /// DVC YAML bytes could not be parsed before migration.
    #[error("failed to parse dvc.yaml: {source}")]
    DvcYamlParse {
        /// YAML parse source error.
        #[source]
        source: serde_yaml::Error,
    },

    /// Converted Crab YAML bytes could not be serialized.
    #[error("failed to serialize migrated crab.yaml: {source}")]
    DvcYamlSerialize {
        /// YAML serialization source error.
        #[source]
        source: serde_yaml::Error,
    },

    /// DVC migration input is valid YAML but not a supported workflow shape.
    #[error("DVC migration input invalid in {origin}: {key}")]
    DvcMigrationInvalid {
        /// Invalid key or shape.
        key: String,
        /// Source document or section.
        origin: String,
    },

    /// Workflow params input is not part of the supported scalar-document shape.
    #[error("workflow params invalid in {origin}: {key}")]
    ParamsInvalid {
        /// Invalid key, path, or parse context.
        key: String,
        /// Source document or section.
        origin: String,
    },

    /// A template context, expression, or expansion input is invalid.
    #[error("workflow template invalid in {origin}: {key}")]
    TemplateInvalid {
        /// Invalid key, expression, or path.
        key: String,
        /// Source document or template phase.
        origin: String,
    },

    /// A template expression referenced a missing key.
    #[error("workflow template key {key:?} is undefined in stage {stage:?} field {field:?}")]
    TemplateUndefined {
        /// Missing dotted key.
        key: String,
        /// Field being templated, if known.
        field: String,
        /// Stage being templated, if known.
        stage: String,
    },

    /// A foreach expansion declared no items.
    #[error("workflow stage {stage:?} has empty foreach")]
    ForeachEmpty {
        /// Stage declaring the foreach expansion.
        stage: String,
    },

    /// A matrix expansion declared an empty variable.
    #[error("workflow stage {stage:?} matrix variable {variable:?} is empty")]
    MatrixEmpty {
        /// Stage declaring the matrix expansion.
        stage: String,
        /// Empty matrix variable.
        variable: String,
    },

    /// A `crab.yaml` document could not be parsed as YAML or a raw stage shape.
    #[error("failed to parse workflow YAML at {path}: {source}")]
    YamlParse {
        /// YAML path, when known.
        path: PathBuf,
        /// YAML parse source error.
        #[source]
        source: serde_yaml::Error,
    },

    /// A `crab.yaml` document is syntactically valid but violates the workflow schema.
    #[error("workflow YAML invalid in {origin}: {key}")]
    YamlInvalid {
        /// Invalid key, field, or declaration.
        key: String,
        /// Source document section or reason.
        origin: String,
    },

    /// A stage declares a dependency path that is also one of its outputs.
    #[error("workflow stage {stage:?} has self-loop at {path}")]
    WorkflowSelfLoop {
        /// Stage containing the self-loop.
        stage: String,
        /// Path that appears as both dependency and output.
        path: PathBuf,
    },

    /// A parsed workflow field violates a semantic validation rule.
    #[error("workflow validation error in {field:?}: got {value:?}, expected {expected}")]
    WorkflowValidation {
        /// Field containing the invalid value.
        field: String,
        /// Invalid value rendered for diagnostics.
        value: String,
        /// Expected value shape or range.
        expected: String,
    },

    /// More than one workflow document matched discovery.
    #[error("ambiguous workflow discovery: {candidates:?}")]
    WorkflowDiscoveryAmbiguous {
        /// Candidate paths that made selection ambiguous.
        candidates: Vec<PathBuf>,
    },

    /// A discovered stage name is invalid.
    #[error("invalid workflow stage name {name:?}: {reason}")]
    WorkflowStageNameInvalid {
        /// Invalid stage name.
        name: String,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// A stage dependency does not exist.
    #[error("stage {stage:?} dependency missing: {path}")]
    StageDepMissing {
        /// Stage declaring the dependency.
        stage: String,
        /// Missing dependency path.
        path: PathBuf,
    },

    /// A stage dependency cannot be hashed or materialized.
    #[error("stage {stage:?} dependency malformed at {path}: {reason}")]
    StageDepMalformed {
        /// Stage declaring the dependency.
        stage: String,
        /// Malformed dependency path.
        path: PathBuf,
        /// Stable validation reason.
        reason: &'static str,
    },

    /// A stage output exceeds the configured byte limit.
    #[error("stage {stage:?} output too large: {path} is {size} bytes, limit {limit}")]
    StageOutTooLarge {
        /// Stage producing the output.
        stage: String,
        /// Oversized output path.
        path: PathBuf,
        /// Observed size.
        size: u64,
        /// Configured limit.
        limit: u64,
    },

    /// A stage produced more outputs than allowed.
    #[error("stage {stage:?} produced {count} outputs, limit {limit}")]
    StageOutCountExceeded {
        /// Stage producing outputs.
        stage: String,
        /// Observed output count.
        count: usize,
        /// Configured limit.
        limit: usize,
    },

    /// A stage command exited unsuccessfully.
    #[error("stage {stage:?} exited with code {exit_code}")]
    StageExecFailed {
        /// Failed stage.
        stage: String,
        /// Process exit code.
        exit_code: i32,
    },

    /// A stage command was terminated by a signal.
    #[error("stage {stage:?} terminated by signal {signal}")]
    StageExecSignaled {
        /// Failed stage.
        stage: String,
        /// Terminating signal.
        signal: i32,
    },

    /// A stage command exceeded its timeout.
    #[error("stage {stage:?} timed out after {elapsed_ms}ms")]
    StageExecTimeout {
        /// Timed-out stage.
        stage: String,
        /// Elapsed runtime.
        elapsed_ms: u64,
    },

    /// A stage ran out of local disk space.
    #[error("stage {stage:?} ran out of disk writing {path}")]
    StageDiskFull {
        /// Failed stage.
        stage: String,
        /// Path being written.
        path: PathBuf,
    },

    /// No cache entry matched a stage.
    #[error("stage {stage:?} cache miss: {reason}")]
    StageCacheMiss {
        /// Stage whose cache entry was requested.
        stage: String,
        /// Miss reason.
        reason: String,
    },

    /// Existing output policy rejected a stage write.
    #[error("stage {stage:?} overwrite conflict at {path}: {reason}")]
    StageOverwriteConflict {
        /// Stage producing the output.
        stage: String,
        /// Conflicting output path.
        path: PathBuf,
        /// Stable conflict reason.
        reason: &'static str,
    },

    /// The current build cannot execute the requested remote stage operation.
    #[error("remote stage execution not supported in this build")]
    StageRemoteExecutionUnsupported,

    /// An experiment worktree id already exists.
    #[error("experiment id collision: {id}")]
    ExperimentCollision {
        /// Colliding experiment id.
        id: String,
    },

    /// Experiment metrics bytes do not match the schema.
    #[error("metrics schema mismatch at {path}: {source}")]
    MetricsSchemaMismatch {
        /// Metrics path.
        path: PathBuf,
        /// JSON parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A workflow journal could not be opened or queried.
    #[error("workflow journal open failed at {path}: {source}")]
    WorkflowJournalOpen {
        /// Journal path.
        path: PathBuf,
        /// SQLite error.
        #[source]
        source: rusqlite::Error,
    },

    /// A workflow journal contains invalid state.
    #[error("workflow journal corrupt for run {run_id}: {detail}")]
    WorkflowJournalCorrupt {
        /// Run id, or another stable record label when no run is available.
        run_id: String,
        /// Corruption detail.
        detail: String,
    },

    /// A journal schema is newer than this library supports.
    #[error(
        "workflow journal schema newer than supported for run {run_id}: found v{found}, supported v{supported}"
    )]
    WorkflowJournalSchemaNewer {
        /// Run id.
        run_id: String,
        /// Schema version found.
        found: u16,
        /// Maximum supported schema version.
        supported: u16,
    },

    /// A journal stage transition violates the state machine.
    #[error("illegal state transition for stage {stage:?}: {from} -> {to}")]
    WorkflowStateTransitionIllegal {
        /// Stage being transitioned.
        stage: String,
        /// Prior state.
        from: String,
        /// Requested state.
        to: String,
    },

    /// Another process held the workflow scheduler lock past the timeout.
    #[error("workflow lock timeout after {waited_ms}ms")]
    WorkflowLockTimeout {
        /// Holder process id when readable.
        held_by: Option<u32>,
        /// Time spent waiting.
        waited_ms: u64,
    },

    /// A stage attempted an operation outside its hermetic policy.
    #[error("stage {stage:?} hermetic violation at {path}")]
    WorkflowHermeticViolation {
        /// Stage violating the policy.
        stage: String,
        /// Disallowed path.
        path: PathBuf,
    },

    /// A cache record uses a newer schema than this library supports.
    #[error(
        "cache entry schema newer than supported for stage hash {stage_hash}: found v{found}, supported v{supported}"
    )]
    CacheEntrySchemaNewer {
        /// Cache key.
        stage_hash: String,
        /// Schema version found.
        found: u16,
        /// Maximum supported schema version.
        supported: u16,
    },

    /// Experiment metadata uses a newer schema than this library supports.
    #[error(
        "experiment metadata schema newer than supported for {id}: found v{found}, supported v{supported}"
    )]
    WorkflowExperimentMetadataSchemaNewer {
        /// Experiment id.
        id: String,
        /// Schema version found.
        found: u16,
        /// Maximum supported schema version.
        supported: u16,
    },

    /// A remote cache artifact failed content verification.
    #[error(
        "remote cache entry corrupt for stage {stage_hash}: file {path:?} expected {expected}, got {actual}"
    )]
    CacheEntryCorrupt {
        /// Cache key.
        stage_hash: String,
        /// Corrupt file path.
        path: String,
        /// Expected content hash.
        expected: String,
        /// Observed content hash.
        actual: String,
    },

    /// A cache manifest names a different stage hash than requested.
    #[error("remote cache entry hash mismatch: manifest {manifest_hash}, local {local_hash}")]
    CacheEntryHashMismatch {
        /// Hash recorded in the manifest.
        manifest_hash: String,
        /// Locally computed hash.
        local_hash: String,
    },

    /// A cache manifest is structurally invalid or contains an unsafe path.
    #[error("cache entry invalid for stage hash {stage_hash}: {detail}")]
    CacheEntryInvalid {
        /// Cache key, when known.
        stage_hash: String,
        /// Stable validation detail.
        detail: String,
    },

    /// A cache push was requested while the remote is read-only.
    #[error("remote cache is read-only")]
    RemoteCacheReadonly,

    /// A journal write failed because the filesystem is full.
    #[error("journal disk full writing {path}")]
    JournalDiskFull {
        /// Journal path.
        path: PathBuf,
    },

    /// Local filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<crab_storage::StorageError> for WorkflowError {
    fn from(error: crab_storage::StorageError) -> Self {
        match error {
            crab_storage::StorageError::NetworkTransient { source } => {
                Self::NetworkTransient(source)
            }
            crab_storage::StorageError::StateConflict { path } => Self::CasConflict {
                path,
                expected_etag: None,
            },
            crab_storage::StorageError::NotFound { path } => Self::NotFound { path },
            crab_storage::StorageError::NotSupported { source }
            | crab_storage::StorageError::ProviderConfig { source, .. }
            | crab_storage::StorageError::UrlStoreConfig { source, .. }
            | crab_storage::StorageError::ObjectStore { source } => Self::Storage(source),
            crab_storage::StorageError::Io { source } => Self::Io(source),
            other => Self::StorageDomain(other),
        }
    }
}
