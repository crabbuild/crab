//! Crate-wide error type and `Result` alias.
//!
//! Every variant carries a stable `CRAB-E####` code in its `Display`
//! output so CLI users can look up remediation via `crab errors <code>`.
//! Codes are append-only: once assigned, they never move variant.

use std::path::PathBuf;
use std::time::Duration;

use crab_types::error::ErrorCategory;

mod read_failure;
pub use read_failure::ReadFailure;

/// Crate-wide error type returned by every fallible public API.
#[derive(thiserror::Error, Debug)]
pub enum CrabError {
    #[error("{0}")]
    Read(#[source] ReadFailure),

    #[error("hydrate failed for {failed} file(s): {source}")]
    HydrationFailed {
        failed: u64,
        #[source]
        source: std::sync::Arc<CrabError>,
    },

    // Transient — retry-worthy.
    #[error("network transient error [CRAB-E0001]: {0}")]
    NetworkTransient(#[source] object_store::Error),

    // `retry_after` is advisory; the retry layer reads it directly rather
    // than parsing it back out of the Display output.
    #[error("throttled [CRAB-E0002]")]
    Throttled { retry_after: Option<Duration> },

    // Conflict — state-dependent.
    #[error("CAS conflict on {path} [CRAB-E0010]")]
    CasConflict {
        path: String,
        expected_etag: Option<String>,
    },

    #[error("non-fast-forward on {ref_name} [CRAB-E0017]: have {have}, want {want}")]
    NonFastForward {
        ref_name: String,
        have: String,
        want: String,
    },

    #[error("ref already exists: {name} [CRAB-E0011]")]
    RefAlreadyExists { name: String },

    #[error(
        "push rejected — ref '{ref_name}' is locked by another push in progress. \
         Retry after it completes. [CRAB-E0012]"
    )]
    PushLockHeld {
        ref_name: String,
        holder: String,
        /// Unix timestamp (seconds) at which the current lease expires.
        /// `None` when the lock payload could not be parsed.
        expires_at_unix: Option<u64>,
    },

    // Data integrity.
    #[error("corrupt object at {path} [CRAB-E0020]: {reason}")]
    CorruptObject { path: String, reason: String },

    // Pack evidence uses the same integrity contract as remote object
    // validation, with a typed payload so dependency causes stay inspectable.
    #[error("corrupt Git pack evidence [CRAB-E0020]: {0}")]
    GitPackCorrupt(#[source] crab_git::pack_locator::PackLocatorError),
    #[error("origin object at {path} failed integrity verification [CRAB-E0020]: {source}")]
    OriginIntegrity {
        path: String,
        #[source]
        source: crab_cache::CacheError,
    },

    #[error("chunk not found [CRAB-E0021]: {hash}")]
    ChunkNotFound { hash: String },

    // Permanent — user-facing.
    #[error("not found: {path} [CRAB-E0030]")]
    NotFound { path: String },

    #[error("forbidden: {path} [CRAB-E0031]")]
    Forbidden { path: String },

    #[error("no credentials available [CRAB-E0040]")]
    NoCredentials,

    #[error("authentication failed [CRAB-E0042]: credentials rejected for {path}")]
    AuthFailed { path: String },

    #[error("credentials expired [CRAB-E0043]: {path}")]
    AuthExpired { path: String },

    #[error("insufficient local space [CRAB-E0041]: need {needed}, have {available}")]
    InsufficientSpace { needed: u64, available: u64 },

    // Configuration. `origin` rather than `source` because thiserror reserves
    // the latter as the shorthand for `#[source]`.
    #[error("configuration error [CRAB-E0050] in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("incompatible format [CRAB-E0051]: require {required}, have {found}")]
    IncompatibleFormat { required: String, found: String },

    #[error("invalid glob pattern [CRAB-E0052]: {0}")]
    InvalidPattern(#[from] globset::Error),

    // Framing and transport.
    #[error("remote helper protocol error [CRAB-E0060]: {0}")]
    Protocol(String),

    #[error("I/O error [CRAB-E0070]: {0}")]
    Io(#[from] std::io::Error),

    #[error("object store error [CRAB-E0071]: {0}")]
    Storage(#[from] object_store::Error),

    // Staging integrity.
    #[error("staging corrupt [CRAB-E0080]: {0}")]
    StagingCorrupt(String),

    #[error("staging is locked by another process [CRAB-E0081]{}", match .holder_pid {
        Some(pid) => format!(" (held by PID {pid})"),
        None => String::new(),
    })]
    StagingLocked { holder_pid: Option<u32> },

    #[error("chunk hash mismatch [CRAB-E0082]: requested {requested}, got {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("segment CRC mismatch [CRAB-E0083] at segment {segment_id} offset {offset}")]
    CrcMismatch { segment_id: u64, offset: u64 },

    #[error("pack integrity check failed [CRAB-E0084]: expected {expected}, computed {computed}")]
    PackIntegrity { expected: String, computed: String },

    #[error("shard reconstruction incomplete [CRAB-E0085]: file {file_hash}{} has {uncovered_chunks} unresolved chunk(s); first gap at index {example_chunk_index} (chunk {example_chunk_hash})", match .path {
        Some(p) => format!(" (path {p})"),
        None => String::new(),
    })]
    IncompleteShardReconstruction {
        file_hash: String,
        path: Option<String>,
        uncovered_chunks: usize,
        example_chunk_hash: String,
        example_chunk_index: u32,
    },

    #[error(
        "pointer has no staged chunks [CRAB-E0086]: {missing} of {total} pushed pointer(s) \
         lack local staging entries (first missing: file {example_file_hash}, size {example_size}); \
         re-stage with `crab add <path>` so the clean filter records chunk offsets before pushing"
    )]
    PointerMissingStaging {
        total: usize,
        missing: usize,
        example_file_hash: String,
        example_size: u64,
    },

    // Push connectivity — an object reachable from a ref tip is missing
    // from the local ODB at the moment we're about to commit the push.
    // If this fires we've either generated an incomplete pack or the
    // local ODB is corrupt; either way we refuse to move the ref.
    #[error(
        "push connectivity check failed [CRAB-E0087] for ref '{ref_name}': \
         {total_missing} object(s) reachable from the new tip are missing \
         from the local ODB (first missing: {oid}); refusing to commit a \
         ref that would point at incomplete history"
    )]
    PushConnectivityMissing {
        ref_name: String,
        oid: String,
        total_missing: usize,
    },

    // Carrier variant for push pipeline errors that still need to
    // preserve per-ref outcomes from upstream steps. Without this the
    // collapse site in `remote_helper.rs` would overwrite every ref
    // with the same error message, destroying the structured per-ref
    // state produced by `unified_manifest_cas` and `verify_connectivity`.
    //
    // `outcomes` carries the exact result for each ref. A pre-commit
    // batch failure contains no `Ok` outcome because no ref was made
    // durable. `source` carries the underlying `CrabError` so exit
    // codes and structured output still pick up the root cause.
    //
    // Assigned E0088 rather than the E0091 originally sketched in the
    // spec because E0091 was already claimed by `BeyondShallowBoundary`.
    // Codes in this enum are append-only per the module doc comment.
    #[error("push pipeline failed with partial outcomes [CRAB-E0088]: {source}")]
    PushPartialOutcome {
        outcomes: Box<crate::git::push::PushResult>,
        #[source]
        source: Box<CrabError>,
    },

    #[error(
        "file changed during staging [CRAB-E0089]: {path}; \
         first pass hash {first_hash} size {first_size}, \
         second pass hash {second_hash} size {second_size}"
    )]
    FileChangedDuringStaging {
        path: String,
        first_hash: String,
        second_hash: String,
        first_size: u64,
        second_size: u64,
    },

    // Shallow boundary — commit not reachable within the shallow depth.
    #[error(
        "commit {oid} is beyond the shallow boundary [CRAB-E0091]: \
         use `git fetch --deepen=N` or `git fetch --unshallow` to access deeper history"
    )]
    BeyondShallowBoundary { oid: String },

    // Per-pack size cap — mirrors git's `receive.maxInputSize`.
    // Aggregate closures are split into bounded packs. This remains an
    // error when one object cannot fit in an individually bounded pack.
    #[error("pack too large [CRAB-E0092]: {size} bytes exceeds limit {limit}")]
    PackTooLarge { size: u64, limit: u64 },

    // Push-side fsck — git rejected an object while indexing a generated
    // pack. Surfaced before pack upload or manifest commit.
    #[error("malformed object [CRAB-E0093]: {kind} {oid}: {detail}")]
    PushMalformedObject {
        oid: String,
        kind: String,
        detail: String,
    },

    // Upload-pack policy rejection — the requested SHA is not permitted
    // under the repo's uploadpack.allow* configuration. Carried through
    // the fetch pipeline and surfaced per-ref via FetchRejectReason::NotAllowed.
    //
    // Assigned E0094 rather than the next sequential E0093 because
    // E0093 is reserved for a future PushMalformedObject variant
    // landing with the object-level fsck work; per-module code
    // numbering is append-only.
    #[error("fetch not allowed [CRAB-E0094]: sha {sha} rejected: {reason}")]
    FetchNotAllowed { sha: String, reason: String },

    // Fetch egress budget exceeded — mirror of PackTooLarge on the read
    // path so an expensive fetch can be stopped before it finishes
    // bursting a client's budget.
    #[error("fetch too large [CRAB-E0095]: {size} bytes exceeds limit {limit}")]
    FetchTooLarge { size: u64, limit: u64 },

    // Manifest inventory validation — an admitted ref tip was absent from the
    // exact immutable pack set. Newly installed packs are rolled back before
    // the error returns.
    #[error(
        "fetched manifest inventory is incomplete [CRAB-E0096]: {kind} {oid} in pack {pack_id}: {detail}"
    )]
    FetchMalformedObject {
        pack_id: String,
        oid: String,
        kind: String,
        detail: String,
    },

    // Push command integration — `crab push --rebase-on-non-fast-forward`
    // could not rebase the local commit on the remote tip. This is a
    // merge/rebase conflict for the agent to resolve, not a Crab bug.
    #[error("push integration failed [CRAB-E0097]: {command}: {message}")]
    PushIntegrationFailed { command: String, message: String },

    // Cancellation — user-initiated via SIGINT/SIGTERM.
    #[error("operation cancelled [CRAB-E0090]")]
    Cancelled,

    // Bug.
    #[error("internal error [CRAB-E0099]: {0}")]
    Internal(String),

    // LFS pointer parsing errors.
    #[error("invalid LFS pointer [CRAB-E0100]: {reason}")]
    InvalidLfsPointer { reason: String },

    // LFS object integrity.
    #[error("LFS object corrupt [CRAB-E0101]: oid {oid}")]
    LfsObjectCorrupt { oid: String },

    // LFS object not found in remote store.
    #[error("LFS object missing [CRAB-E0102]: oid {oid}")]
    LfsObjectMissing { oid: String },

    // LFS file lock conflict.
    #[error("file locked by {owner} [CRAB-E0103]: {path}")]
    LfsLockConflict { path: String, owner: String },

    // LFS transfer agent protocol error.
    #[error("LFS transfer protocol error [CRAB-E0104]: {0}")]
    LfsTransferProtocol(String),

    // LFS migration error.
    #[error("LFS migration failed [CRAB-E0105]: {reason}")]
    LfsMigrationFailed { reason: String },

    // LFS command intentionally unavailable until its safety contract is wired.
    #[error("LFS command unsupported [CRAB-E0106]: {command}: {reason}")]
    LfsUnsupported { command: String, reason: String },

    // Cache service client errors (timeouts, connection refused, HTTP errors).
    #[error("cache service error [CRAB-E0110]: {reason}")]
    CacheService { reason: String },

    #[error("{diagnostic} [CRAB-E0140]")]
    ManagedRepository {
        diagnostic: crab_auth_store::ManagedRepositoryDiagnostic,
    },

    // Import: `--from` must be raw; `crab://` sources belong to
    // `crab clone`.
    #[error(
        "import source must be a raw cloud URL [CRAB-E0118]: \
         got {url}; use s3://, gs://, az://, or file:// \
         (for an existing Crab repo, use `crab clone`)"
    )]
    ImportSourceMustBeRaw { url: String },

    // Import: cross-cloud raw targets are rejected; opt in via
    // `crab://` on `--to` to make the cross-cloud intent explicit.
    #[error(
        "import scheme mismatch [CRAB-E0119]: --from is {from_scheme}, \
         --to is {to_scheme}; use matching raw schemes or write the \
         target as crab:// for cross-cloud imports"
    )]
    ImportSchemeMismatch {
        from_scheme: String,
        to_scheme: String,
    },

    // Import: the resume journal's recorded plan disagrees with the
    // canonicalized CLI arguments the user just passed. Strict match
    // so a partial re-run does not silently change the commit graph.
    #[error("import plan mismatch [CRAB-E0114]: recorded ({recorded}) != provided ({provided})")]
    ImportPlanMismatch { recorded: String, provided: String },

    // Import: `--resume` was requested but no resume journal exists
    // at the expected path. The user either targeted the wrong
    // `--into` directory or tried to resume a run whose journal
    // was already cleaned up on a prior success.
    #[error(
        "no import journal at {path} [CRAB-E0115]; \
         pass --from / --to to start a new import, or point --into at \
         the directory where the interrupted run lives"
    )]
    ImportNoJournal { path: String },

    // Import: the user asked for `--versions on` but the source
    // bucket is flat. We detect this up front so the user can
    // retry with `--versions auto` or `off` rather than hitting a
    // confusing failure mid-ingest.
    #[error(
        "versioning requested but source bucket does not have versioning enabled \
         [CRAB-E0120]: {url}"
    )]
    ImportVersioningUnavailable { url: String },

    // Import: the window planner produced more commits than the
    // user's `--max-commits` ceiling allows. Raised before any
    // work begins so the user can widen `--window` and retry.
    #[error(
        "commit ceiling exceeded [CRAB-E0121]: {planned} commits exceeds \
         --max-commits {ceiling}; try a larger --window"
    )]
    ImportCommitCeilingExceeded { planned: u64, ceiling: u64 },

    // Import: the CLI / window planner received a `--since` /
    // `--until` pair where `since > until`. The range is empty by
    // construction; surfacing the error up front keeps us from
    // doing enumeration work that can't produce output.
    #[error("invalid history range [CRAB-E0122]: --since {since} must be before --until {until}")]
    ImportInvalidHistoryRange { since: String, until: String },

    // Import: target directory already contains content that isn't
    // a freshly-initialized empty git repo. Surfaces before any
    // commit so we never clobber user data.
    #[error(
        "import target directory is not empty [CRAB-E0123]: {path}; \
         use --force to overwrite or pick an empty --into directory"
    )]
    ImportTargetNotEmpty { path: String },

    // Import: the source prefix resolves to an existing Crab
    // repo (refs/HEAD or manifests/ already published under the
    // source prefix). The user almost certainly meant
    // `crab clone`; refuse unless `--force` is set.
    #[error(
        "source prefix looks like a Crab repo [CRAB-E0112]: {url}; \
         use `crab clone` for Crab-backed sources, or pass --force \
         to ingest its raw bytes anyway"
    )]
    ImportSourceIsCrabRepo { url: String },

    // Import: source prefix is a `.gitattributes filter=lfs` tree,
    // and the user left LFS source handling at the default `fail`.
    #[error(
        "source bucket uses Git LFS format [CRAB-E0113]: {url}; \
         use --lfs-source resolve with --lfs-objects, or --lfs-source skip"
    )]
    ImportLfsSourceUnsupported { url: String },
    /// The source is LFS-formatted and `--lfs-source resolve` was set, but
    /// the companion LFS object store could not be discovered. Use
    /// `--lfs-objects <URL>` to specify the store explicitly.
    #[error(
        "LFS object store not discovered for import of {url:?}; use --lfs-objects to specify the store URL or ensure a .lfsstore file exists at the source prefix"
    )]
    ImportLfsStoreNotFound { url: String },

    // Import: the source prefix overlaps the target's Crab
    // layout. This is a hard error — reading xorbs back from the
    // very prefix we're writing to never produces a coherent
    // repo. No `--force` override.
    #[error("source prefix collides with target .crab layout [CRAB-E0117]: {detail}")]
    ImportPrefixCollision { detail: String },

    // Import: `git commit` requires a configured identity.
    // Assemble checks this once up front so a long enumerate →
    // ingest run does not die at the commit step.
    #[error(
        "git identity not configured [CRAB-E0116]: set user.name and user.email \
         via `git config --global user.name 'Your Name'` and `git config --global user.email you@example.com`"
    )]
    ImportMissingGitIdentity,

    // Import: the target git repo already has an `origin` remote
    // and the user did not pass `--force`. Silent overwrite would
    // surprise users who pointed a half-configured repo at the
    // import.
    #[error(
        "remote 'origin' already configured in target repo [CRAB-E0111]: \
         existing URL {existing_url}; use --force to overwrite with {new_url}"
    )]
    ImportRemoteExists {
        existing_url: String,
        new_url: String,
    },

    // ─── Workflow layer ───
    //
    // Workflow YAML parsing failed. The source error carries the line /
    // column so users can jump straight to the offending stanza.
    #[error("workflow parse error [CRAB-E0200] at {path}: {source}")]
    WorkflowParse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    // Topological sort failed. `stages` lists the cycle members in the
    // order they were discovered so the user can read it as a → b → a.
    #[error("workflow has a dependency cycle [CRAB-E0201]: {stages:?}")]
    WorkflowCycle { stages: Vec<String> },

    // A stage references an `out` that no other stage produces. Surfaces
    // before execution so we never start a run that cannot finish.
    #[error("workflow stage '{consumer}' references undefined out '{out}' [CRAB-E0202]")]
    WorkflowUndefinedOut { consumer: String, out: String },

    #[error("invalid workflow stage name '{name}' [CRAB-E0203]: {reason}")]
    WorkflowStageNameInvalid { name: String, reason: &'static str },

    // More than one `workflow.yaml` resolved for the CWD — we refuse to
    // guess which one the user meant.
    #[error("ambiguous workflow discovery [CRAB-E0204]: {candidates:?}")]
    WorkflowDiscoveryAmbiguous { candidates: Vec<PathBuf> },

    #[error("stage '{stage}' dependency missing [CRAB-E0205]: {path}")]
    StageDepMissing { stage: String, path: PathBuf },

    #[error("stage '{stage}' dependency malformed [CRAB-E0206] at {path}: {reason}")]
    StageDepMalformed {
        stage: String,
        path: PathBuf,
        reason: &'static str,
    },

    #[error("stage '{stage}' output malformed [CRAB-E0207] at {path}: {reason}")]
    StageOutMalformed {
        stage: String,
        path: PathBuf,
        reason: &'static str,
    },

    #[error("stage '{stage}' output too large [CRAB-E0208]: {path} is {size} bytes, limit {limit}")]
    StageOutTooLarge {
        stage: String,
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    #[error("stage '{stage}' produced {count} outputs, exceeds limit {limit} [CRAB-E0209]")]
    StageOutCountExceeded {
        stage: String,
        count: usize,
        limit: usize,
    },

    #[error("stage '{stage}' missing required env var '{var}' [CRAB-E0210]")]
    StageEnvMissing { stage: String, var: String },

    #[error("stage '{stage}' exited with code {exit_code} [CRAB-E0211]")]
    StageExecFailed { stage: String, exit_code: i32 },

    // Signalled kills are kept distinct from clean exits: a SIGKILL under
    // memory pressure is a retry candidate; a `exit 1` is not.
    #[error("stage '{stage}' terminated by signal {signal} [CRAB-E0212]")]
    StageExecSignaled { stage: String, signal: i32 },

    #[error("stage '{stage}' timed out after {elapsed_ms}ms [CRAB-E0213]")]
    StageExecTimeout { stage: String, elapsed_ms: u64 },

    #[error("stage '{stage}' ran out of disk [CRAB-E0214] writing {path}")]
    StageDiskFull { stage: String, path: PathBuf },

    #[error("stage '{stage}' cache miss [CRAB-E0215]: {reason}")]
    StageCacheMiss { stage: String, reason: String },

    #[error("stage '{stage}' retry budget exhausted after {attempts} attempts [CRAB-E0216]")]
    StageRetryExhausted { stage: String, attempts: u32 },

    #[error("stage '{stage}' overwrite conflict [CRAB-E0217] at {path}: {reason}")]
    StageOverwriteConflict {
        stage: String,
        path: PathBuf,
        reason: &'static str,
    },

    #[error("stage '{stage}' side-effects retry limit reached [CRAB-E0218]")]
    StageSideEffectsRetryLimit { stage: String },

    #[error("stage '{stage}' on_cache_hit hook failed with exit code {exit_code} [CRAB-E0239]")]
    StageSideEffectHookFailed { stage: String, exit_code: i32 },

    #[error("lockfile for stage '{stage}' is stale [CRAB-E0219]")]
    LockfileStale { stage: String },

    // Canonicalization wraps a serde_yaml failure encountered while
    // re-serializing a lockfile to its normalized form.
    #[error("lockfile canonicalization failed [CRAB-E0220] at {path}: {source}")]
    LockfileCanonicalizationFailed {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("lockfile merge conflict [CRAB-E0221] at {path}")]
    LockfileMergeConflict { path: PathBuf },

    #[error("experiment not found [CRAB-E0222]: {id}")]
    ExperimentNotFound { id: String },

    #[error("experiment id collision [CRAB-E0223]: {id}")]
    ExperimentCollision { id: String },

    #[error("metrics schema mismatch [CRAB-E0224] at {path}: {source}")]
    MetricsSchemaMismatch {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("workflow journal open failed [CRAB-E0225] at {path}: {source}")]
    WorkflowJournalOpen {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("workflow journal corrupt for run {run_id} [CRAB-E0226]: {detail}")]
    WorkflowJournalCorrupt { run_id: String, detail: String },

    // Forward-compat: the on-disk journal was written by a newer binary
    // than we can read. Refuse rather than silently misinterpret it.
    #[error(
        "workflow journal schema newer than supported for run {run_id} [CRAB-E0227]: \
         found v{found}, supported v{supported}"
    )]
    WorkflowJournalSchemaNewer {
        run_id: String,
        found: u16,
        supported: u16,
    },

    // Resume detected that the filesystem contents don't match the
    // journal snapshot — we refuse rather than produce an inconsistent run.
    #[error(
        "workflow resume filesystem drift on stage '{stage}' [CRAB-E0228]: \
         expected {expected}, observed {observed}"
    )]
    WorkflowResumeFilesystemDrift {
        stage: String,
        expected: String,
        observed: String,
    },

    // `from` / `to` are `String` so this variant does not depend on the
    // workflow state-machine types. Tighten when those types land.
    #[error("illegal state transition for stage '{stage}' [CRAB-E0229]: {from} → {to}")]
    WorkflowStateTransitionIllegal {
        stage: String,
        from: String,
        to: String,
    },

    #[error(
        "workflow lock timeout [CRAB-E0230]{} after {waited_ms}ms",
        match .held_by {
            Some(pid) => format!(" (held by PID {pid})"),
            None => String::new(),
        }
    )]
    WorkflowLockTimeout {
        held_by: Option<u32>,
        waited_ms: u64,
    },

    #[error("workflow feature is disabled [CRAB-E0231]")]
    WorkflowDisabled,

    // Stage tried to read or write outside its declared sandbox. We
    // surface the offending path so users can either fix the stage or
    // extend its `reads:` / `writes:` declarations.
    #[error("stage '{stage}' hermetic violation [CRAB-E0232] at {path}")]
    WorkflowHermeticViolation { stage: String, path: PathBuf },

    #[error(
        "cache entry schema newer than supported for stage hash {stage_hash} [CRAB-E0233]: \
         found v{found}, supported v{supported}"
    )]
    CacheEntrySchemaNewer {
        stage_hash: String,
        found: u16,
        supported: u16,
    },

    #[error("remote stage execution not supported in this build [CRAB-E0234]")]
    StageRemoteExecutionUnsupported,

    // Hermetic sandbox is scoped for phase 5 of the workflow layer
    // (FUSE-backed read/write isolation). Phase 1 parses `hermetic:
    // true` and feeds it into the stage hash so cache keys stay
    // honest, but refuses to execute — running the user command in a
    // non-isolated environment would violate the declared contract.
    #[error(
        "stage '{stage}' declares hermetic: true but the hermetic sandbox \
         is not yet implemented [CRAB-E0235]; hermetic execution lands \
         in phase 5 of the workflow layer"
    )]
    StageHermeticNotImplemented { stage: String },

    // Two stages in the same workflow declare the same out path. The
    // DAG builder rejects this at parse time because the edge
    // inference from producer-out → consumer-dep becomes ambiguous,
    // and materialization would race to write the same path.
    #[error("workflow stages '{first}' and '{second}' both declare out '{path}' [CRAB-E0236]")]
    WorkflowDuplicateOutput {
        first: String,
        second: String,
        path: PathBuf,
    },

    // Experiment IDs must be canonical UUIDv7 strings. Anything else
    // (v4 uuid, truncated hex, arbitrary garbage) is rejected up front
    // so downstream ref-name and object-path builders never emit an
    // unparseable identifier.
    #[error("invalid experiment id '{raw}' [CRAB-E0237]: {reason}")]
    WorkflowExperimentIdInvalid { raw: String, reason: &'static str },

    // The on-disk experiment metadata blob was written by a newer
    // binary than we can read. Refuse rather than silently misread
    // fields that may have gained new semantics.
    #[error(
        "experiment metadata schema newer than supported for experiment {id} [CRAB-E0238]: \
         found v{found}, supported v{supported}"
    )]
    WorkflowExperimentMetadataSchemaNewer {
        id: String,
        found: u16,
        supported: u16,
    },

    // Remote cache integrity: a materialized output file's blake3
    // hash does not match the hash recorded in the manifest.
    #[error(
        "remote cache entry corrupt for stage {stage_hash} [CRAB-E0240]: \
         file '{path}' hash mismatch (expected {expected}, got {actual})"
    )]
    CacheEntryCorrupt {
        stage_hash: String,
        path: String,
        expected: String,
        actual: String,
    },

    // Remote cache integrity: the manifest's stage_hash field does
    // not match the locally-computed stage hash.
    #[error(
        "remote cache entry hash mismatch [CRAB-E0241]: \
         manifest stage_hash {manifest_hash} != local {local_hash}"
    )]
    CacheEntryHashMismatch {
        manifest_hash: String,
        local_hash: String,
    },

    // The remote cache is configured as read-only; push operations
    // are rejected.
    #[error(
        "remote cache is read-only [CRAB-E0242]: \
         --cache-push is disabled when remote_cache_readonly = true"
    )]
    RemoteCacheReadonly,

    // Semantic validation: a field has an invalid value (e.g.
    // timeout: "banana", retry.max_attempts: -1).
    #[error(
        "workflow validation error [CRAB-E0243] in '{field}': got '{value}', expected {expected}"
    )]
    WorkflowValidationError {
        field: String,
        value: String,
        expected: String,
    },

    // Self-loop: a stage declares a dep that is also one of its own
    // outs. Detected at parse time before the full DAG cycle check.
    #[error("workflow self-loop [CRAB-E0244] in stage '{stage}': dep '{path}' is also an out")]
    WorkflowSelfLoop { stage: String, path: PathBuf },

    #[error("journal disk full [CRAB-E0245] writing {path}")]
    JournalDiskFull { path: PathBuf },

    // Template substitution references a key that does not exist in
    // any source (vars, params, env). Surfaced at parse time so the
    // user sees the offending field and stage before execution starts.
    #[error(
        "undefined template variable '{key}' [CRAB-E0246] in field '{field}' of stage '{stage}'"
    )]
    WorkflowTemplateUndefined {
        key: String,
        field: String,
        stage: String,
    },

    // A `foreach:` stage has an empty iteration source (empty list or
    // empty dict). Surfaced at parse time so the user fixes the YAML
    // rather than silently producing zero stages.
    #[error("foreach expansion for stage '{stage}' has an empty iteration source [CRAB-E0247]")]
    WorkflowForeachEmpty { stage: String },

    // A `matrix:` stage has an empty value list for one of its
    // variables. Surfaced at parse time so the user fixes the YAML
    // rather than silently producing zero stages.
    #[error(
        "matrix expansion for stage '{stage}' has an empty value list for variable '{variable}' [CRAB-E0248]"
    )]
    WorkflowMatrixEmpty { stage: String, variable: String },

    // ─── Storage economy (tier / xorb optimization / cost) ───
    #[error(
        "lifecycle conflict for prefix {prefix} [CRAB-E0300]: existing rule {existing_id} differs from new rule {new_id}"
    )]
    TierLifecycleConflict {
        prefix: String,
        existing_id: String,
        new_id: String,
    },

    #[error("not authorized to apply lifecycle [CRAB-E0301]: missing {required_permission}")]
    TierApplyUnauthorized { required_permission: String },

    #[error("provider {provider} is not supported for tiering [CRAB-E0302]")]
    TierProviderUnsupported { provider: String },

    #[error("xorb {xorb} is in class {class} [CRAB-E0310]; archive restore required")]
    ArchiveRestoreRequired {
        xorb: String,
        class: String,
        estimated_eta: Option<String>,
    },

    #[error("restore timed out for {xorb} in class {class} [CRAB-E0311] after {elapsed_secs}s")]
    ArchiveRestoreTimeout {
        xorb: String,
        class: String,
        elapsed_secs: u64,
    },

    #[error("restore tier {tier} is not supported for class {class} [CRAB-E0312]")]
    RestoreTierUnsupported {
        tier: String,
        class: String,
        supported: Vec<String>,
    },

    #[error(
        "early delete blocked [CRAB-E0320]: class {class}, age {age_days}d, min {min_days}d, estimated penalty ${penalty_usd}"
    )]
    GcEarlyDeleteBlocked {
        class: String,
        age_days: u32,
        min_days: u32,
        penalty_usd: String,
    },

    #[error("object-lock retention in effect [CRAB-E0321] until {until}")]
    ObjectLockedRetention { path: String, until: String },

    #[error(
        "garbage collection completed partially [CRAB-E0322]: deleted {objects_deleted} object(s), {delete_failures} deletion(s) failed, reconciliation_failed={reconciliation_failed}: {source}"
    )]
    GcPartialFailure {
        objects_deleted: u64,
        delete_failures: u64,
        reconciliation_failed: bool,
        #[source]
        source: Box<CrabError>,
    },

    #[error(
        "xorb optimization profile '{name}' out of range [CRAB-E0330]: target_xorb_bytes={bytes} (allowed 4 MiB..2 GiB)"
    )]
    OptimizeXorbsProfileOutOfRange { name: String, bytes: u64 },

    #[error("xorb optimization source {xorb} is corrupt (hash mismatch) [CRAB-E0331]; skipping")]
    OptimizeXorbsCorruptSource { xorb: String },

    #[error("xorb optimization already in progress [CRAB-E0332] (pid={pid}, started={started_at})")]
    OptimizeXorbsAlreadyInProgress { pid: u32, started_at: String },

    #[error("concurrent maintenance operation detected [CRAB-E0333]: {other} is running")]
    ConcurrentMaintenance { other: &'static str },

    #[error("price data missing [CRAB-E0340] for provider {provider} region {region}")]
    CostPricingMissing { provider: String, region: String },

    #[error(
        "inventory report [CRAB-E0341] for provider {provider} is stale: generated {report_at}, max staleness {max_hours}h"
    )]
    CostInventoryReportStale {
        provider: String,
        report_at: String,
        max_hours: u32,
    },

    // ─── Hydrate manifest ───
    #[error("manifest parse error [CRAB-E0400] at line {line}: {reason}")]
    ManifestParse { line: u32, reason: String },

    // ─── Hydrate prefetch profiles ───
    #[error("prefetch config error [CRAB-E0401]: {reason}")]
    PrefetchParse { reason: String },

    #[error("prefetch profile not found [CRAB-E0402]: {name}")]
    PrefetchProfileNotFound { name: String },

    // ─── Speculation DB ───
    #[error("speculation DB error [CRAB-E0410] at {path}: {source}")]
    SpeculationDb {
        path: std::path::PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    // ─── Gitoxide surface wrappers ───
    //
    // Each variant wraps a `gix-*` error type via `#[from]` so the
    // source chain is preserved end-to-end. The Display message
    // prefixes with `gix <crate>:` for grep-ability.
    //
    // Path deviations from `design.md §Error Mapping`:
    //   * `GixRef` wraps `gix_ref::file::find::Error` because
    //     `gix_ref` has no crate-level `Error` — the type is scoped
    //     per operation, and `file::find::Error` is what the
    //     `crab_git::ref_resolve` Interface surfaces.
    //   * `GixFilterHandshake` / `GixFilterRequest` paths include
    //     the `server` module (the design snippet skipped it); the
    //     real paths are `gix_filter::driver::process::server::
    //     {handshake,next_request}::Error`.
    //   * `GixRevwalk` wraps `gix_revwalk::graph::insert_parents::
    //     Error` — `gix_revwalk::graph::lookup::Error` named in the
    //     design snippet does not exist. `insert_parents::Error` is
    //     the broadest graph-traversal error exposed by the crate.
    //   * `gix_protocol::fetch::Error` is gated behind the
    //     `blocking-client` feature of `gix-protocol`; that feature
    //     is enabled in `Cargo.toml` so the wrap compiles.
    #[error("gix ref: {0} [CRAB-E0600]")]
    GixRef(#[from] gix_ref::file::find::Error),

    #[error("gix object: {0} [CRAB-E0601]")]
    GixObject(#[from] gix_object::decode::Error),

    #[error("gix pack: {0} [CRAB-E0602]")]
    GixPack(#[from] gix_pack::data::decode::Error),

    #[error("gix transport: {0} [CRAB-E0603]")]
    GixTransport(#[from] gix_transport::client::Error),

    #[error("gix protocol: {0} [CRAB-E0604]")]
    GixProtocol(#[from] gix_protocol::fetch::Error),

    // Two variants because `gix-filter` exposes two distinct error
    // modules — one for the capability handshake, one for per-
    // request read/write — and the call sites match on them
    // separately.
    #[error("gix filter handshake: {0} [CRAB-E0605]")]
    GixFilterHandshake(#[from] gix_filter::driver::process::server::handshake::Error),

    #[error("gix filter request: {0} [CRAB-E0606]")]
    GixFilterRequest(#[from] gix_filter::driver::process::server::next_request::Error),

    #[error("gix worktree: {0} [CRAB-E0607]")]
    GixWorktree(#[from] gix_worktree_state::checkout::Error),

    #[error("gix config: {0} [CRAB-E0608]")]
    GixConfig(#[from] gix_config::parse::Error),

    #[error("gix credentials: {0} [CRAB-E0609]")]
    GixCreds(#[from] gix_credentials::protocol::Error),

    #[error("gix status: {0} [CRAB-E060A]")]
    GixStatus(#[from] gix_status::index_as_worktree::Error),

    #[error("gix revwalk: {0} [CRAB-E060B]")]
    GixRevwalk(#[from] gix_revwalk::graph::insert_parents::Error),

    #[error("git tag discovery: {0} [CRAB-E060C]")]
    GitTag(#[from] crab_git::tag::TagPeelError),

    // ─── Shell completions ───
    #[error("invalid config key '{key}' [CRAB-E0134]: valid keys are {valid_keys}")]
    InvalidConfigKey { key: String, valid_keys: String },

    #[error(
        "unsupported shell '{shell}' [CRAB-E0135]: valid options are bash, zsh, fish, powershell"
    )]
    UnsupportedShell { shell: String },

    // ─── Pull command ───
    #[error(
        "merge conflict in {count} file(s) [CRAB-E0130]: resolve conflicts then run `crab hydrate`"
    )]
    PullConflict { count: usize, files: Vec<String> },

    #[error("remote '{remote}' unreachable [CRAB-E0131]: {reason}")]
    PullRemoteUnreachable { remote: String, reason: String },

    #[error("unadopt chunks missing [CRAB-E0132]: {count} file(s) cannot be restored from staging")]
    UnadoptChunksMissing { count: usize, files: Vec<String> },

    #[error(
        "nothing to undo [CRAB-E0133]: no reversible crab operation detected in staged changes"
    )]
    NothingToUndo,

    // ─── MetaDB (SlateDB-backed metadata index) ───
    //
    // `MetaDbError` is its own `thiserror` enum so inner variants carry
    // their own `CRAB-E05xx` codes. The outer wrapper is transparent —
    // display and source chain through to the inner variant.
    #[error(transparent)]
    MetaDb(#[from] MetaDbError),
}

impl From<crab_types::pointer::PointerParseError> for CrabError {
    fn from(error: crab_types::pointer::PointerParseError) -> Self {
        Self::Protocol(error.to_string())
    }
}

impl From<crab_xet::error::XetError> for CrabError {
    fn from(error: crab_xet::error::XetError) -> Self {
        match error {
            crab_xet::error::XetError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_xet::error::XetError::ShardReplayIo { source, .. } => Self::Io(source),
            crab_xet::error::XetError::ChunkNotFound { hash } => Self::ChunkNotFound { hash },
            crab_xet::error::XetError::IncompleteShardReconstruction {
                file_hash,
                path,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
            } => Self::IncompleteShardReconstruction {
                file_hash,
                path,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
            },
            error @ (crab_xet::error::XetError::Decompress { .. }
            | crab_xet::error::XetError::Compress { .. }
            | crab_xet::error::XetError::Layout { .. }
            | crab_xet::error::XetError::ShardFormat { .. }
            | crab_xet::error::XetError::Internal(_)) => Self::Internal(error.to_string()),
        }
    }
}

impl From<crab_cache::CacheError> for CrabError {
    fn from(error: crab_cache::CacheError) -> Self {
        match error {
            crab_cache::CacheError::Cancelled => Self::Cancelled,
            crab_cache::CacheError::Io(source) => Self::Io(source),
            error @ crab_cache::CacheError::UnsafeRoot { .. } => Self::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                error,
            )),
            error @ crab_cache::CacheError::BudgetConflict { .. } => Self::Configuration {
                key: "cache.max_bytes".into(),
                origin: error.to_string(),
            },
            error @ crab_cache::CacheError::InspectionTimeout { .. } => {
                Self::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, error))
            }
            crab_cache::CacheError::HashMismatch { requested, actual } => {
                Self::HashMismatch { requested, actual }
            }
            crab_cache::CacheError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_cache::CacheError::Index { path, source } => {
                Self::Internal(format!("cache index error at {path}: {source}"))
            }
            crab_cache::CacheError::ChunkNotFound { hash } => Self::ChunkNotFound { hash },
            crab_cache::CacheError::Xet { source } => Self::from(source),
            crab_cache::CacheError::XetChunkCache { path, source } => Self::Internal(format!(
                "failed to initialize xet chunk cache at {path}: {source}"
            )),
            crab_cache::CacheError::PrefetchParse { reason } => Self::PrefetchParse { reason },
            crab_cache::CacheError::PrefetchProfileNotFound { name } => {
                Self::PrefetchProfileNotFound { name }
            }
            crab_cache::CacheError::Internal(message) => Self::Internal(message),
            service @ (crab_cache::CacheError::Service { .. }
            | crab_cache::CacheError::ServiceRequestTimeout { .. }
            | crab_cache::CacheError::ServiceConnection { .. }
            | crab_cache::CacheError::ServiceRequest { .. }
            | crab_cache::CacheError::ReadCaCert { .. }
            | crab_cache::CacheError::InvalidCaCert { .. }
            | crab_cache::CacheError::MissingClientKey
            | crab_cache::CacheError::ReadClientCert { .. }
            | crab_cache::CacheError::ReadClientKey { .. }
            | crab_cache::CacheError::InvalidClientIdentity { .. }
            | crab_cache::CacheError::HttpClientBuild { .. }) => Self::CacheService {
                reason: service.to_string(),
            },
        }
    }
}

impl From<crab_cache_store::CacheStoreError> for CrabError {
    fn from(error: crab_cache_store::CacheStoreError) -> Self {
        match error {
            crab_cache_store::CacheStoreError::Cache(source) => Self::from(source),
            crab_cache_store::CacheStoreError::Storage(source) => Self::from(source),
            crab_cache_store::CacheStoreError::OriginIntegrity { path, source } => {
                Self::OriginIntegrity { path, source }
            }
        }
    }
}

impl From<crab_read::upload_pack_wire::WireError> for CrabError {
    fn from(error: crab_read::upload_pack_wire::WireError) -> Self {
        match error {
            crab_read::upload_pack_wire::WireError::Protocol(message) => Self::Protocol(message),
            crab_read::upload_pack_wire::WireError::Io(source) => Self::Io(source),
            crab_read::upload_pack_wire::WireError::Cancelled => Self::Cancelled,
        }
    }
}

impl From<crab_read::ReadError> for CrabError {
    fn from(error: crab_read::ReadError) -> Self {
        match error {
            crab_read::ReadError::Pointer(source) => Self::from(source),
            crab_read::ReadError::Cache(source) => Self::from(source),
            crab_read::ReadError::CacheStore(source) => Self::from(source),
            crab_read::ReadError::Storage(source) => Self::from(source),
            crab_read::ReadError::Metadata(source) => Self::from(source),
            crab_read::ReadError::RemoteGit(source) => Self::Protocol(source.to_string()),
            crab_read::ReadError::Xet(source) => Self::from(source),
            crab_read::ReadError::Io(source) => Self::Io(source),
            crab_read::ReadError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_read::ReadError::NotFound { path } => Self::NotFound { path },
            crab_read::ReadError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_read::ReadError::HashMismatch { requested, actual } => {
                Self::HashMismatch { requested, actual }
            }
            crab_read::ReadError::IncompleteShardReconstruction {
                file_hash,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
            } => Self::IncompleteShardReconstruction {
                file_hash,
                path: None,
                uncovered_chunks: usize::try_from(uncovered_chunks).unwrap_or(usize::MAX),
                example_chunk_hash,
                example_chunk_index,
            },
            crab_read::ReadError::Cancelled => Self::Cancelled,
            error @ (crab_read::ReadError::Availability { .. }
            | crab_read::ReadError::Runtime(_)
            | crab_read::ReadError::Reconstruction { .. }) => Self::Read(ReadFailure(error)),
            crab_read::ReadError::UnauthorizedObject => {
                Self::Protocol("requested object is outside the visible generation".to_owned())
            }
            crab_read::ReadError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<crab_staging::StagingError> for CrabError {
    fn from(error: crab_staging::StagingError) -> Self {
        match error {
            crab_staging::StagingError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_staging::StagingError::Io(source) => Self::Io(source),
            crab_staging::StagingError::StagingCorrupt(message) => Self::StagingCorrupt(message),
            crab_staging::StagingError::ShardReplayCorrupt { reason } => Self::CorruptObject {
                path: "Xet shard replay".to_owned(),
                reason,
            },
            crab_staging::StagingError::StagingLocked { holder_pid } => {
                Self::StagingLocked { holder_pid }
            }
            crab_staging::StagingError::ChunkNotFound { hash } => Self::ChunkNotFound { hash },
            crab_staging::StagingError::NotFound { path } => Self::NotFound { path },
            crab_staging::StagingError::HashMismatch { requested, actual } => {
                Self::HashMismatch { requested, actual }
            }
            crab_staging::StagingError::CrcMismatch { segment_id, offset } => {
                Self::CrcMismatch { segment_id, offset }
            }
            crab_staging::StagingError::Xet(source) => Self::from(source),
            crab_staging::StagingError::Cancelled => Self::Cancelled,
            crab_staging::StagingError::FileChangedDuringStaging {
                path,
                first_hash,
                second_hash,
                first_size,
                second_size,
            } => Self::FileChangedDuringStaging {
                path,
                first_hash,
                second_hash,
                first_size,
                second_size,
            },
            crab_staging::StagingError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<crab_vfs::VfsError> for CrabError {
    fn from(error: crab_vfs::VfsError) -> Self {
        match error {
            crab_vfs::VfsError::AuthFailed { path } => Self::AuthFailed { path },
            crab_vfs::VfsError::Cancelled => Self::Cancelled,
            crab_vfs::VfsError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_vfs::VfsError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_vfs::VfsError::Forbidden { path } => Self::Forbidden { path },
            crab_vfs::VfsError::HashMismatch { requested, actual } => {
                Self::HashMismatch { requested, actual }
            }
            crab_vfs::VfsError::Internal(message) => Self::Internal(message),
            crab_vfs::VfsError::Io(source) => Self::Io(source),
            crab_vfs::VfsError::NotFound { path } => Self::NotFound { path },
            crab_vfs::VfsError::Cache(source) => Self::from(source),
            crab_vfs::VfsError::Read(source) => Self::from(source),
            crab_vfs::VfsError::Staging(source) => Self::from(source),
            crab_vfs::VfsError::Storage(source) => Self::from(source),
            crab_vfs::VfsError::Url(source) => Self::from(source),
            crab_vfs::VfsError::Pointer(source) => Self::from(source),
            source @ (crab_vfs::VfsError::Database(_) | crab_vfs::VfsError::Json(_)) => {
                Self::Internal(source.to_string())
            }
        }
    }
}

impl From<crab_workflow::WorkflowError> for CrabError {
    fn from(error: crab_workflow::WorkflowError) -> Self {
        match error {
            crab_workflow::WorkflowError::Cancelled => Self::Cancelled,
            crab_workflow::WorkflowError::NetworkTransient(source) => {
                Self::NetworkTransient(source)
            }
            crab_workflow::WorkflowError::CasConflict {
                path,
                expected_etag,
            } => Self::CasConflict {
                path,
                expected_etag,
            },
            crab_workflow::WorkflowError::NotFound { path } => Self::NotFound { path },
            crab_workflow::WorkflowError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_workflow::WorkflowError::Storage(source) => Self::Storage(source),
            crab_workflow::WorkflowError::StorageDomain(source) => Self::from(source),
            crab_workflow::WorkflowError::GcFence(source) => Self::from(source),
            crab_workflow::WorkflowError::Internal(message) => Self::Internal(message),
            crab_workflow::WorkflowError::ExperimentIdInvalid { raw, reason } => {
                Self::WorkflowExperimentIdInvalid { raw, reason }
            }
            crab_workflow::WorkflowError::StageNameInvalid { name, reason } => {
                Self::WorkflowStageNameInvalid { name, reason }
            }
            crab_workflow::WorkflowError::ParamRefInvalid { key, reason } => Self::Configuration {
                key,
                origin: reason.to_owned(),
            },
            crab_workflow::WorkflowError::StageOutMalformed {
                stage,
                path,
                reason,
            } => Self::StageOutMalformed {
                stage,
                path,
                reason,
            },
            crab_workflow::WorkflowError::WorkflowDiscoveryAmbiguous { candidates } => {
                Self::WorkflowDiscoveryAmbiguous { candidates }
            }
            crab_workflow::WorkflowError::WorkflowStageNameInvalid { name, reason } => {
                Self::WorkflowStageNameInvalid { name, reason }
            }
            crab_workflow::WorkflowError::StageDepMissing { stage, path } => {
                Self::StageDepMissing { stage, path }
            }
            crab_workflow::WorkflowError::StageDepMalformed {
                stage,
                path,
                reason,
            } => Self::StageDepMalformed {
                stage,
                path,
                reason,
            },
            crab_workflow::WorkflowError::StageOutTooLarge {
                stage,
                path,
                size,
                limit,
            } => Self::StageOutTooLarge {
                stage,
                path,
                size,
                limit,
            },
            crab_workflow::WorkflowError::StageOutCountExceeded {
                stage,
                count,
                limit,
            } => Self::StageOutCountExceeded {
                stage,
                count,
                limit,
            },
            crab_workflow::WorkflowError::StageExecFailed { stage, exit_code } => {
                Self::StageExecFailed { stage, exit_code }
            }
            crab_workflow::WorkflowError::StageExecSignaled { stage, signal } => {
                Self::StageExecSignaled { stage, signal }
            }
            crab_workflow::WorkflowError::StageExecTimeout { stage, elapsed_ms } => {
                Self::StageExecTimeout { stage, elapsed_ms }
            }
            crab_workflow::WorkflowError::StageDiskFull { stage, path } => {
                Self::StageDiskFull { stage, path }
            }
            crab_workflow::WorkflowError::StageCacheMiss { stage, reason } => {
                Self::StageCacheMiss { stage, reason }
            }
            crab_workflow::WorkflowError::StageOverwriteConflict {
                stage,
                path,
                reason,
            } => Self::StageOverwriteConflict {
                stage,
                path,
                reason,
            },
            crab_workflow::WorkflowError::StageRemoteExecutionUnsupported => {
                Self::StageRemoteExecutionUnsupported
            }
            crab_workflow::WorkflowError::ExperimentCollision { id } => {
                Self::ExperimentCollision { id }
            }
            crab_workflow::WorkflowError::MetricsSchemaMismatch { path, source } => {
                Self::MetricsSchemaMismatch { path, source }
            }
            crab_workflow::WorkflowError::WorkflowJournalOpen { path, source } => {
                Self::WorkflowJournalOpen { path, source }
            }
            crab_workflow::WorkflowError::WorkflowJournalCorrupt { run_id, detail } => {
                Self::WorkflowJournalCorrupt { run_id, detail }
            }
            crab_workflow::WorkflowError::WorkflowJournalSchemaNewer {
                run_id,
                found,
                supported,
            } => Self::WorkflowJournalSchemaNewer {
                run_id,
                found,
                supported,
            },
            crab_workflow::WorkflowError::WorkflowStateTransitionIllegal { stage, from, to } => {
                Self::WorkflowStateTransitionIllegal { stage, from, to }
            }
            crab_workflow::WorkflowError::WorkflowLockTimeout { held_by, waited_ms } => {
                Self::WorkflowLockTimeout { held_by, waited_ms }
            }
            crab_workflow::WorkflowError::WorkflowHermeticViolation { stage, path } => {
                Self::WorkflowHermeticViolation { stage, path }
            }
            crab_workflow::WorkflowError::CacheEntrySchemaNewer {
                stage_hash,
                found,
                supported,
            } => Self::CacheEntrySchemaNewer {
                stage_hash,
                found,
                supported,
            },
            crab_workflow::WorkflowError::WorkflowExperimentMetadataSchemaNewer {
                id,
                found,
                supported,
            } => Self::WorkflowExperimentMetadataSchemaNewer {
                id,
                found,
                supported,
            },
            crab_workflow::WorkflowError::CacheEntryCorrupt {
                stage_hash,
                path,
                expected,
                actual,
            } => Self::CacheEntryCorrupt {
                stage_hash,
                path,
                expected,
                actual,
            },
            crab_workflow::WorkflowError::CacheEntryHashMismatch {
                manifest_hash,
                local_hash,
            } => Self::CacheEntryHashMismatch {
                manifest_hash,
                local_hash,
            },
            crab_workflow::WorkflowError::CacheEntryInvalid { stage_hash, detail } => {
                Self::CorruptObject {
                    path: format!("workflow stage cache/{stage_hash}"),
                    reason: detail,
                }
            }
            crab_workflow::WorkflowError::RemoteCacheReadonly => Self::RemoteCacheReadonly,
            crab_workflow::WorkflowError::JournalDiskFull { path } => {
                Self::JournalDiskFull { path }
            }
            crab_workflow::WorkflowError::WdirInvalid {
                stage,
                path: _,
                reason,
            } => Self::Configuration {
                key: format!("stage '{stage}' wdir"),
                origin: reason.to_owned(),
            },
            crab_workflow::WorkflowError::WorkflowCycle { stages } => {
                Self::WorkflowCycle { stages }
            }
            crab_workflow::WorkflowError::WorkflowUndefinedOut { consumer, out } => {
                Self::WorkflowUndefinedOut { consumer, out }
            }
            crab_workflow::WorkflowError::WorkflowDuplicateOutput {
                first,
                second,
                path,
            } => Self::WorkflowDuplicateOutput {
                first,
                second,
                path,
            },
            crab_workflow::WorkflowError::QueueEntryNotFound { path } => Self::NotFound { path },
            crab_workflow::WorkflowError::Io(source) => Self::Io(source),
            crab_workflow::WorkflowError::LockfileCanonicalizationFailed { path, source } => {
                Self::LockfileCanonicalizationFailed { path, source }
            }
            crab_workflow::WorkflowError::LockfileMergeConflict { path } => {
                Self::LockfileMergeConflict { path }
            }
            crab_workflow::WorkflowError::DvcYamlParse { source } => Self::WorkflowParse {
                path: PathBuf::from("dvc.yaml"),
                source,
            },
            crab_workflow::WorkflowError::DvcYamlSerialize { source } => Self::Configuration {
                key: format!("failed to serialize crab.yaml: {source}"),
                origin: "migrate".to_owned(),
            },
            crab_workflow::WorkflowError::DvcMigrationInvalid { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_workflow::WorkflowError::YamlParse { path, source } => {
                Self::WorkflowParse { path, source }
            }
            crab_workflow::WorkflowError::YamlInvalid { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_workflow::WorkflowError::ParamsInvalid { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_workflow::WorkflowError::TemplateInvalid { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_workflow::WorkflowError::TemplateUndefined { key, field, stage } => {
                Self::WorkflowTemplateUndefined { key, field, stage }
            }
            crab_workflow::WorkflowError::ForeachEmpty { stage } => {
                Self::WorkflowForeachEmpty { stage }
            }
            crab_workflow::WorkflowError::MatrixEmpty { stage, variable } => {
                Self::WorkflowMatrixEmpty { stage, variable }
            }
            crab_workflow::WorkflowError::WorkflowSelfLoop { stage, path } => {
                Self::WorkflowSelfLoop { stage, path }
            }
            crab_workflow::WorkflowError::WorkflowValidation {
                field,
                value,
                expected,
            } => Self::WorkflowValidationError {
                field,
                value,
                expected,
            },
            crab_workflow::WorkflowError::QueueEntrySerialize { .. }
            | crab_workflow::WorkflowError::QueueEntryMalformed { .. }
            | crab_workflow::WorkflowError::QueueEntryPathNoParent { .. }
            | crab_workflow::WorkflowError::QueueEntryPersist { .. }
            | crab_workflow::WorkflowError::LockfilePathNoParent { .. }
            | crab_workflow::WorkflowError::LockfileHashMalformed { .. }
            | crab_workflow::WorkflowError::GraphInvariant { .. } => {
                Self::Internal(error.to_string())
            }
        }
    }
}

impl From<crab_auth::error::AuthError> for CrabError {
    fn from(error: crab_auth::error::AuthError) -> Self {
        match error {
            crab_auth::error::AuthError::Io(source) => Self::Io(source),
            crab_auth::error::AuthError::NoCredentials => Self::NoCredentials,
            crab_auth::error::AuthError::InvalidJwt(message)
            | crab_auth::error::AuthError::InvalidCredentialResponse(message)
            | crab_auth::error::AuthError::InvalidProtectedPushFinalizeResponse(message)
            | crab_auth::error::AuthError::InvalidProtectedPushPrepareResponse(message)
            | crab_auth::error::AuthError::InvalidProtectedPushRefUpdate(message)
            | crab_auth::error::AuthError::InvalidProtectedPushRefUpdates(message)
            | crab_auth::error::AuthError::InvalidCrabAuthRequest(message)
            | crab_auth::error::AuthError::KeyStore(message) => Self::AuthFailed { path: message },
            crab_auth::error::AuthError::ManagedProfileNotFound { authority } => {
                Self::Configuration {
                    key: format!("no managed service profile is installed for {authority}"),
                    origin: "managed service profiles".into(),
                }
            }
            error @ crab_auth::error::AuthError::InvalidManagedContract(_) => Self::Configuration {
                key: error.to_string(),
                origin: "managed service profile".into(),
            },
            crab_auth::error::AuthError::UnsupportedManagedApiVersion {
                supported,
                advertised,
            } => Self::IncompatibleFormat {
                required: format!("managed API version in {supported:?}"),
                found: format!("managed API versions {advertised:?}"),
            },
            error @ crab_auth::error::AuthError::ProviderFeatureDisabled { .. } => {
                Self::Configuration {
                    key: "auth.provider".into(),
                    origin: error.to_string(),
                }
            }
            crab_auth::error::AuthError::CredentialsExpired(message) => {
                Self::AuthExpired { path: message }
            }
            error @ (crab_auth::error::AuthError::ParseCredentialResponse { .. }
            | crab_auth::error::AuthError::OidcRequest { .. }
            | crab_auth::error::AuthError::OidcRejected { .. }
            | crab_auth::error::AuthError::ParseOidcResponse { .. }
            | crab_auth::error::AuthError::ManagedDiscoveryRequest { .. }
            | crab_auth::error::AuthError::ManagedDiscoveryRejected { .. }
            | crab_auth::error::AuthError::ManagedDiscoveryUnavailable { .. }
            | crab_auth::error::AuthError::CrabAuthRequest { .. }
            | crab_auth::error::AuthError::CrabAuthRejected { .. }
            | crab_auth::error::AuthError::ParseCrabAuthResponse { .. }
            | crab_auth::error::AuthError::CrabAuthFailed { .. }
            | crab_auth::error::AuthError::AwsStsRequest { .. }
            | crab_auth::error::AuthError::AwsStsRejected(_)
            | crab_auth::error::AuthError::AzureRequest { .. }
            | crab_auth::error::AuthError::ParseAzureResponse { .. }
            | crab_auth::error::AuthError::AzureRejected(_)
            | crab_auth::error::AuthError::GcpRequest { .. }
            | crab_auth::error::AuthError::ParseGcpResponse { .. }
            | crab_auth::error::AuthError::GcpRejected(_)) => Self::AuthFailed {
                path: error.to_string(),
            },
            error @ crab_auth::error::AuthError::OidcRefreshExpired { .. } => Self::AuthExpired {
                path: error.to_string(),
            },
            crab_auth::error::AuthError::AzureConfig { key, reason } => Self::Configuration {
                key: key.into(),
                origin: reason.into(),
            },
            error @ (crab_auth::error::AuthError::SerializeTokens { .. }
            | crab_auth::error::AuthError::ParseCachedTokens { .. }
            | crab_auth::error::AuthError::SerializeServiceProfile { .. }
            | crab_auth::error::AuthError::ParseServiceProfile { .. }
            | crab_auth::error::AuthError::ParseManagedDiscovery { .. }
            | crab_auth::error::AuthError::JwtPayloadBase64 { .. }
            | crab_auth::error::AuthError::JwtPayloadJson { .. }
            | crab_auth::error::AuthError::Crypto { .. }) => Self::Internal(error.to_string()),
        }
    }
}

impl From<crab_auth::ManagedApiError> for CrabError {
    fn from(error: crab_auth::ManagedApiError) -> Self {
        match error {
            crab_auth::ManagedApiError::Service {
                status: 401, error, ..
            } => Self::AuthExpired {
                path: error.to_string(),
            },
            crab_auth::ManagedApiError::Service {
                status: 403, error, ..
            } => Self::AuthFailed {
                path: error.to_string(),
            },
            error @ (crab_auth::ManagedApiError::InvalidRequest { .. }
            | crab_auth::ManagedApiError::InvalidResponse { .. }
            | crab_auth::ManagedApiError::Contract(_)) => Self::Configuration {
                key: error.to_string(),
                origin: "managed service API".to_owned(),
            },
            crab_auth::ManagedApiError::ExpiredGrant => Self::AuthExpired {
                path: "managed transfer grant; retry the operation".to_owned(),
            },
            error => Self::AuthFailed {
                path: error.to_string(),
            },
        }
    }
}

impl From<crab_git::url::UrlError> for CrabError {
    fn from(error: crab_git::url::UrlError) -> Self {
        match error {
            crab_git::url::UrlError::InvalidCrabUrl { origin, message } => Self::Configuration {
                key: format!("invalid crab URL: {message}"),
                origin,
            },
            crab_git::url::UrlError::ExpectedCrabScheme { origin, actual } => Self::Configuration {
                key: format!("expected crab:// scheme, got {actual}"),
                origin,
            },
            crab_git::url::UrlError::MissingBucket { origin } => Self::Configuration {
                key: "missing bucket (host) in crab URL".into(),
                origin,
            },
            crab_git::url::UrlError::MissingRepoPath { origin } => Self::Configuration {
                key: "missing repo path in crab URL".into(),
                origin,
            },
            crab_git::url::UrlError::InvalidManagedRepository { authority, message } => {
                Self::Configuration {
                    key: format!("invalid managed repository URL: {message}"),
                    origin: authority,
                }
            }
            crab_git::url::UrlError::ManagedServiceNotEnabled {
                authority,
                organization,
                repository,
            } => Self::Configuration {
                key: format!(
                    "managed repository support is not enabled for {organization}/{repository}"
                ),
                origin: authority,
            },
            crab_git::url::UrlError::EmptyUrl { origin } => Self::Configuration {
                key: "empty URL".into(),
                origin,
            },
            crab_git::url::UrlError::MissingSchemeSeparator { origin, input } => {
                Self::Configuration {
                    key: format!("missing scheme separator in URL: {input}"),
                    origin,
                }
            }
            crab_git::url::UrlError::UnsupportedScheme { origin, scheme } => Self::Configuration {
                key: format!(
                    "unsupported URL scheme {scheme:?}: \
                         expected s3, gs, az, azure, file, or crab"
                ),
                origin,
            },
            crab_git::url::UrlError::UnsupportedRepositoryScheme { origin, scheme } => {
                Self::Configuration {
                    key: format!(
                        "unsupported repository URL scheme {scheme:?}: \
                         expected crab, s3, gs, gcs, az, or azure"
                    ),
                    origin,
                }
            }
            crab_git::url::UrlError::MissingObjectBucket { origin } => Self::Configuration {
                key: "missing bucket in URL".into(),
                origin,
            },
            crab_git::url::UrlError::InvalidRepositoryBucket { origin, message } => {
                Self::Configuration {
                    key: format!("invalid repository bucket: {message}"),
                    origin,
                }
            }
            crab_git::url::UrlError::InvalidRepositoryPrefix { origin, message } => {
                Self::Configuration {
                    key: format!("invalid repository prefix: {message}"),
                    origin,
                }
            }
            crab_git::url::UrlError::FileMissingAbsolutePath { origin } => Self::Configuration {
                key: "file:// URL missing absolute path".into(),
                origin,
            },
            crab_git::url::UrlError::ImportSourceMustBeRaw { url } => {
                Self::ImportSourceMustBeRaw { url }
            }
            crab_git::url::UrlError::ExpectedAzureObjectUrl { url } => Self::Configuration {
                key: "expected Azure object URL".into(),
                origin: url,
            },
            crab_git::url::UrlError::MissingAzureContainer { url } => Self::Configuration {
                key: "Azure object URL missing container".into(),
                origin: url,
            },
        }
    }
}

impl From<crab_git::lfs_pointer::LfsPointerError> for CrabError {
    fn from(error: crab_git::lfs_pointer::LfsPointerError) -> Self {
        Self::InvalidLfsPointer {
            reason: error.reason,
        }
    }
}

impl From<crab_git::pack::PackError> for CrabError {
    fn from(error: crab_git::pack::PackError) -> Self {
        match error {
            crab_git::pack::PackError::TooShort { .. } => Self::PackIntegrity {
                expected: String::new(),
                computed: String::new(),
            },
            crab_git::pack::PackError::Sha1Mismatch { expected, computed } => {
                Self::PackIntegrity { expected, computed }
            }
            crab_git::pack::PackError::PackTooLarge { size, limit } => {
                Self::PackTooLarge { size, limit }
            }
            crab_git::pack::PackError::Io { source, .. } => Self::Io(source),
            crab_git::pack::PackError::ReverseIndex { source } => source.into(),
            crab_git::pack::PackError::InvalidPackFile { .. } => Self::PackIntegrity {
                expected: String::new(),
                computed: String::new(),
            },
            error => Self::Internal(error.to_string()),
        }
    }
}

impl From<crab_git::pack_locator::PackLocatorError> for CrabError {
    fn from(error: crab_git::pack_locator::PackLocatorError) -> Self {
        use crab_git::pack_locator::PackLocatorError;

        match &error {
            PackLocatorError::IndexOpen {
                source: gix_pack::index::init::Error::Io { source, .. },
                ..
            }
            | PackLocatorError::ReverseIndexIo { source, .. } => {
                Self::Io(std::io::Error::new(source.kind(), error))
            }
            PackLocatorError::IndexChecksum {
                source: gix_pack::index::verify::checksum::Error::Interrupted,
                ..
            } => Self::Cancelled,
            _ => Self::GitPackCorrupt(error),
        }
    }
}

impl From<crab_git::repack::RepackError> for CrabError {
    fn from(error: crab_git::repack::RepackError) -> Self {
        match error {
            crab_git::repack::RepackError::Io { source, .. } => Self::Io(source),
            crab_git::repack::RepackError::Pack { source } => source.into(),
            crab_git::repack::RepackError::SourceIntegrity { pack_id, reason } => {
                Self::CorruptObject {
                    path: pack_id,
                    reason,
                }
            }
            crab_git::repack::RepackError::EmptyRefs => {
                Self::Internal("cannot repack a repository without refs".to_owned())
            }
            crab_git::repack::RepackError::Locator { source } => source.into(),
            crab_git::repack::RepackError::Git { operation, status } => {
                Self::Internal(format!("{operation} failed with {status}"))
            }
            _ => Self::Internal(error.to_string()),
        }
    }
}

impl From<crab_git::ref_resolve::RefResolveError> for CrabError {
    fn from(error: crab_git::ref_resolve::RefResolveError) -> Self {
        match error {
            crab_git::ref_resolve::RefResolveError::TypedRefStore { source } => {
                Self::GixRef(source)
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_git::pointer_ref::PointerRefError> for CrabError {
    fn from(error: crab_git::pointer_ref::PointerRefError) -> Self {
        match error {
            crab_git::pointer_ref::PointerRefError::NotFound { refspec } => {
                Self::NotFound { path: refspec }
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_git::worktree::WorktreeError> for CrabError {
    fn from(error: crab_git::worktree::WorktreeError) -> Self {
        match error {
            crab_git::worktree::WorktreeError::Io(source) => Self::Io(source),
            error @ crab_git::worktree::WorktreeError::Discover { .. } => {
                Self::Protocol(error.to_string())
            }
            crab_git::worktree::WorktreeError::Protocol(message) => Self::Protocol(message),
        }
    }
}

impl From<crab_git::walk::WalkError> for CrabError {
    fn from(error: crab_git::walk::WalkError) -> Self {
        match error {
            crab_git::walk::WalkError::ObjectsDirectoryNotFound { path } => {
                Self::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("git objects directory not found: {path}"),
                ))
            }
            crab_git::walk::WalkError::BeyondShallowBoundary { oid } => {
                Self::BeyondShallowBoundary { oid }
            }
            crab_git::walk::WalkError::LimitExceeded { actual, maximum } => Self::Protocol(
                format!("Git visibility proof exceeds {maximum} objects (observed {actual})"),
            ),
            crab_git::walk::WalkError::Cancelled => Self::Cancelled,
            error @ crab_git::walk::WalkError::LookupLimitExceeded { .. } => {
                Self::Protocol(error.to_string())
            }
            error @ crab_git::walk::WalkError::Git { .. } => Self::Io(std::io::Error::other(error)),
        }
    }
}

impl From<crab_git::refname::RefNameError> for CrabError {
    fn from(error: crab_git::refname::RefNameError) -> Self {
        Self::Protocol(error.to_string())
    }
}

impl From<crab_git::odb_adapter::OdbError> for CrabError {
    fn from(error: crab_git::odb_adapter::OdbError) -> Self {
        match error {
            crab_git::odb_adapter::OdbError::ObjectsDirectoryNotFound { path } => {
                Self::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("git objects directory not found: {path}"),
                ))
            }
            error @ (crab_git::odb_adapter::OdbError::Git { .. }
            | crab_git::odb_adapter::OdbError::Poisoned { .. }) => {
                Self::Internal(error.to_string())
            }
        }
    }
}

#[cfg(feature = "gix-facade")]
impl From<crab_git::facade::FacadeError> for CrabError {
    fn from(error: crab_git::facade::FacadeError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<crab_storage::error::StorageError> for CrabError {
    fn from(error: crab_storage::error::StorageError) -> Self {
        match error {
            crab_storage::error::StorageError::NetworkTransient { source } => {
                Self::NetworkTransient(source)
            }
            crab_storage::error::StorageError::Throttled { retry_after } => {
                Self::Throttled { retry_after }
            }
            crab_storage::error::StorageError::StateConflict { path } => Self::CasConflict {
                path,
                expected_etag: None,
            },
            crab_storage::error::StorageError::NotFound { path } => Self::NotFound { path },
            crab_storage::error::StorageError::InvalidHash { hash } => {
                Self::Internal(format!("invalid storage object hash: {hash}"))
            }
            crab_storage::error::StorageError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_storage::error::StorageError::UnsupportedProvider { provider } => {
                Self::Configuration {
                    key: format!(
                        "unsupported storage provider for object-store construction: {provider:?}"
                    ),
                    origin: "storage provider".into(),
                }
            }
            crab_storage::error::StorageError::InvalidStaticEnvTarget { target, reason } => {
                Self::Configuration {
                    key: target,
                    origin: reason,
                }
            }
            crab_storage::error::StorageError::StaticEnvProviderMismatch {
                expected,
                actual,
                bucket,
            } => Self::Configuration {
                key: format!("static-env provider for {bucket}"),
                origin: format!("expected {expected:?}, got {actual:?}"),
            },
            crab_storage::error::StorageError::ProviderConfig {
                provider,
                bucket,
                source,
            } => Self::Configuration {
                key: format!("failed to build {provider:?} object store: {source}"),
                origin: bucket,
            },
            crab_storage::error::StorageError::InvalidObjectStoreUrl { url, source } => {
                Self::Configuration {
                    key: format!("invalid object-store URL {url:?}: {source}"),
                    origin: "object-store URL".into(),
                }
            }
            crab_storage::error::StorageError::UrlStoreConfig { url, source } => {
                Self::Configuration {
                    key: format!("failed to build object store from URL {url:?}: {source}"),
                    origin: "object-store URL".into(),
                }
            }
            crab_storage::error::StorageError::Forbidden { path } => Self::Forbidden { path },
            crab_storage::error::StorageError::NoCredentials => Self::NoCredentials,
            crab_storage::error::StorageError::AuthFailed { path } => Self::AuthFailed { path },
            crab_storage::error::StorageError::AuthExpired { path } => Self::AuthExpired { path },
            crab_storage::error::StorageError::Io { source } => Self::Io(source),
            crab_storage::error::StorageError::Cancelled => Self::Cancelled,
            crab_storage::error::StorageError::MultipartJournal { source, .. } => {
                Self::Storage(object_store::Error::Generic {
                    store: "multipart journal",
                    source,
                })
            }
            crab_storage::error::StorageError::NotSupported { source }
            | crab_storage::error::StorageError::ObjectStore { source } => Self::Storage(source),
            crab_storage::error::StorageError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<crab_write::WriteError> for CrabError {
    fn from(error: crab_write::WriteError) -> Self {
        match error {
            crab_write::WriteError::RefChanged { path, .. } => Self::CasConflict {
                path,
                expected_etag: None,
            },
            crab_write::WriteError::Storage(source) => Self::from(source),
            crab_write::WriteError::Coordination(source) => Self::from(source),
            crab_write::WriteError::Metadata(source) => Self::from(source),
            crab_write::WriteError::Git(source) => Self::from(source),
            crab_write::WriteError::Io(source) => Self::Io(source),
            crab_write::WriteError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_write::WriteError::Internal(message) => Self::Internal(message),
            crab_write::WriteError::Cancelled => Self::Cancelled,
            error @ (crab_write::WriteError::Worker(_)
            | crab_write::WriteError::PackIdentity { .. }
            | crab_write::WriteError::ManifestHash { .. }) => {
                Self::Io(std::io::Error::other(error))
            }
        }
    }
}

impl From<crab_lfs::LfsError> for CrabError {
    fn from(error: crab_lfs::LfsError) -> Self {
        match error {
            crab_lfs::LfsError::ObjectCorrupt { oid } => Self::LfsObjectCorrupt { oid },
            crab_lfs::LfsError::ObjectMissing { oid } => Self::LfsObjectMissing { oid },
            crab_lfs::LfsError::Io { source } => Self::Io(source),
            crab_lfs::LfsError::Storage { source } => Self::from(source),
        }
    }
}

impl From<crab_metadata::error::MetadataError> for CrabError {
    fn from(error: crab_metadata::error::MetadataError) -> Self {
        match error {
            crab_metadata::error::MetadataError::FileLookupLimit { resource, maximum } => {
                Self::Protocol(format!("file lookup exceeds {resource} limit ({maximum})"))
            }
            error @ (crab_metadata::error::MetadataError::FileLookupAdmission { .. }
            | crab_metadata::error::MetadataError::FileLookupWorker { .. }
            | crab_metadata::error::MetadataError::RefJournalCommitUncertain { .. }) => {
                Self::Io(std::io::Error::other(error))
            }
            crab_metadata::error::MetadataError::Io { source } => Self::Io(source),
            crab_metadata::error::MetadataError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_metadata::error::MetadataError::Xet { source } => Self::from(source),
            crab_metadata::error::MetadataError::Sqlite { context, source } => {
                Self::Internal(format!("{context}: {source}"))
            }
            crab_metadata::error::MetadataError::Storage { source } => Self::from(source),
            crab_metadata::error::MetadataError::ObjectStore { source } => Self::Storage(source),
            crab_metadata::error::MetadataError::SlateDbOpen { db, path, source } => {
                Self::MetaDb(MetaDbError::Open { db, path, source })
            }
            crab_metadata::error::MetadataError::SlateDbRead { db, source } => {
                Self::MetaDb(MetaDbError::Read {
                    db,
                    prefix: String::from("<content>"),
                    source,
                })
            }
            crab_metadata::error::MetadataError::SlateDbWrite { db, source } => {
                Self::MetaDb(MetaDbError::Write { db, source })
            }
            crab_metadata::error::MetadataError::SlateDbClose { db, source } => {
                Self::MetaDb(MetaDbError::Close { db, source })
            }
            crab_metadata::error::MetadataError::SlateDbOperationAndClose {
                db,
                operation,
                close,
            } => Self::MetaDb(MetaDbError::OperationAndClose {
                db,
                operation,
                close,
            }),
            crab_metadata::error::MetadataError::ManifestCasConflict {
                path,
                expected_etag,
            } => Self::CasConflict {
                path,
                expected_etag,
            },
            crab_metadata::error::MetadataError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<crab_coordination::error::CoordinationError> for CrabError {
    fn from(error: crab_coordination::error::CoordinationError) -> Self {
        match error {
            crab_coordination::error::CoordinationError::ObjectStore { path, source } => {
                Self::from(crab_storage::map_object_store_error(source, &path))
            }
            crab_coordination::error::CoordinationError::CasConflict {
                path,
                expected_etag,
            } => Self::CasConflict {
                path,
                expected_etag,
            },
            crab_coordination::error::CoordinationError::NonFastForward {
                ref_name,
                have,
                want,
            } => Self::NonFastForward {
                ref_name,
                have,
                want,
            },
            crab_coordination::error::CoordinationError::NotFound { path } => {
                Self::NotFound { path }
            }
            crab_coordination::error::CoordinationError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            crab_coordination::error::CoordinationError::RetryDeadline { path, source } => {
                Self::Configuration {
                    key: path,
                    origin: format!("coordination retry deadline exceeded: {source}"),
                }
            }
            crab_coordination::error::CoordinationError::Serialize {
                key,
                context,
                source,
            } => Self::Configuration {
                key,
                origin: format!("{context}: {source}"),
            },
            crab_coordination::error::CoordinationError::PushLockHeld {
                ref_name,
                holder,
                expires_at_unix,
            } => Self::PushLockHeld {
                ref_name,
                holder,
                expires_at_unix,
            },
            crab_coordination::error::CoordinationError::MalformedPushLock { path, source } => {
                Self::CorruptObject {
                    path,
                    reason: source.to_string(),
                }
            }
            crab_coordination::error::CoordinationError::GcFenceHeld { domain, holder, .. } => {
                Self::PushLockHeld {
                    ref_name: format!("gc-fence/{domain}"),
                    holder,
                    expires_at_unix: None,
                }
            }
            crab_coordination::error::CoordinationError::GcFenceLost { domain, holder } => {
                Self::Configuration {
                    key: domain,
                    origin: format!("GC fence lease lost for holder {holder}"),
                }
            }
            crab_coordination::error::CoordinationError::GcFenceMalformed { path, reason } => {
                Self::CorruptObject { path, reason }
            }
        }
    }
}

/// Errors raised by the two-database SlateDB metadata index layer
/// (per-repo `file_index_db` + globally shared `chunk_index_db`).
///
/// Each variant carries its own stable `CRAB-E05##` code so structured
/// output and the error catalog can pivot on the specific failure mode.
/// Rolls up into [`CrabError::MetaDb`] for propagation.
#[derive(thiserror::Error, Debug)]
pub enum MetaDbError {
    #[error("metadb open failed [CRAB-E0500] for {db} at {path}: {source}")]
    Open {
        db: String,
        path: String,
        #[source]
        source: slatedb::Error,
    },

    #[error("metadb close failed [CRAB-E0501] for {db}: {source}")]
    Close {
        db: String,
        #[source]
        source: slatedb::Error,
    },

    #[error("metadb read failed [CRAB-E0502] for {db} (prefix={prefix}): {source}")]
    Read {
        db: String,
        prefix: String,
        #[source]
        source: slatedb::Error,
    },

    #[error("metadb write failed [CRAB-E0503] for {db}: {source}")]
    Write {
        db: String,
        #[source]
        source: slatedb::Error,
    },

    // CRAB-E0504 — reserved. Formerly `NotBootstrapped`, removed because
    // the SlateDB metadata layer does not have a separate table bootstrap
    // step: after canonical repository init, fresh databases are created by
    // `slatedb::Db::open`. Do not reuse this code for a different error.
    #[error(
        "metadb {db} is not canonical v1 [CRAB-E0505] (found {found:?}); reset this isolated development repository and rebuild metadata"
    )]
    UnsupportedFormat { db: String, found: Option<u32> },

    #[error("file {file_hash} not found in file_index_db [CRAB-E0506]")]
    FileNotFoundInFileIndexDb { file_hash: String },

    #[error("corrupt value in {db} at key {key} [CRAB-E0507]: {reason}")]
    CorruptValue {
        db: String,
        key: String,
        reason: String,
    },

    #[error("metadb already closed [CRAB-E0508]")]
    AlreadyClosed,

    #[error("metadb {op} rejected [CRAB-E0509] for {db}: handle opened in read-only mode")]
    ReadOnly {
        db: String,
        /// The write-path operation the caller attempted ("write",
        /// "commit", "bump_gc_generation", …). Used for log triage.
        op: &'static str,
    },

    #[error(
        "metadb {db} at {path} is uninitialized [CRAB-E050A]; read-only open against a never-written database"
    )]
    ReadOnlyUninitialized { db: String, path: String },

    #[error(
        "metadb operation failed [CRAB-E050B] for {db}: {operation}; close also failed: {close}"
    )]
    OperationAndClose {
        db: String,
        #[source]
        operation: Box<crab_metadata::error::MetadataError>,
        close: slatedb::Error,
    },
}

impl CrabError {
    /// Whether this error means a read-only MetaDB open found no manifest.
    ///
    /// Fresh object-store repos legitimately have no `file_index_db`
    /// or `chunk_index_db` manifest until the first writer commits.
    /// Read-only planning/proof callers can map that state to an
    /// empty index while preserving fail-loud handling for corruption.
    pub(crate) fn is_metadb_read_only_uninitialized(&self) -> bool {
        matches!(
            self,
            Self::MetaDb(MetaDbError::ReadOnlyUninitialized { .. })
        )
    }

    /// Map this error to a numeric process exit code.
    ///
    /// Exit codes are stable and documented in the CLI surface design:
    /// 0 success, 1 user/general, 2 non-fast-forward, 3 CAS exhaustion,
    /// 4 corrupt, 5 I/O, 6 config, 7 credentials, 8 incompatible,
    /// 9 internal, 10 cancelled.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Read(error) => error.exit_code(),
            Self::HydrationFailed { source, .. } => source.exit_code(),
            Self::NonFastForward { .. } => 2,

            Self::CasConflict { .. }
            | Self::RefAlreadyExists { .. }
            | Self::PushLockHeld { .. }
            | Self::PushPartialOutcome { .. }
            | Self::PushIntegrationFailed { .. }
            | Self::FileChangedDuringStaging { .. }
            | Self::StageCacheMiss { .. }
            | Self::TierLifecycleConflict { .. }
            | Self::OptimizeXorbsAlreadyInProgress { .. }
            | Self::ConcurrentMaintenance { .. } => 3,

            Self::CorruptObject { .. }
            | Self::GitPackCorrupt(_)
            | Self::OriginIntegrity { .. }
            | Self::ChunkNotFound { .. }
            | Self::HashMismatch { .. }
            | Self::CrcMismatch { .. }
            | Self::PackIntegrity { .. }
            | Self::IncompleteShardReconstruction { .. }
            | Self::PointerMissingStaging { .. }
            | Self::PushConnectivityMissing { .. }
            | Self::PushMalformedObject { .. }
            | Self::FetchMalformedObject { .. }
            | Self::StagingCorrupt(_)
            | Self::GitTag(_)
            | Self::LfsObjectCorrupt { .. }
            | Self::LockfileCanonicalizationFailed { .. }
            | Self::MetricsSchemaMismatch { .. }
            | Self::WorkflowJournalCorrupt { .. }
            | Self::WorkflowJournalSchemaNewer { .. }
            | Self::WorkflowResumeFilesystemDrift { .. }
            | Self::CacheEntrySchemaNewer { .. }
            | Self::CacheEntryCorrupt { .. }
            | Self::CacheEntryHashMismatch { .. }
            | Self::WorkflowExperimentMetadataSchemaNewer { .. }
            | Self::OptimizeXorbsCorruptSource { .. } => 4,

            Self::Io(_)
            | Self::Storage(_)
            | Self::StageOverwriteConflict { .. }
            | Self::LockfileMergeConflict { .. }
            | Self::WorkflowLockTimeout { .. }
            | Self::WorkflowStateTransitionIllegal { .. }
            | Self::GixRef(_)
            | Self::GixObject(_)
            | Self::GixPack(_)
            | Self::GixTransport(_)
            | Self::GixProtocol(_)
            | Self::GixFilterHandshake(_)
            | Self::GixFilterRequest(_)
            | Self::GixWorktree(_)
            | Self::GixCreds(_)
            | Self::GixStatus(_)
            | Self::GixRevwalk(_) => 5,

            Self::Configuration { .. }
            | Self::InvalidPattern(_)
            | Self::WorkflowParse { .. }
            | Self::WorkflowCycle { .. }
            | Self::WorkflowUndefinedOut { .. }
            | Self::WorkflowStageNameInvalid { .. }
            | Self::WorkflowDiscoveryAmbiguous { .. }
            | Self::WorkflowDisabled
            | Self::StageRemoteExecutionUnsupported
            | Self::StageHermeticNotImplemented { .. }
            | Self::WorkflowDuplicateOutput { .. }
            | Self::WorkflowExperimentIdInvalid { .. }
            | Self::WorkflowValidationError { .. }
            | Self::WorkflowSelfLoop { .. }
            | Self::WorkflowTemplateUndefined { .. }
            | Self::WorkflowForeachEmpty { .. }
            | Self::WorkflowMatrixEmpty { .. }
            | Self::RemoteCacheReadonly
            | Self::UnsupportedShell { .. }
            | Self::InvalidConfigKey { .. }
            | Self::PullConflict { .. }
            | Self::UnadoptChunksMissing { .. }
            | Self::NothingToUndo
            | Self::GixConfig(_) => 6,

            Self::NoCredentials
            | Self::AuthFailed { .. }
            | Self::AuthExpired { .. }
            | Self::Forbidden { .. }
            | Self::TierApplyUnauthorized { .. } => 7,

            Self::IncompatibleFormat { .. } => 8,

            Self::Internal(_) => 9,

            Self::Cancelled => 10,

            Self::ManagedRepository { diagnostic } => match diagnostic {
                crab_auth_store::ManagedRepositoryDiagnostic::MalformedLocator
                | crab_auth_store::ManagedRepositoryDiagnostic::MissingProfile { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::ActiveProfileMissing
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidProfile { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::DiscoveryFailed { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidBearer
                | crab_auth_store::ManagedRepositoryDiagnostic::Inactive { .. } => 6,
                crab_auth_store::ManagedRepositoryDiagnostic::IncompatibleApi { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidServiceResponse { .. } => 8,
                crab_auth_store::ManagedRepositoryDiagnostic::LoginRequired { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::Forbidden { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::ExpiredGrant { .. } => 7,
                crab_auth_store::ManagedRepositoryDiagnostic::Cancelled => 10,
                crab_auth_store::ManagedRepositoryDiagnostic::NotFound { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable { .. } => 1,
            },

            // General user/operational errors.
            Self::Protocol(_)
            | Self::NotFound { .. }
            | Self::InsufficientSpace { .. }
            | Self::Throttled { .. }
            | Self::NetworkTransient(_)
            | Self::StagingLocked { .. }
            | Self::BeyondShallowBoundary { .. }
            | Self::PackTooLarge { .. }
            | Self::FetchNotAllowed { .. }
            | Self::FetchTooLarge { .. }
            | Self::InvalidLfsPointer { .. }
            | Self::LfsObjectMissing { .. }
            | Self::LfsLockConflict { .. }
            | Self::LfsTransferProtocol(_)
            | Self::LfsMigrationFailed { .. }
            | Self::LfsUnsupported { .. }
            | Self::CacheService { .. }
            | Self::ImportSourceMustBeRaw { .. }
            | Self::ImportSchemeMismatch { .. }
            | Self::ImportPlanMismatch { .. }
            | Self::ImportNoJournal { .. }
            | Self::ImportVersioningUnavailable { .. }
            | Self::ImportCommitCeilingExceeded { .. }
            | Self::ImportInvalidHistoryRange { .. }
            | Self::ImportTargetNotEmpty { .. }
            | Self::ImportSourceIsCrabRepo { .. }
            | Self::ImportLfsSourceUnsupported { .. }
            | Self::ImportLfsStoreNotFound { .. }
            | Self::ImportPrefixCollision { .. }
            | Self::ImportMissingGitIdentity
            | Self::ImportRemoteExists { .. }
            | Self::StageDepMissing { .. }
            | Self::StageDepMalformed { .. }
            | Self::StageOutMalformed { .. }
            | Self::StageOutTooLarge { .. }
            | Self::StageOutCountExceeded { .. }
            | Self::StageEnvMissing { .. }
            | Self::StageExecFailed { .. }
            | Self::StageExecSignaled { .. }
            | Self::StageExecTimeout { .. }
            | Self::StageDiskFull { .. }
            | Self::StageRetryExhausted { .. }
            | Self::StageSideEffectsRetryLimit { .. }
            | Self::StageSideEffectHookFailed { .. }
            | Self::LockfileStale { .. }
            | Self::ExperimentNotFound { .. }
            | Self::ExperimentCollision { .. }
            | Self::WorkflowJournalOpen { .. }
            | Self::WorkflowHermeticViolation { .. }
            | Self::JournalDiskFull { .. }
            | Self::TierProviderUnsupported { .. }
            | Self::ArchiveRestoreRequired { .. }
            | Self::ArchiveRestoreTimeout { .. }
            | Self::RestoreTierUnsupported { .. }
            | Self::GcEarlyDeleteBlocked { .. }
            | Self::ObjectLockedRetention { .. }
            | Self::GcPartialFailure { .. }
            | Self::OptimizeXorbsProfileOutOfRange { .. }
            | Self::CostPricingMissing { .. }
            | Self::CostInventoryReportStale { .. }
            | Self::ManifestParse { .. }
            | Self::PrefetchParse { .. }
            | Self::PrefetchProfileNotFound { .. }
            | Self::SpeculationDb { .. }
            | Self::PullRemoteUnreachable { .. } => 1,

            Self::MetaDb(inner) => match inner {
                MetaDbError::FileNotFoundInFileIndexDb { .. } => 1,
                MetaDbError::CorruptValue { .. } => 4,
                MetaDbError::Open { .. }
                | MetaDbError::Close { .. }
                | MetaDbError::Read { .. }
                | MetaDbError::Write { .. }
                | MetaDbError::OperationAndClose { .. }
                | MetaDbError::AlreadyClosed
                | MetaDbError::ReadOnly { .. }
                | MetaDbError::ReadOnlyUninitialized { .. } => 5,
                MetaDbError::UnsupportedFormat { .. } => 6,
            },
        }
    }

    /// Stable error code for structured output (`CRAB-E####`).
    ///
    /// Exhaustive — no catch-all. Adding a variant without a code arm
    /// is a compile error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Read(error) => error.code(),
            Self::HydrationFailed { source, .. } => source.code(),
            Self::NetworkTransient(_) => "CRAB-E0001",
            Self::Throttled { .. } => "CRAB-E0002",
            Self::CasConflict { .. } => "CRAB-E0010",
            Self::RefAlreadyExists { .. } => "CRAB-E0011",
            Self::PushLockHeld { .. } => "CRAB-E0012",
            Self::NonFastForward { .. } => "CRAB-E0017",
            Self::CorruptObject { .. } | Self::GitPackCorrupt(_) | Self::OriginIntegrity { .. } => {
                "CRAB-E0020"
            }
            Self::ChunkNotFound { .. } => "CRAB-E0021",
            Self::NotFound { .. } => "CRAB-E0030",
            Self::Forbidden { .. } => "CRAB-E0031",
            Self::NoCredentials => "CRAB-E0040",
            Self::InsufficientSpace { .. } => "CRAB-E0041",
            Self::AuthFailed { .. } => "CRAB-E0042",
            Self::AuthExpired { .. } => "CRAB-E0043",
            Self::Configuration { .. } => "CRAB-E0050",
            Self::IncompatibleFormat { .. } => "CRAB-E0051",
            Self::InvalidPattern(_) => "CRAB-E0052",
            Self::Protocol(_) => "CRAB-E0060",
            Self::Io(_) => "CRAB-E0070",
            Self::Storage(_) => "CRAB-E0071",
            Self::StagingCorrupt(_) => "CRAB-E0080",
            Self::StagingLocked { .. } => "CRAB-E0081",
            Self::HashMismatch { .. } => "CRAB-E0082",
            Self::CrcMismatch { .. } => "CRAB-E0083",
            Self::PackIntegrity { .. } => "CRAB-E0084",
            Self::IncompleteShardReconstruction { .. } => "CRAB-E0085",
            Self::PointerMissingStaging { .. } => "CRAB-E0086",
            Self::PushConnectivityMissing { .. } => "CRAB-E0087",
            Self::PushPartialOutcome { .. } => "CRAB-E0088",
            Self::FileChangedDuringStaging { .. } => "CRAB-E0089",
            Self::Cancelled => "CRAB-E0090",
            Self::BeyondShallowBoundary { .. } => "CRAB-E0091",
            Self::PackTooLarge { .. } => "CRAB-E0092",
            Self::PushMalformedObject { .. } => "CRAB-E0093",
            Self::FetchNotAllowed { .. } => "CRAB-E0094",
            Self::FetchTooLarge { .. } => "CRAB-E0095",
            Self::FetchMalformedObject { .. } => "CRAB-E0096",
            Self::PushIntegrationFailed { .. } => "CRAB-E0097",
            Self::Internal(_) => "CRAB-E0099",
            Self::InvalidLfsPointer { .. } => "CRAB-E0100",
            Self::LfsObjectCorrupt { .. } => "CRAB-E0101",
            Self::LfsObjectMissing { .. } => "CRAB-E0102",
            Self::LfsLockConflict { .. } => "CRAB-E0103",
            Self::LfsTransferProtocol(_) => "CRAB-E0104",
            Self::LfsMigrationFailed { .. } => "CRAB-E0105",
            Self::LfsUnsupported { .. } => "CRAB-E0106",
            Self::CacheService { .. } => "CRAB-E0110",
            Self::ManagedRepository { .. } => "CRAB-E0140",
            Self::ImportPlanMismatch { .. } | Self::ImportLfsStoreNotFound { .. } => "CRAB-E0114",
            Self::ImportNoJournal { .. } => "CRAB-E0115",
            Self::ImportSourceMustBeRaw { .. } => "CRAB-E0118",
            Self::ImportSchemeMismatch { .. } => "CRAB-E0119",
            Self::ImportVersioningUnavailable { .. } => "CRAB-E0120",
            Self::ImportCommitCeilingExceeded { .. } => "CRAB-E0121",
            Self::ImportInvalidHistoryRange { .. } => "CRAB-E0122",
            Self::ImportTargetNotEmpty { .. } => "CRAB-E0123",
            Self::ImportSourceIsCrabRepo { .. } => "CRAB-E0112",
            Self::ImportLfsSourceUnsupported { .. } => "CRAB-E0113",
            Self::ImportPrefixCollision { .. } => "CRAB-E0117",
            Self::ImportMissingGitIdentity => "CRAB-E0116",
            Self::ImportRemoteExists { .. } => "CRAB-E0111",
            Self::WorkflowParse { .. } => "CRAB-E0200",
            Self::WorkflowCycle { .. } => "CRAB-E0201",
            Self::WorkflowUndefinedOut { .. } => "CRAB-E0202",
            Self::WorkflowStageNameInvalid { .. } => "CRAB-E0203",
            Self::WorkflowDiscoveryAmbiguous { .. } => "CRAB-E0204",
            Self::StageDepMissing { .. } => "CRAB-E0205",
            Self::StageDepMalformed { .. } => "CRAB-E0206",
            Self::StageOutMalformed { .. } => "CRAB-E0207",
            Self::StageOutTooLarge { .. } => "CRAB-E0208",
            Self::StageOutCountExceeded { .. } => "CRAB-E0209",
            Self::StageEnvMissing { .. } => "CRAB-E0210",
            Self::StageExecFailed { .. } => "CRAB-E0211",
            Self::StageExecSignaled { .. } => "CRAB-E0212",
            Self::StageExecTimeout { .. } => "CRAB-E0213",
            Self::StageDiskFull { .. } => "CRAB-E0214",
            Self::StageCacheMiss { .. } => "CRAB-E0215",
            Self::StageRetryExhausted { .. } => "CRAB-E0216",
            Self::StageOverwriteConflict { .. } => "CRAB-E0217",
            Self::StageSideEffectsRetryLimit { .. } => "CRAB-E0218",
            Self::StageSideEffectHookFailed { .. } => "CRAB-E0239",
            Self::LockfileStale { .. } => "CRAB-E0219",
            Self::LockfileCanonicalizationFailed { .. } => "CRAB-E0220",
            Self::LockfileMergeConflict { .. } => "CRAB-E0221",
            Self::ExperimentNotFound { .. } => "CRAB-E0222",
            Self::ExperimentCollision { .. } => "CRAB-E0223",
            Self::MetricsSchemaMismatch { .. } => "CRAB-E0224",
            Self::WorkflowJournalOpen { .. } => "CRAB-E0225",
            Self::WorkflowJournalCorrupt { .. } => "CRAB-E0226",
            Self::WorkflowJournalSchemaNewer { .. } => "CRAB-E0227",
            Self::WorkflowResumeFilesystemDrift { .. } => "CRAB-E0228",
            Self::WorkflowStateTransitionIllegal { .. } => "CRAB-E0229",
            Self::WorkflowLockTimeout { .. } => "CRAB-E0230",
            Self::WorkflowDisabled => "CRAB-E0231",
            Self::WorkflowHermeticViolation { .. } => "CRAB-E0232",
            Self::CacheEntrySchemaNewer { .. } => "CRAB-E0233",
            Self::StageRemoteExecutionUnsupported => "CRAB-E0234",
            Self::StageHermeticNotImplemented { .. } => "CRAB-E0235",
            Self::WorkflowDuplicateOutput { .. } => "CRAB-E0236",
            Self::WorkflowExperimentIdInvalid { .. } => "CRAB-E0237",
            Self::WorkflowExperimentMetadataSchemaNewer { .. } => "CRAB-E0238",
            Self::CacheEntryCorrupt { .. } => "CRAB-E0240",
            Self::CacheEntryHashMismatch { .. } => "CRAB-E0241",
            Self::RemoteCacheReadonly => "CRAB-E0242",
            Self::WorkflowValidationError { .. } => "CRAB-E0243",
            Self::WorkflowSelfLoop { .. } => "CRAB-E0244",
            Self::JournalDiskFull { .. } => "CRAB-E0245",
            Self::WorkflowTemplateUndefined { .. } => "CRAB-E0246",
            Self::WorkflowForeachEmpty { .. } => "CRAB-E0247",
            Self::WorkflowMatrixEmpty { .. } => "CRAB-E0248",
            Self::TierLifecycleConflict { .. } => "CRAB-E0300",
            Self::TierApplyUnauthorized { .. } => "CRAB-E0301",
            Self::TierProviderUnsupported { .. } => "CRAB-E0302",
            Self::ArchiveRestoreRequired { .. } => "CRAB-E0310",
            Self::ArchiveRestoreTimeout { .. } => "CRAB-E0311",
            Self::RestoreTierUnsupported { .. } => "CRAB-E0312",
            Self::GcEarlyDeleteBlocked { .. } => "CRAB-E0320",
            Self::ObjectLockedRetention { .. } => "CRAB-E0321",
            Self::GcPartialFailure { .. } => "CRAB-E0322",
            Self::OptimizeXorbsProfileOutOfRange { .. } => "CRAB-E0330",
            Self::OptimizeXorbsCorruptSource { .. } => "CRAB-E0331",
            Self::OptimizeXorbsAlreadyInProgress { .. } => "CRAB-E0332",
            Self::ConcurrentMaintenance { .. } => "CRAB-E0333",
            Self::CostPricingMissing { .. } => "CRAB-E0340",
            Self::CostInventoryReportStale { .. } => "CRAB-E0341",
            Self::ManifestParse { .. } => "CRAB-E0400",
            Self::PrefetchParse { .. } => "CRAB-E0401",
            Self::PrefetchProfileNotFound { .. } => "CRAB-E0402",
            Self::SpeculationDb { .. } => "CRAB-E0410",
            Self::GixRef(_) => "CRAB-E0600",
            Self::GixObject(_) => "CRAB-E0601",
            Self::GixPack(_) => "CRAB-E0602",
            Self::GixTransport(_) => "CRAB-E0603",
            Self::GixProtocol(_) => "CRAB-E0604",
            Self::GixFilterHandshake(_) => "CRAB-E0605",
            Self::GixFilterRequest(_) => "CRAB-E0606",
            Self::GixWorktree(_) => "CRAB-E0607",
            Self::GixConfig(_) => "CRAB-E0608",
            Self::GixCreds(_) => "CRAB-E0609",
            Self::GixStatus(_) => "CRAB-E060A",
            Self::GixRevwalk(_) => "CRAB-E060B",
            Self::GitTag(_) => "CRAB-E060C",
            Self::UnsupportedShell { .. } => "CRAB-E0135",
            Self::InvalidConfigKey { .. } => "CRAB-E0134",
            Self::PullConflict { .. } => "CRAB-E0130",
            Self::PullRemoteUnreachable { .. } => "CRAB-E0131",
            Self::UnadoptChunksMissing { .. } => "CRAB-E0132",
            Self::NothingToUndo => "CRAB-E0133",
            Self::MetaDb(inner) => match inner {
                MetaDbError::Open { .. } => "CRAB-E0500",
                MetaDbError::Close { .. } => "CRAB-E0501",
                MetaDbError::Read { .. } => "CRAB-E0502",
                MetaDbError::Write { .. } => "CRAB-E0503",
                // CRAB-E0504 reserved (formerly NotBootstrapped).
                MetaDbError::UnsupportedFormat { .. } => "CRAB-E0505",
                MetaDbError::FileNotFoundInFileIndexDb { .. } => "CRAB-E0506",
                MetaDbError::CorruptValue { .. } => "CRAB-E0507",
                MetaDbError::AlreadyClosed => "CRAB-E0508",
                MetaDbError::ReadOnly { .. } => "CRAB-E0509",
                MetaDbError::ReadOnlyUninitialized { .. } => "CRAB-E050A",
                MetaDbError::OperationAndClose { .. } => "CRAB-E050B",
            },
        }
    }

    /// Broad classification bucket for structured error output.
    ///
    /// Exhaustive — no catch-all.
    #[expect(
        clippy::match_same_arms,
        reason = "category arms stay grouped by product surface for auditability"
    )]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Read(error) => error.category(),
            Self::HydrationFailed { source, .. } => source.category(),
            Self::ManagedRepository { diagnostic } => match diagnostic {
                crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable { .. } => {
                    ErrorCategory::Transient
                }
                crab_auth_store::ManagedRepositoryDiagnostic::MalformedLocator
                | crab_auth_store::ManagedRepositoryDiagnostic::MissingProfile { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::ActiveProfileMissing
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidProfile { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::DiscoveryFailed { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::IncompatibleApi { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidBearer
                | crab_auth_store::ManagedRepositoryDiagnostic::InvalidServiceResponse { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::Inactive { .. } => {
                    ErrorCategory::Config
                }
                crab_auth_store::ManagedRepositoryDiagnostic::LoginRequired { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::NotFound { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::Forbidden { .. }
                | crab_auth_store::ManagedRepositoryDiagnostic::ExpiredGrant { .. } => {
                    ErrorCategory::Permanent
                }
                crab_auth_store::ManagedRepositoryDiagnostic::Cancelled => ErrorCategory::Cancelled,
            },
            Self::NetworkTransient(_) | Self::Throttled { .. } => ErrorCategory::Transient,

            Self::CasConflict { .. }
            | Self::NonFastForward { .. }
            | Self::RefAlreadyExists { .. }
            | Self::PushLockHeld { .. }
            | Self::PushPartialOutcome { .. }
            | Self::PushIntegrationFailed { .. }
            | Self::FileChangedDuringStaging { .. } => ErrorCategory::Conflict,

            Self::CorruptObject { .. }
            | Self::GitPackCorrupt(_)
            | Self::OriginIntegrity { .. }
            | Self::ChunkNotFound { .. }
            | Self::HashMismatch { .. }
            | Self::CrcMismatch { .. }
            | Self::PackIntegrity { .. }
            | Self::IncompleteShardReconstruction { .. }
            | Self::PointerMissingStaging { .. }
            | Self::PushConnectivityMissing { .. }
            | Self::PushMalformedObject { .. }
            | Self::FetchMalformedObject { .. }
            | Self::GitTag(_)
            | Self::LfsObjectCorrupt { .. } => ErrorCategory::Integrity,

            Self::NotFound { .. }
            | Self::Forbidden { .. }
            | Self::NoCredentials
            | Self::AuthFailed { .. }
            | Self::AuthExpired { .. }
            | Self::InsufficientSpace { .. }
            | Self::BeyondShallowBoundary { .. }
            | Self::PackTooLarge { .. }
            | Self::FetchNotAllowed { .. }
            | Self::FetchTooLarge { .. } => ErrorCategory::Permanent,

            Self::Configuration { .. }
            | Self::IncompatibleFormat { .. }
            | Self::InvalidPattern(_)
            | Self::UnsupportedShell { .. }
            | Self::InvalidConfigKey { .. }
            | Self::GixConfig(_) => ErrorCategory::Config,

            Self::Protocol(_)
            | Self::Io(_)
            | Self::Storage(_)
            | Self::CacheService { .. }
            | Self::WorkflowJournalOpen { .. }
            | Self::SpeculationDb { .. }
            | Self::GixRef(_)
            | Self::GixObject(_)
            | Self::GixPack(_)
            | Self::GixTransport(_)
            | Self::GixProtocol(_)
            | Self::GixFilterHandshake(_)
            | Self::GixFilterRequest(_)
            | Self::GixWorktree(_)
            | Self::GixCreds(_)
            | Self::GixStatus(_)
            | Self::GixRevwalk(_) => ErrorCategory::Transport,

            Self::StagingCorrupt(_) | Self::StagingLocked { .. } => ErrorCategory::Staging,

            Self::InvalidLfsPointer { .. }
            | Self::LfsObjectMissing { .. }
            | Self::LfsLockConflict { .. }
            | Self::LfsTransferProtocol(_)
            | Self::LfsMigrationFailed { .. }
            | Self::LfsUnsupported { .. } => ErrorCategory::Lfs,

            Self::Internal(_) => ErrorCategory::Internal,

            Self::Cancelled => ErrorCategory::Cancelled,

            // Import URL validation errors surface as configuration
            // problems from the user's perspective — a wrong flag
            // value, not a storage or protocol issue.
            Self::ImportSourceMustBeRaw { .. }
            | Self::ImportSchemeMismatch { .. }
            | Self::ImportPlanMismatch { .. }
            | Self::ImportNoJournal { .. }
            | Self::ImportVersioningUnavailable { .. }
            | Self::ImportCommitCeilingExceeded { .. }
            | Self::ImportInvalidHistoryRange { .. }
            | Self::ImportTargetNotEmpty { .. }
            | Self::ImportSourceIsCrabRepo { .. }
            | Self::ImportLfsSourceUnsupported { .. }
            | Self::ImportLfsStoreNotFound { .. }
            | Self::ImportPrefixCollision { .. }
            | Self::ImportMissingGitIdentity
            | Self::ImportRemoteExists { .. } => ErrorCategory::Config,

            // Workflow: schema / name / discovery / kill-switch failures
            // are always a config-shape problem.
            Self::WorkflowParse { .. }
            | Self::WorkflowCycle { .. }
            | Self::WorkflowUndefinedOut { .. }
            | Self::WorkflowStageNameInvalid { .. }
            | Self::WorkflowDiscoveryAmbiguous { .. }
            | Self::WorkflowDisabled
            | Self::RemoteCacheReadonly
            | Self::StageRemoteExecutionUnsupported
            | Self::StageHermeticNotImplemented { .. }
            | Self::WorkflowDuplicateOutput { .. }
            | Self::WorkflowValidationError { .. }
            | Self::WorkflowSelfLoop { .. }
            | Self::WorkflowExperimentIdInvalid { .. }
            | Self::WorkflowForeachEmpty { .. }
            | Self::WorkflowMatrixEmpty { .. }
            | Self::WorkflowTemplateUndefined { .. } => ErrorCategory::Config,

            // Workflow: stage-level failures the user has to fix in
            // their workflow or environment — not retry-able by crab.
            Self::StageDepMissing { .. }
            | Self::StageDepMalformed { .. }
            | Self::StageOutMalformed { .. }
            | Self::StageOutTooLarge { .. }
            | Self::StageOutCountExceeded { .. }
            | Self::StageEnvMissing { .. }
            | Self::StageExecFailed { .. }
            | Self::StageSideEffectsRetryLimit { .. }
            | Self::StageSideEffectHookFailed { .. }
            | Self::LockfileStale { .. }
            | Self::ExperimentNotFound { .. }
            | Self::WorkflowHermeticViolation { .. }
            | Self::ArchiveRestoreRequired { .. }
            | Self::GcEarlyDeleteBlocked { .. }
            | Self::ObjectLockedRetention { .. }
            | Self::GcPartialFailure { .. }
            | Self::CostInventoryReportStale { .. }
            | Self::PrefetchProfileNotFound { .. }
            | Self::UnadoptChunksMissing { .. }
            | Self::NothingToUndo => ErrorCategory::Permanent,

            Self::StageOverwriteConflict { .. }
            | Self::LockfileMergeConflict { .. }
            | Self::ExperimentCollision { .. }
            | Self::WorkflowLockTimeout { .. }
            | Self::TierLifecycleConflict { .. }
            | Self::OptimizeXorbsAlreadyInProgress { .. }
            | Self::ConcurrentMaintenance { .. }
            | Self::PullConflict { .. } => ErrorCategory::Conflict,

            Self::MetricsSchemaMismatch { .. }
            | Self::WorkflowJournalCorrupt { .. }
            | Self::WorkflowJournalSchemaNewer { .. }
            | Self::WorkflowResumeFilesystemDrift { .. }
            | Self::WorkflowStateTransitionIllegal { .. }
            | Self::CacheEntrySchemaNewer { .. }
            | Self::CacheEntryCorrupt { .. }
            | Self::CacheEntryHashMismatch { .. }
            | Self::LockfileCanonicalizationFailed { .. }
            | Self::WorkflowExperimentMetadataSchemaNewer { .. } => ErrorCategory::Integrity,

            Self::StageExecSignaled { .. }
            | Self::StageExecTimeout { .. }
            | Self::StageDiskFull { .. }
            | Self::StageCacheMiss { .. }
            | Self::StageRetryExhausted { .. }
            | Self::JournalDiskFull { .. }
            | Self::ArchiveRestoreTimeout { .. }
            | Self::PullRemoteUnreachable { .. } => ErrorCategory::Transient,

            // Storage economy: config-shape problems.
            Self::TierProviderUnsupported { .. }
            | Self::RestoreTierUnsupported { .. }
            | Self::OptimizeXorbsProfileOutOfRange { .. }
            | Self::CostPricingMissing { .. } => ErrorCategory::Config,

            // Storage economy: permanent user-facing errors.
            Self::TierApplyUnauthorized { .. } => ErrorCategory::Permanent,

            // Storage economy: integrity.
            Self::OptimizeXorbsCorruptSource { .. } => ErrorCategory::Integrity,

            // Hydrate manifest: config-shape problem (bad manifest content).
            Self::ManifestParse { .. } | Self::PrefetchParse { .. } => ErrorCategory::Config,

            Self::MetaDb(inner) => match inner {
                MetaDbError::Open { .. }
                | MetaDbError::Close { .. }
                | MetaDbError::Read { .. }
                | MetaDbError::Write { .. }
                | MetaDbError::OperationAndClose { .. }
                | MetaDbError::AlreadyClosed => ErrorCategory::Transport,
                MetaDbError::UnsupportedFormat { .. } => ErrorCategory::Config,
                MetaDbError::FileNotFoundInFileIndexDb { .. }
                | MetaDbError::ReadOnly { .. }
                | MetaDbError::ReadOnlyUninitialized { .. } => ErrorCategory::Permanent,
                MetaDbError::CorruptValue { .. } => ErrorCategory::Integrity,
            },
        }
    }

    /// Whether a retry is likely to succeed for this error class.
    ///
    /// Exhaustive — no catch-all. Adding a new variant forces an explicit
    /// decision about retry semantics.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Read(error) => error.is_retryable(),
            Self::HydrationFailed { source, .. } => source.is_retryable(),
            Self::ManagedRepository { diagnostic } => matches!(
                diagnostic,
                crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable { .. }
            ),
            // Transient infrastructure issues and advisory conflicts that
            // a retry layer can wait out.
            Self::NetworkTransient(_)
            | Self::Throttled { .. }
            | Self::CasConflict { .. }
            | Self::PushLockHeld { .. }
            | Self::StagingLocked { .. }
            | Self::StageExecSignaled { .. }
            | Self::StageExecTimeout { .. }
            | Self::StageDiskFull { .. }
            | Self::WorkflowLockTimeout { .. }
            | Self::ArchiveRestoreTimeout { .. }
            | Self::PullRemoteUnreachable { .. } => true,

            Self::NonFastForward { .. }
            | Self::RefAlreadyExists { .. }
            | Self::PushPartialOutcome { .. }
            | Self::PushIntegrationFailed { .. }
            | Self::FileChangedDuringStaging { .. }
            | Self::CorruptObject { .. }
            | Self::GitPackCorrupt(_)
            | Self::OriginIntegrity { .. }
            | Self::ChunkNotFound { .. }
            | Self::NotFound { .. }
            | Self::Forbidden { .. }
            | Self::NoCredentials
            | Self::AuthFailed { .. }
            | Self::AuthExpired { .. }
            | Self::InsufficientSpace { .. }
            | Self::Configuration { .. }
            | Self::IncompatibleFormat { .. }
            | Self::InvalidPattern(_)
            | Self::Protocol(_)
            | Self::Io(_)
            | Self::Storage(_)
            | Self::StagingCorrupt(_)
            | Self::HashMismatch { .. }
            | Self::CrcMismatch { .. }
            | Self::PackIntegrity { .. }
            | Self::IncompleteShardReconstruction { .. }
            | Self::PointerMissingStaging { .. }
            | Self::PushConnectivityMissing { .. }
            | Self::PushMalformedObject { .. }
            | Self::BeyondShallowBoundary { .. }
            | Self::PackTooLarge { .. }
            | Self::FetchNotAllowed { .. }
            | Self::FetchTooLarge { .. }
            | Self::FetchMalformedObject { .. }
            | Self::Cancelled
            | Self::Internal(_)
            | Self::InvalidLfsPointer { .. }
            | Self::LfsObjectCorrupt { .. }
            | Self::LfsObjectMissing { .. }
            | Self::LfsLockConflict { .. }
            | Self::LfsTransferProtocol(_)
            | Self::LfsMigrationFailed { .. }
            | Self::LfsUnsupported { .. }
            | Self::CacheService { .. }
            | Self::ImportSourceMustBeRaw { .. }
            | Self::ImportSchemeMismatch { .. }
            | Self::ImportPlanMismatch { .. }
            | Self::ImportNoJournal { .. }
            | Self::ImportVersioningUnavailable { .. }
            | Self::ImportCommitCeilingExceeded { .. }
            | Self::ImportInvalidHistoryRange { .. }
            | Self::ImportTargetNotEmpty { .. }
            | Self::ImportSourceIsCrabRepo { .. }
            | Self::ImportLfsSourceUnsupported { .. }
            | Self::ImportLfsStoreNotFound { .. }
            | Self::ImportPrefixCollision { .. }
            | Self::ImportMissingGitIdentity
            | Self::ImportRemoteExists { .. }
            | Self::WorkflowParse { .. }
            | Self::WorkflowCycle { .. }
            | Self::WorkflowUndefinedOut { .. }
            | Self::WorkflowStageNameInvalid { .. }
            | Self::WorkflowDiscoveryAmbiguous { .. }
            | Self::StageDepMissing { .. }
            | Self::StageDepMalformed { .. }
            | Self::StageOutMalformed { .. }
            | Self::StageOutTooLarge { .. }
            | Self::StageOutCountExceeded { .. }
            | Self::StageEnvMissing { .. }
            | Self::StageExecFailed { .. }
            // Budget already spent — no point retrying at this layer.
            | Self::StageCacheMiss { .. }
            | Self::StageRetryExhausted { .. }
            | Self::StageOverwriteConflict { .. }
            | Self::StageSideEffectsRetryLimit { .. }
            | Self::StageSideEffectHookFailed { .. }
            | Self::LockfileStale { .. }
            | Self::LockfileCanonicalizationFailed { .. }
            | Self::LockfileMergeConflict { .. }
            | Self::ExperimentNotFound { .. }
            | Self::ExperimentCollision { .. }
            | Self::MetricsSchemaMismatch { .. }
            | Self::WorkflowJournalOpen { .. }
            | Self::WorkflowJournalCorrupt { .. }
            | Self::WorkflowJournalSchemaNewer { .. }
            | Self::WorkflowResumeFilesystemDrift { .. }
            | Self::WorkflowStateTransitionIllegal { .. }
            | Self::WorkflowDisabled
            | Self::WorkflowHermeticViolation { .. }
            | Self::CacheEntrySchemaNewer { .. }
            | Self::CacheEntryCorrupt { .. }
            | Self::CacheEntryHashMismatch { .. }
            | Self::RemoteCacheReadonly
            | Self::StageRemoteExecutionUnsupported
            | Self::StageHermeticNotImplemented { .. }
            | Self::WorkflowDuplicateOutput { .. }
            | Self::WorkflowValidationError { .. }
            | Self::WorkflowSelfLoop { .. }
            | Self::JournalDiskFull { .. }
            | Self::WorkflowExperimentIdInvalid { .. }
            | Self::WorkflowExperimentMetadataSchemaNewer { .. }
            | Self::WorkflowForeachEmpty { .. }
            | Self::WorkflowMatrixEmpty { .. }
            | Self::WorkflowTemplateUndefined { .. }
            | Self::TierLifecycleConflict { .. }
            | Self::TierApplyUnauthorized { .. }
            | Self::TierProviderUnsupported { .. }
            | Self::ArchiveRestoreRequired { .. }
            | Self::RestoreTierUnsupported { .. }
            | Self::GcEarlyDeleteBlocked { .. }
            | Self::ObjectLockedRetention { .. }
            | Self::GcPartialFailure { .. }
            | Self::OptimizeXorbsProfileOutOfRange { .. }
            | Self::OptimizeXorbsCorruptSource { .. }
            | Self::OptimizeXorbsAlreadyInProgress { .. }
            | Self::ConcurrentMaintenance { .. }
            | Self::CostPricingMissing { .. }
            | Self::CostInventoryReportStale { .. }
            | Self::ManifestParse { .. }
            | Self::PrefetchParse { .. }
            | Self::PrefetchProfileNotFound { .. }
            | Self::SpeculationDb { .. }
            | Self::GixRef(_)
            | Self::GixObject(_)
            | Self::GixPack(_)
            | Self::GixTransport(_)
            | Self::GixProtocol(_)
            | Self::GixFilterHandshake(_)
            | Self::GixFilterRequest(_)
            | Self::GixWorktree(_)
            | Self::GixConfig(_)
            | Self::GixCreds(_)
            | Self::GixStatus(_)
            | Self::GixRevwalk(_)
            | Self::GitTag(_)
            | Self::UnsupportedShell { .. }
            | Self::InvalidConfigKey { .. }
            | Self::PullConflict { .. }
            | Self::UnadoptChunksMissing { .. }
            | Self::NothingToUndo
            | Self::MetaDb(_) => false,
        }
    }

    /// Per-variant structured fields for the error envelope `details` key.
    ///
    /// Returns `Value::Null` for variants with no meaningful structured data.
    pub fn details_json(&self) -> serde_json::Value {
        match self {
            Self::Read(error) => error.details_json(),
            Self::HydrationFailed { failed, source } => serde_json::json!({
                "failed_files": failed,
                "cause": source.details_json(),
            }),
            Self::NetworkTransient(err) | Self::Storage(err) => {
                serde_json::json!({ "source": err.to_string() })
            }
            Self::Throttled { retry_after } => {
                serde_json::json!({
                    "retry_after_ms": retry_after.map(|d| d.as_millis() as u64)
                })
            }
            Self::CasConflict {
                path,
                expected_etag,
            } => {
                serde_json::json!({
                    "path": path,
                    "expected_etag": expected_etag,
                })
            }
            Self::NonFastForward {
                ref_name,
                have,
                want,
            } => {
                serde_json::json!({
                    "ref_name": ref_name,
                    "have": have,
                    "want": want,
                })
            }
            Self::RefAlreadyExists { name } | Self::PrefetchProfileNotFound { name } => {
                serde_json::json!({ "name": name })
            }
            Self::PushLockHeld {
                ref_name,
                holder,
                expires_at_unix,
            } => {
                serde_json::json!({
                    "ref_name": ref_name,
                    "holder": holder,
                    "expires_at_unix": expires_at_unix,
                })
            }
            Self::CorruptObject { path, reason } => {
                serde_json::json!({
                    "path": path,
                    "reason": reason,
                })
            }
            Self::GitPackCorrupt(source) => serde_json::json!({ "source": source.to_string() }),
            Self::OriginIntegrity { path, source } => serde_json::json!({
                "path": path,
                "reason": source.to_string(),
                "origin": "object-store",
            }),
            Self::ChunkNotFound { hash } => {
                serde_json::json!({ "hash": hash })
            }
            Self::NotFound { path }
            | Self::Forbidden { path }
            | Self::AuthFailed { path }
            | Self::AuthExpired { path } => serde_json::json!({ "path": path }),
            Self::ManagedRepository { diagnostic } => serde_json::json!({
                "kind": diagnostic.kind(),
                "message": diagnostic.to_string(),
            }),
            Self::NoCredentials
            | Self::Cancelled
            | Self::ImportMissingGitIdentity
            | Self::WorkflowDisabled
            | Self::StageRemoteExecutionUnsupported
            | Self::RemoteCacheReadonly
            | Self::NothingToUndo => serde_json::Value::Null,
            Self::InsufficientSpace { needed, available } => {
                serde_json::json!({
                    "needed": needed,
                    "available": available,
                })
            }
            Self::Configuration { key, origin } => {
                serde_json::json!({
                    "key": key,
                    "origin": origin,
                })
            }
            Self::IncompatibleFormat { required, found } => {
                serde_json::json!({
                    "required": required,
                    "found": found,
                })
            }
            Self::InvalidPattern(err) => {
                serde_json::json!({ "pattern": err.to_string() })
            }
            Self::Protocol(msg)
            | Self::StagingCorrupt(msg)
            | Self::Internal(msg)
            | Self::LfsTransferProtocol(msg) => {
                serde_json::json!({ "message": msg })
            }
            Self::Io(err) => {
                serde_json::json!({ "message": err.to_string() })
            }
            Self::StagingLocked { holder_pid } => {
                serde_json::json!({ "holder_pid": holder_pid })
            }
            Self::HashMismatch { requested, actual } => {
                serde_json::json!({
                    "requested": requested,
                    "actual": actual,
                })
            }
            Self::CrcMismatch { segment_id, offset } => {
                serde_json::json!({
                    "segment_id": segment_id,
                    "offset": offset,
                })
            }
            Self::PackIntegrity { expected, computed } => {
                serde_json::json!({
                    "expected": expected,
                    "computed": computed,
                })
            }
            Self::IncompleteShardReconstruction {
                file_hash,
                path,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
            } => {
                serde_json::json!({
                    "file_hash": file_hash,
                    "path": path,
                    "uncovered_chunks": uncovered_chunks,
                    "example_chunk_hash": example_chunk_hash,
                    "example_chunk_index": example_chunk_index,
                })
            }
            Self::PointerMissingStaging {
                total,
                missing,
                example_file_hash,
                example_size,
            } => {
                serde_json::json!({
                    "total": total,
                    "missing": missing,
                    "example_file_hash": example_file_hash,
                    "example_size": example_size,
                })
            }
            Self::PushConnectivityMissing {
                ref_name,
                oid,
                total_missing,
            } => {
                serde_json::json!({
                    "ref_name": ref_name,
                    "oid": oid,
                    "total_missing": total_missing,
                })
            }
            Self::PushPartialOutcome { outcomes, source } => {
                // Surface the per-ref outcome map so downstream JSON
                // consumers can see which refs succeeded and which
                // failed without re-parsing the Display message.
                let refs: serde_json::Map<String, serde_json::Value> = outcomes
                    .outcomes
                    .iter()
                    .map(|(name, outcome)| (name.clone(), serde_json::json!(outcome.to_string())))
                    .collect();
                serde_json::json!({
                    "source": source.to_string(),
                    "source_code": source.code(),
                    "outcomes": serde_json::Value::Object(refs),
                })
            }
            Self::PushIntegrationFailed { command, message } => {
                serde_json::json!({
                    "command": command,
                    "message": message,
                })
            }
            Self::FileChangedDuringStaging {
                path,
                first_hash,
                second_hash,
                first_size,
                second_size,
            } => {
                serde_json::json!({
                    "path": path,
                    "first_hash": first_hash,
                    "second_hash": second_hash,
                    "first_size": first_size,
                    "second_size": second_size,
                })
            }
            Self::BeyondShallowBoundary { oid }
            | Self::LfsObjectCorrupt { oid }
            | Self::LfsObjectMissing { oid } => serde_json::json!({ "oid": oid }),
            Self::PackTooLarge { size, limit } | Self::FetchTooLarge { size, limit } => {
                serde_json::json!({
                    "size": size,
                    "limit": limit,
                })
            }
            Self::PushMalformedObject { oid, kind, detail } => {
                serde_json::json!({
                    "oid": oid,
                    "kind": kind,
                    "detail": detail,
                })
            }
            Self::FetchNotAllowed { sha, reason } => {
                serde_json::json!({
                    "sha": sha,
                    "reason": reason,
                })
            }
            Self::FetchMalformedObject {
                pack_id,
                oid,
                kind,
                detail,
            } => {
                serde_json::json!({
                    "pack_id": pack_id,
                    "oid": oid,
                    "kind": kind,
                    "detail": detail,
                })
            }
            Self::InvalidLfsPointer { reason }
            | Self::LfsMigrationFailed { reason }
            | Self::CacheService { reason }
            | Self::PrefetchParse { reason } => serde_json::json!({ "reason": reason }),
            Self::LfsUnsupported { command, reason } => {
                serde_json::json!({
                    "command": command,
                    "reason": reason,
                })
            }
            Self::LfsLockConflict { path, owner } => {
                serde_json::json!({
                    "path": path,
                    "owner": owner,
                })
            }
            Self::ImportSourceMustBeRaw { url }
            | Self::ImportVersioningUnavailable { url }
            | Self::ImportSourceIsCrabRepo { url }
            | Self::ImportLfsSourceUnsupported { url }
            | Self::ImportLfsStoreNotFound { url } => {
                serde_json::json!({ "url": url })
            }
            Self::ImportSchemeMismatch {
                from_scheme,
                to_scheme,
            } => {
                serde_json::json!({
                    "from_scheme": from_scheme,
                    "to_scheme": to_scheme,
                })
            }
            Self::ImportPlanMismatch { recorded, provided } => {
                serde_json::json!({
                    "recorded": recorded,
                    "provided": provided,
                })
            }
            Self::ImportNoJournal { path } | Self::ImportTargetNotEmpty { path } => {
                serde_json::json!({ "path": path })
            }
            Self::ImportCommitCeilingExceeded { planned, ceiling } => {
                serde_json::json!({
                    "planned": planned,
                    "ceiling": ceiling,
                })
            }
            Self::ImportInvalidHistoryRange { since, until } => {
                serde_json::json!({
                    "since": since,
                    "until": until,
                })
            }
            Self::ImportPrefixCollision { detail } => {
                serde_json::json!({ "detail": detail })
            }
            Self::ImportRemoteExists {
                existing_url,
                new_url,
            } => {
                serde_json::json!({
                    "existing_url": existing_url,
                    "new_url": new_url,
                })
            }
            Self::WorkflowParse { path, source }
            | Self::LockfileCanonicalizationFailed { path, source } => {
                serde_json::json!({
                    "path": path,
                    "source": source.to_string(),
                })
            }
            Self::WorkflowCycle { stages } => {
                serde_json::json!({ "stages": stages })
            }
            Self::WorkflowUndefinedOut { consumer, out } => {
                serde_json::json!({
                    "consumer": consumer,
                    "out": out,
                })
            }
            Self::WorkflowStageNameInvalid { name, reason } => {
                serde_json::json!({
                    "name": name,
                    "reason": reason,
                })
            }
            Self::WorkflowDiscoveryAmbiguous { candidates } => {
                serde_json::json!({ "candidates": candidates })
            }
            Self::StageDepMissing { stage, path }
            | Self::StageDiskFull { stage, path }
            | Self::WorkflowHermeticViolation { stage, path }
            | Self::WorkflowSelfLoop { stage, path } => {
                serde_json::json!({
                    "stage": stage,
                    "path": path,
                })
            }
            Self::StageDepMalformed {
                stage,
                path,
                reason,
            }
            | Self::StageOutMalformed {
                stage,
                path,
                reason,
            }
            | Self::StageOverwriteConflict {
                stage,
                path,
                reason,
            } => {
                serde_json::json!({
                    "stage": stage,
                    "path": path,
                    "reason": reason,
                })
            }
            Self::StageOutTooLarge {
                stage,
                path,
                size,
                limit,
            } => {
                serde_json::json!({
                    "stage": stage,
                    "path": path,
                    "size": size,
                    "limit": limit,
                })
            }
            Self::StageOutCountExceeded {
                stage,
                count,
                limit,
            } => {
                serde_json::json!({
                    "stage": stage,
                    "count": count,
                    "limit": limit,
                })
            }
            Self::StageEnvMissing { stage, var } => {
                serde_json::json!({
                    "stage": stage,
                    "var": var,
                })
            }
            Self::StageExecFailed { stage, exit_code }
            | Self::StageSideEffectHookFailed { stage, exit_code } => {
                serde_json::json!({
                    "stage": stage,
                    "exit_code": exit_code,
                })
            }
            Self::StageExecSignaled { stage, signal } => {
                serde_json::json!({
                    "stage": stage,
                    "signal": signal,
                })
            }
            Self::StageExecTimeout { stage, elapsed_ms } => {
                serde_json::json!({
                    "stage": stage,
                    "elapsed_ms": elapsed_ms,
                })
            }
            Self::StageCacheMiss { stage, reason } => {
                serde_json::json!({
                    "stage": stage,
                    "reason": reason,
                })
            }
            Self::StageRetryExhausted { stage, attempts } => {
                serde_json::json!({
                    "stage": stage,
                    "attempts": attempts,
                })
            }
            Self::StageSideEffectsRetryLimit { stage }
            | Self::LockfileStale { stage }
            | Self::StageHermeticNotImplemented { stage }
            | Self::WorkflowForeachEmpty { stage } => {
                serde_json::json!({ "stage": stage })
            }
            Self::LockfileMergeConflict { path } | Self::JournalDiskFull { path } => {
                serde_json::json!({ "path": path })
            }
            Self::ExperimentNotFound { id } | Self::ExperimentCollision { id } => {
                serde_json::json!({ "id": id })
            }
            Self::MetricsSchemaMismatch { path, source } => {
                serde_json::json!({
                    "path": path,
                    "source": source.to_string(),
                })
            }
            Self::WorkflowJournalOpen { path, source } => {
                serde_json::json!({
                    "path": path,
                    "source": source.to_string(),
                })
            }
            Self::WorkflowJournalCorrupt { run_id, detail } => {
                serde_json::json!({
                    "run_id": run_id,
                    "detail": detail,
                })
            }
            Self::WorkflowJournalSchemaNewer {
                run_id,
                found,
                supported,
            } => {
                serde_json::json!({
                    "run_id": run_id,
                    "found": found,
                    "supported": supported,
                })
            }
            Self::WorkflowResumeFilesystemDrift {
                stage,
                expected,
                observed,
            } => {
                serde_json::json!({
                    "stage": stage,
                    "expected": expected,
                    "observed": observed,
                })
            }
            Self::WorkflowStateTransitionIllegal { stage, from, to } => {
                serde_json::json!({
                    "stage": stage,
                    "from": from,
                    "to": to,
                })
            }
            Self::WorkflowLockTimeout { held_by, waited_ms } => {
                serde_json::json!({
                    "held_by": held_by,
                    "waited_ms": waited_ms,
                })
            }
            Self::CacheEntrySchemaNewer {
                stage_hash,
                found,
                supported,
            } => {
                serde_json::json!({
                    "stage_hash": stage_hash,
                    "found": found,
                    "supported": supported,
                })
            }
            Self::WorkflowDuplicateOutput {
                first,
                second,
                path,
            } => {
                serde_json::json!({
                    "first": first,
                    "second": second,
                    "path": path,
                })
            }
            Self::WorkflowExperimentIdInvalid { raw, reason } => {
                serde_json::json!({
                    "raw": raw,
                    "reason": reason,
                })
            }
            Self::WorkflowExperimentMetadataSchemaNewer {
                id,
                found,
                supported,
            } => {
                serde_json::json!({
                    "id": id,
                    "found": found,
                    "supported": supported,
                })
            }
            Self::CacheEntryCorrupt {
                stage_hash,
                path,
                expected,
                actual,
            } => {
                serde_json::json!({
                    "stage_hash": stage_hash,
                    "path": path,
                    "expected": expected,
                    "actual": actual,
                })
            }
            Self::CacheEntryHashMismatch {
                manifest_hash,
                local_hash,
            } => {
                serde_json::json!({
                    "manifest_hash": manifest_hash,
                    "local_hash": local_hash,
                })
            }
            Self::WorkflowValidationError {
                field,
                value,
                expected,
            } => {
                serde_json::json!({
                    "field": field,
                    "value": value,
                    "expected": expected,
                })
            }
            Self::WorkflowTemplateUndefined { key, field, stage } => {
                serde_json::json!({
                    "key": key,
                    "field": field,
                    "stage": stage,
                })
            }
            Self::WorkflowMatrixEmpty { stage, variable } => {
                serde_json::json!({
                    "stage": stage,
                    "variable": variable,
                })
            }
            Self::TierLifecycleConflict {
                prefix,
                existing_id,
                new_id,
            } => {
                serde_json::json!({
                    "prefix": prefix,
                    "existing_id": existing_id,
                    "new_id": new_id,
                })
            }
            Self::TierApplyUnauthorized {
                required_permission,
            } => {
                serde_json::json!({ "required_permission": required_permission })
            }
            Self::TierProviderUnsupported { provider } => {
                serde_json::json!({ "provider": provider })
            }
            Self::ArchiveRestoreRequired {
                xorb,
                class,
                estimated_eta,
            } => {
                serde_json::json!({
                    "xorb": xorb,
                    "class": class,
                    "estimated_eta": estimated_eta,
                })
            }
            Self::ArchiveRestoreTimeout {
                xorb,
                class,
                elapsed_secs,
            } => {
                serde_json::json!({
                    "xorb": xorb,
                    "class": class,
                    "elapsed_secs": elapsed_secs,
                })
            }
            Self::RestoreTierUnsupported {
                tier,
                class,
                supported,
            } => {
                serde_json::json!({
                    "tier": tier,
                    "class": class,
                    "supported": supported,
                })
            }
            Self::GcEarlyDeleteBlocked {
                class,
                age_days,
                min_days,
                penalty_usd,
            } => {
                serde_json::json!({
                    "class": class,
                    "age_days": age_days,
                    "min_days": min_days,
                    "penalty_usd": penalty_usd,
                })
            }
            Self::ObjectLockedRetention { path, until } => {
                serde_json::json!({
                    "path": path,
                    "until": until,
                })
            }
            Self::GcPartialFailure {
                objects_deleted,
                delete_failures,
                reconciliation_failed,
                source,
            } => {
                serde_json::json!({
                    "objects_deleted": objects_deleted,
                    "delete_failures": delete_failures,
                    "reconciliation_failed": reconciliation_failed,
                    "source_code": source.code(),
                    "source": source.to_string(),
                })
            }
            Self::OptimizeXorbsProfileOutOfRange { name, bytes } => {
                serde_json::json!({
                    "name": name,
                    "bytes": bytes,
                })
            }
            Self::OptimizeXorbsCorruptSource { xorb } => {
                serde_json::json!({ "xorb": xorb })
            }
            Self::OptimizeXorbsAlreadyInProgress { pid, started_at } => {
                serde_json::json!({
                    "pid": pid,
                    "started_at": started_at,
                })
            }
            Self::ConcurrentMaintenance { other } => {
                serde_json::json!({ "other": other })
            }
            Self::CostPricingMissing { provider, region } => {
                serde_json::json!({
                    "provider": provider,
                    "region": region,
                })
            }
            Self::CostInventoryReportStale {
                provider,
                report_at,
                max_hours,
            } => {
                serde_json::json!({
                    "provider": provider,
                    "report_at": report_at,
                    "max_hours": max_hours,
                })
            }
            Self::ManifestParse { line, reason } => {
                serde_json::json!({
                    "line": line,
                    "reason": reason,
                })
            }
            Self::SpeculationDb { path, source } => {
                serde_json::json!({
                    "path": path.display().to_string(),
                    "source": source.to_string(),
                })
            }
            // gix-* wrappers expose only the wrapped error's string
            // form — each underlying gix type is `non_exhaustive` or
            // carries structured fields that aren't worth re-shaping
            // here. The source chain (via `#[from]`) remains intact
            // for programmatic branching via `.source()`.
            Self::GixRef(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixObject(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixPack(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixTransport(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixProtocol(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixFilterHandshake(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixFilterRequest(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixWorktree(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixConfig(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixCreds(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixStatus(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GixRevwalk(err) => serde_json::json!({ "source": err.to_string() }),
            Self::GitTag(err) => serde_json::json!({ "source": err.to_string() }),
            Self::UnsupportedShell { shell } => serde_json::json!({ "shell": shell }),
            Self::InvalidConfigKey { key, valid_keys } => serde_json::json!({
                "key": key,
                "valid_keys": valid_keys,
            }),
            Self::PullConflict { count, files } | Self::UnadoptChunksMissing { count, files } => {
                serde_json::json!({
                    "count": count,
                    "files": files,
                })
            }
            Self::PullRemoteUnreachable { remote, reason } => serde_json::json!({
                "remote": remote,
                "reason": reason,
            }),
            Self::MetaDb(inner) => match inner {
                MetaDbError::Open { db, path, source } => serde_json::json!({
                    "db": db,
                    "path": path,
                    "source": source.to_string(),
                }),
                MetaDbError::Close { db, source } | MetaDbError::Write { db, source } => {
                    serde_json::json!({
                        "db": db,
                        "source": source.to_string(),
                    })
                }
                MetaDbError::Read { db, prefix, source } => serde_json::json!({
                    "db": db,
                    "prefix": prefix,
                    "source": source.to_string(),
                }),
                MetaDbError::UnsupportedFormat { db, found } => serde_json::json!({
                    "db": db,
                    "found": found,
                    "required": 1,
                }),
                MetaDbError::FileNotFoundInFileIndexDb { file_hash } => serde_json::json!({
                    "file_hash": file_hash,
                }),
                MetaDbError::CorruptValue { db, key, reason } => serde_json::json!({
                    "db": db,
                    "key": key,
                    "reason": reason,
                }),
                MetaDbError::AlreadyClosed => serde_json::Value::Null,
                MetaDbError::ReadOnly { db, op } => serde_json::json!({
                    "db": db,
                    "op": op,
                }),
                MetaDbError::ReadOnlyUninitialized { db, path } => serde_json::json!({
                    "db": db,
                    "path": path,
                }),
                MetaDbError::OperationAndClose {
                    db,
                    operation,
                    close,
                } => serde_json::json!({
                    "db": db,
                    "operation": operation.to_string(),
                    "close": close.to_string(),
                }),
            },
        }
    }
}

const STORAGE_HINT: &str = "Run `crab doctor` to check the remote, credentials, and repository. Retry with `--log-level debug` if the cause is still unclear.";
const STORAGE_DOCS_ANCHOR: &str = "cli/diagnostics/health-check";

impl CrabError {
    /// Human-readable remediation hint for CLI display.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::Read(error) => error.hint(),
            Self::HydrationFailed { source, .. } => source.hint(),
            Self::NotFound { .. } => Some(
                "Check the requested path and remote. For a new repository, run `crab configure <REMOTE>`; otherwise run `crab doctor`.",
            ),
            Self::Forbidden { .. } => Some(
                "Grant the active cloud identity access to this bucket and repository prefix, then run `crab doctor` to verify it.",
            ),
            Self::NoCredentials => Some(
                "Configure credentials for the selected cloud provider, then run `crab doctor` to verify bucket access.",
            ),
            Self::AuthFailed { .. } => Some(
                "Refresh or replace the active cloud credentials, then run `crab doctor` to verify them.",
            ),
            Self::AuthExpired { .. } => Some(
                "Refresh the expired cloud credentials, then run `crab doctor` to verify them.",
            ),
            Self::Configuration { .. } => Some(
                "Run `crab doctor` to inspect the current setup, or `crab configure` for guided repository configuration.",
            ),
            Self::Storage(_) => Some(STORAGE_HINT),
            _ => None,
        }
    }

    /// Docs-site path and optional anchor for deep-linking.
    pub fn docs_anchor(&self) -> Option<&'static str> {
        match self {
            Self::Read(error) => error.docs_anchor(),
            Self::HydrationFailed { source, .. } => source.docs_anchor(),
            Self::NotFound { .. } => Some("cli/diagnostics/error-codes"),
            Self::Forbidden { .. }
            | Self::NoCredentials
            | Self::AuthFailed { .. }
            | Self::AuthExpired { .. } => {
                Some("cli/authentication/static-credentials#troubleshooting")
            }
            Self::Configuration { .. } => Some("cli/reference/crab-configure"),
            Self::Storage(_) => Some(STORAGE_DOCS_ANCHOR),
            _ => None,
        }
    }
}

/// Crate-level `Result` alias. Library APIs should return this.
pub type Result<T> = std::result::Result<T, CrabError>;

/// Check whether a cancellation token has been triggered.
///
/// Convenience for subsystems that hold a raw `CancellationToken`
/// rather than an `AppContext` (e.g. GC sweep phases).
///
/// # Errors
///
/// Returns [`CrabError::Cancelled`] when the token has been triggered.
pub fn check_cancelled(cancel: &tokio_util::sync::CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hydration_failure_preserves_cause_diagnostics() {
        use std::error::Error;

        for source in [
            CrabError::Forbidden {
                path: "xorbs/private".into(),
            },
            CrabError::Throttled {
                retry_after: Some(Duration::from_secs(3)),
            },
            CrabError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        ] {
            let source = std::sync::Arc::new(source);
            let failure = CrabError::HydrationFailed {
                failed: 2,
                source: source.clone(),
            };
            assert_eq!(failure.code(), source.code());
            assert_eq!(
                crate::core::error_catalog::error_code(&failure),
                source.code()
            );
            assert_eq!(failure.exit_code(), source.exit_code());
            assert_eq!(failure.category(), source.category());
            assert_eq!(failure.is_retryable(), source.is_retryable());
            assert_eq!(failure.hint(), source.hint());
            assert_eq!(failure.docs_anchor(), source.docs_anchor());
            assert_eq!(
                failure.details_json(),
                serde_json::json!({
                    "failed_files": 2, "cause": source.details_json(),
                })
            );
            let cause = failure
                .source()
                .unwrap()
                .downcast_ref::<std::sync::Arc<CrabError>>()
                .unwrap();
            assert!(std::sync::Arc::ptr_eq(cause, &source));
        }
    }

    #[test]
    fn availability_preserves_product_diagnostics() {
        use std::error::Error;

        for source in [
            CrabError::NoCredentials,
            CrabError::AuthExpired {
                path: "xorbs/archive".into(),
            },
            CrabError::Throttled {
                retry_after: Some(Duration::from_secs(3)),
            },
            CrabError::Cancelled,
        ] {
            let code = source.code();
            let category = source.category();
            let exit_code = source.exit_code();
            let retry = crate::storage::retry::retry_class(&source);
            let details = source.details_json();
            let message = source.to_string();
            let error = CrabError::from(crab_read::ReadError::availability(source));
            assert_eq!(error.code(), code);
            assert_eq!(crate::core::error_catalog::error_code(&error), code);
            assert_eq!(error.category(), category);
            assert_eq!(error.exit_code(), exit_code);
            assert_eq!(crate::storage::retry::retry_class(&error), retry);
            assert_eq!(error.details_json(), details);
            assert_eq!(error.to_string(), message);
            assert!(
                std::iter::successors(error.source(), |source| (*source).source())
                    .any(|source| source.is::<CrabError>())
            );
        }
    }

    #[test]
    fn uncertain_journal_commit_survives_write_and_read_boundaries() {
        for through_write in [false, true] {
            let source = crab_metadata::error::MetadataError::RefJournalCommitUncertain {
                transaction_id: "a".repeat(64),
                source: Box::new(crab_storage::StorageError::Io {
                    source: std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "write reply lost",
                    ),
                }),
                verification: None,
            };
            let mapped = if through_write {
                CrabError::from(crab_write::WriteError::Metadata(source))
            } else {
                CrabError::from(crab_read::ReadError::Metadata(source))
            };
            let CrabError::Io(error) = mapped else {
                panic!("expected typed I/O error");
            };
            assert!(
                matches!(error.get_ref().and_then(|source| source.downcast_ref::<crab_metadata::error::MetadataError>()),
                Some(crab_metadata::error::MetadataError::RefJournalCommitUncertain { transaction_id, .. }) if transaction_id == &"a".repeat(64))
            );
        }
    }

    #[test]
    fn metadata_lookup_limit_is_a_protocol_rejection() {
        let error = CrabError::from(crab_metadata::error::MetadataError::FileLookupLimit {
            resource: "shard visits",
            maximum: 4,
        });
        assert_eq!(error.code(), "CRAB-E0060");
    }

    #[tokio::test]
    async fn metadata_lookup_worker_errors_retain_their_sources() {
        let gate = tokio::sync::Semaphore::new(0);
        gate.close();
        let admission = crab_metadata::error::MetadataError::FileLookupAdmission {
            source: gate.acquire().await.unwrap_err(),
        };
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let worker = crab_metadata::error::MetadataError::FileLookupWorker {
            source: task.await.unwrap_err(),
        };
        for source in [admission, worker] {
            let CrabError::Io(error) = CrabError::from(source) else {
                panic!("expected read I/O failure");
            };
            assert!(
                error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<crab_metadata::error::MetadataError>())
                    .is_some()
            );
        }
    }

    #[test]
    fn origin_integrity_keeps_typed_source_and_terminal_classification() {
        use std::error::Error as _;

        let error = CrabError::from(crab_read::ReadError::from(
            crab_cache_store::CacheStoreError::OriginIntegrity {
                path: "xorbs/bad".into(),
                source: crab_cache::CacheError::HashMismatch {
                    requested: "expected".into(),
                    actual: "corrupt".into(),
                },
            },
        ));
        assert!(error.source().unwrap().is::<crab_cache::CacheError>());
        assert_eq!(error.code(), "CRAB-E0020");
        assert_eq!(error.code(), crate::core::error_catalog::error_code(&error));
        assert_eq!(error.exit_code(), 4);
        assert_eq!(error.category(), ErrorCategory::Integrity);
        assert!(!error.is_retryable());
        assert_eq!(error.details_json()["origin"], "object-store");
        assert_eq!(
            crate::storage::retry::retry_class(&error),
            crate::storage::retry::RetryClass::Fatal
        );
    }

    #[test]
    fn unsafe_cache_path_keeps_the_original_source() {
        let error = CrabError::from(crab_cache::CacheError::UnsafeRoot {
            path: "cache/chunks".into(),
            reason: "unsafe permissions".into(),
        });
        let CrabError::Io(source) = error else {
            panic!("unsafe cache access must be a permission error");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(source.get_ref().unwrap().is::<crab_cache::CacheError>());
    }

    #[test]
    fn cache_budget_conflict_is_a_configuration_error() {
        let error = CrabError::from(crab_cache::CacheError::BudgetConflict {
            path: "cache/chunks".into(),
            active_bytes: Some(1024),
            requested_bytes: Some(512),
        });
        let CrabError::Configuration { key, origin } = error else {
            panic!("cache budget conflicts must retain configuration attribution");
        };
        assert_eq!(key, "cache.max_bytes");
        assert!(origin.contains("active budget is 1024 bytes, requested 512 bytes"));
    }

    #[test]
    fn cache_inspection_timeout_preserves_io_kind() {
        let error = CrabError::from(crab_cache::CacheError::InspectionTimeout {
            path: "cache/hints/shard-hints.sqlite".into(),
            timeout_ms: 5_000,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            ),
        });
        let CrabError::Io(source) = error else {
            panic!("cache inspection timeouts must retain I/O attribution");
        };

        assert_eq!(source.kind(), std::io::ErrorKind::TimedOut);
        let cache_error = source
            .get_ref()
            .unwrap()
            .downcast_ref::<crab_cache::CacheError>()
            .unwrap();
        assert!(
            std::error::Error::source(cache_error)
                .unwrap()
                .is::<rusqlite::Error>()
        );
    }

    #[test]
    fn exit_code_non_fast_forward() {
        let err = CrabError::NonFastForward {
            ref_name: "refs/heads/main".into(),
            have: "aaa".into(),
            want: "bbb".into(),
        };
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn exit_code_cas_and_ref_exists() {
        let cas = CrabError::CasConflict {
            path: "p".into(),
            expected_etag: None,
        };
        let rae = CrabError::RefAlreadyExists { name: "r".into() };
        let integration = CrabError::PushIntegrationFailed {
            command: "git pull --rebase --autostash origin main".into(),
            message: "CONFLICT".into(),
        };
        assert_eq!(cas.exit_code(), 3);
        assert_eq!(rae.exit_code(), 3);
        assert_eq!(integration.exit_code(), 3);
    }

    #[test]
    fn exit_code_corrupt_variants() {
        let cases: Vec<CrabError> = vec![
            CrabError::CorruptObject {
                path: "p".into(),
                reason: "r".into(),
            },
            CrabError::ChunkNotFound { hash: "h".into() },
            CrabError::OriginIntegrity {
                path: "p".into(),
                source: crab_cache::CacheError::CorruptObject {
                    path: "p".into(),
                    reason: "r".into(),
                },
            },
            CrabError::HashMismatch {
                requested: "a".into(),
                actual: "b".into(),
            },
            CrabError::CrcMismatch {
                segment_id: 0,
                offset: 0,
            },
            CrabError::PackIntegrity {
                expected: "a".into(),
                computed: "b".into(),
            },
            CrabError::StagingCorrupt("bad".into()),
        ];
        for err in &cases {
            assert_eq!(err.exit_code(), 4, "expected 4 for {err}");
        }
    }

    #[test]
    fn exit_code_io_and_storage() {
        let io = CrabError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x"));
        assert_eq!(io.exit_code(), 5);
    }

    #[test]
    fn exit_code_config() {
        let err = CrabError::Configuration {
            key: "k".into(),
            origin: "o".into(),
        };
        assert_eq!(err.exit_code(), 6);
    }

    #[test]
    fn exit_code_credentials() {
        assert_eq!(CrabError::NoCredentials.exit_code(), 7);
        assert_eq!(CrabError::AuthFailed { path: "p".into() }.exit_code(), 7);
        assert_eq!(CrabError::AuthExpired { path: "p".into() }.exit_code(), 7);
        assert_eq!(CrabError::Forbidden { path: "p".into() }.exit_code(), 7);
    }

    #[test]
    fn exit_code_incompatible() {
        let err = CrabError::IncompatibleFormat {
            required: "2.0".into(),
            found: "1.0".into(),
        };
        assert_eq!(err.exit_code(), 8);
    }

    #[test]
    fn exit_code_internal() {
        assert_eq!(CrabError::Internal("bug".into()).exit_code(), 9);
    }

    #[test]
    fn exit_code_cancelled() {
        assert_eq!(CrabError::Cancelled.exit_code(), 10);
    }

    #[test]
    fn exit_code_general_user_errors() {
        assert_eq!(CrabError::Protocol("x".into()).exit_code(), 1);
        assert_eq!(CrabError::NotFound { path: "p".into() }.exit_code(), 1);
        assert_eq!(
            CrabError::InsufficientSpace {
                needed: 1,
                available: 0
            }
            .exit_code(),
            1
        );
        assert_eq!(CrabError::Throttled { retry_after: None }.exit_code(), 1);
        assert_eq!(CrabError::StagingLocked { holder_pid: None }.exit_code(), 1);
    }

    #[test]
    fn exit_code_lfs_object_corrupt() {
        let err = CrabError::LfsObjectCorrupt {
            oid: "abc123".into(),
        };
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn exit_code_lfs_general_errors() {
        assert_eq!(
            CrabError::InvalidLfsPointer {
                reason: "bad oid".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            CrabError::LfsObjectMissing {
                oid: "abc123".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            CrabError::LfsLockConflict {
                path: "model.bin".into(),
                owner: "alice".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            CrabError::LfsTransferProtocol("bad json".into()).exit_code(),
            1
        );
        assert_eq!(
            CrabError::LfsMigrationFailed {
                reason: "dirty tree".into()
            }
            .exit_code(),
            1
        );
    }

    // --- code() tests ---

    #[test]
    fn code_returns_expected_codes() {
        assert_eq!(
            CrabError::NetworkTransient(object_store::Error::Generic {
                store: "test",
                source: "net".into(),
            })
            .code(),
            "CRAB-E0001"
        );
        assert_eq!(
            CrabError::Throttled { retry_after: None }.code(),
            "CRAB-E0002"
        );
        assert_eq!(
            CrabError::CasConflict {
                path: "p".into(),
                expected_etag: None,
            }
            .code(),
            "CRAB-E0010"
        );
        assert_eq!(
            CrabError::FileChangedDuringStaging {
                path: "model.bin".into(),
                first_hash: "aaa".into(),
                second_hash: "bbb".into(),
                first_size: 1,
                second_size: 2,
            }
            .code(),
            "CRAB-E0089"
        );
        assert_eq!(CrabError::Cancelled.code(), "CRAB-E0090");
        assert_eq!(
            CrabError::PushIntegrationFailed {
                command: "git pull --rebase --autostash origin main".into(),
                message: "CONFLICT".into(),
            }
            .code(),
            "CRAB-E0097"
        );
        assert_eq!(CrabError::Internal("x".into()).code(), "CRAB-E0099");
        assert_eq!(
            CrabError::CacheService {
                reason: "timeout".into()
            }
            .code(),
            "CRAB-E0110"
        );
    }

    #[test]
    fn code_all_start_with_crab_e() {
        let variants: Vec<CrabError> = all_variants();
        for err in &variants {
            let c = err.code();
            assert!(
                c.starts_with("CRAB-E"),
                "code for {err} should start with CRAB-E, got {c}"
            );
        }
    }

    // --- category() tests ---

    #[test]
    fn category_transient() {
        assert_eq!(
            CrabError::NetworkTransient(object_store::Error::Generic {
                store: "test",
                source: "net".into(),
            })
            .category(),
            ErrorCategory::Transient
        );
        assert_eq!(
            CrabError::Throttled { retry_after: None }.category(),
            ErrorCategory::Transient
        );
    }

    #[test]
    fn category_conflict() {
        assert_eq!(
            CrabError::CasConflict {
                path: "p".into(),
                expected_etag: None,
            }
            .category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            CrabError::NonFastForward {
                ref_name: "r".into(),
                have: "a".into(),
                want: "b".into(),
            }
            .category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            CrabError::FileChangedDuringStaging {
                path: "model.bin".into(),
                first_hash: "aaa".into(),
                second_hash: "bbb".into(),
                first_size: 1,
                second_size: 2,
            }
            .category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            CrabError::PushIntegrationFailed {
                command: "git pull --rebase --autostash origin main".into(),
                message: "CONFLICT".into(),
            }
            .category(),
            ErrorCategory::Conflict
        );
    }

    #[test]
    fn category_permanent() {
        assert_eq!(
            CrabError::NotFound { path: "p".into() }.category(),
            ErrorCategory::Permanent
        );
        assert_eq!(
            CrabError::NoCredentials.category(),
            ErrorCategory::Permanent
        );
        assert_eq!(
            CrabError::BeyondShallowBoundary { oid: "x".into() }.category(),
            ErrorCategory::Permanent
        );
    }

    #[test]
    fn category_lfs() {
        assert_eq!(
            CrabError::InvalidLfsPointer {
                reason: "bad".into()
            }
            .category(),
            ErrorCategory::Lfs
        );
        assert_eq!(
            CrabError::LfsLockConflict {
                path: "f".into(),
                owner: "o".into(),
            }
            .category(),
            ErrorCategory::Lfs
        );
    }

    #[test]
    fn category_cancelled() {
        assert_eq!(CrabError::Cancelled.category(), ErrorCategory::Cancelled);
    }

    // --- is_retryable() tests ---

    #[test]
    fn is_retryable_true_variants() {
        assert!(
            CrabError::NetworkTransient(object_store::Error::Generic {
                store: "test",
                source: "net".into(),
            })
            .is_retryable()
        );
        assert!(CrabError::Throttled { retry_after: None }.is_retryable());
        assert!(
            CrabError::CasConflict {
                path: "p".into(),
                expected_etag: None,
            }
            .is_retryable()
        );
        assert!(
            CrabError::PushLockHeld {
                ref_name: "refs/heads/main".into(),
                holder: "agent-a".into(),
                expires_at_unix: Some(120),
            }
            .is_retryable()
        );
        assert!(CrabError::StagingLocked { holder_pid: None }.is_retryable());
    }

    #[test]
    fn is_retryable_false_for_others() {
        assert!(!CrabError::Cancelled.is_retryable());
        assert!(!CrabError::Internal("x".into()).is_retryable());
        assert!(
            !CrabError::PushIntegrationFailed {
                command: "git pull --rebase --autostash origin main".into(),
                message: "CONFLICT".into(),
            }
            .is_retryable()
        );
        assert!(!CrabError::NotFound { path: "p".into() }.is_retryable());
        assert!(!CrabError::NoCredentials.is_retryable());
        assert!(
            !CrabError::FileChangedDuringStaging {
                path: "model.bin".into(),
                first_hash: "aaa".into(),
                second_hash: "bbb".into(),
                first_size: 1,
                second_size: 2,
            }
            .is_retryable()
        );
    }

    // --- details_json() tests ---

    #[test]
    fn details_json_null_for_unit_variants() {
        assert!(CrabError::NoCredentials.details_json().is_null());
        assert!(CrabError::Cancelled.details_json().is_null());
    }

    #[test]
    fn details_json_struct_fields() {
        let err = CrabError::NonFastForward {
            ref_name: "refs/heads/main".into(),
            have: "aaa".into(),
            want: "bbb".into(),
        };
        let d = err.details_json();
        assert_eq!(d["ref_name"], "refs/heads/main");
        assert_eq!(d["have"], "aaa");
        assert_eq!(d["want"], "bbb");

        let err = CrabError::FileChangedDuringStaging {
            path: "model.bin".into(),
            first_hash: "aaa".into(),
            second_hash: "bbb".into(),
            first_size: 1,
            second_size: 2,
        };
        let d = err.details_json();
        assert_eq!(d["path"], "model.bin");
        assert_eq!(d["first_hash"], "aaa");
        assert_eq!(d["second_hash"], "bbb");
        assert_eq!(d["first_size"], 1);
        assert_eq!(d["second_size"], 2);

        let err = CrabError::PushIntegrationFailed {
            command: "git pull --rebase --autostash origin main".into(),
            message: "CONFLICT".into(),
        };
        let d = err.details_json();
        assert_eq!(d["command"], "git pull --rebase --autostash origin main");
        assert_eq!(d["message"], "CONFLICT");
    }

    #[test]
    fn details_json_numeric_fields() {
        let err = CrabError::InsufficientSpace {
            needed: 1024,
            available: 512,
        };
        let d = err.details_json();
        assert_eq!(d["needed"], 1024);
        assert_eq!(d["available"], 512);
    }

    #[test]
    fn details_json_optional_field() {
        let err = CrabError::Throttled {
            retry_after: Some(Duration::from_millis(500)),
        };
        let d = err.details_json();
        assert_eq!(d["retry_after_ms"], 500);
    }

    // --- IncompleteShardReconstruction catalog invariants ---

    #[test]
    fn incomplete_shard_reconstruction_error_catalog() {
        // With a path — the default rendering includes the path fragment.
        let err = CrabError::IncompleteShardReconstruction {
            file_hash: "abc".into(),
            path: Some("dir/file.bin".into()),
            uncovered_chunks: 3,
            example_chunk_hash: "def".into(),
            example_chunk_index: 42,
        };
        assert_eq!(err.code(), "CRAB-E0085");
        assert_eq!(err.category(), ErrorCategory::Integrity);
        assert!(!err.is_retryable());
        assert_eq!(err.exit_code(), 4);

        let rendered = err.to_string();
        assert!(
            rendered.contains("abc"),
            "display missing file_hash: {rendered}"
        );
        assert!(
            rendered.contains("42"),
            "display missing example_chunk_index: {rendered}"
        );
        assert!(
            rendered.contains("(path dir/file.bin)"),
            "display missing path fragment: {rendered}"
        );

        // Without a path — catalog invariants unchanged; path fragment absent.
        let err_no_path = CrabError::IncompleteShardReconstruction {
            file_hash: "abc".into(),
            path: None,
            uncovered_chunks: 3,
            example_chunk_hash: "def".into(),
            example_chunk_index: 42,
        };
        assert_eq!(err_no_path.code(), "CRAB-E0085");
        assert_eq!(err_no_path.category(), ErrorCategory::Integrity);
        assert!(!err_no_path.is_retryable());
        assert_eq!(err_no_path.exit_code(), 4);

        let details = err_no_path.details_json();
        assert_eq!(details["file_hash"], "abc");
        assert_eq!(details["uncovered_chunks"], 3);
        assert_eq!(details["example_chunk_index"], 42);
        assert_eq!(details["example_chunk_hash"], "def");
        assert!(details["path"].is_null());

        let rendered_no_path = err_no_path.to_string();
        assert!(
            !rendered_no_path.contains("(path"),
            "display should omit path fragment when path is None: {rendered_no_path}"
        );
    }

    // --- workflow variant coverage ---

    /// Every new workflow variant gets its own sequential code in the
    /// `E02xx` range. A regression here (duplicate or missing code)
    /// would silently break downstream tooling that pivots on the code.
    #[test]
    fn workflow_variants_have_unique_codes() {
        // These are the formatted codes, appended sequentially to the
        // existing catalog. The E0200..E0234 range is treated as a
        // decimal-formatted sequence, not a hex one.
        let expected: Vec<&'static str> = vec![
            "CRAB-E0200",
            "CRAB-E0201",
            "CRAB-E0202",
            "CRAB-E0203",
            "CRAB-E0204",
            "CRAB-E0205",
            "CRAB-E0206",
            "CRAB-E0207",
            "CRAB-E0208",
            "CRAB-E0209",
            "CRAB-E0210",
            "CRAB-E0211",
            "CRAB-E0212",
            "CRAB-E0213",
            "CRAB-E0214",
            "CRAB-E0215",
            "CRAB-E0216",
            "CRAB-E0217",
            "CRAB-E0218",
            "CRAB-E0219",
            "CRAB-E0220",
            "CRAB-E0221",
            "CRAB-E0222",
            "CRAB-E0223",
            "CRAB-E0224",
            "CRAB-E0225",
            "CRAB-E0226",
            "CRAB-E0227",
            "CRAB-E0228",
            "CRAB-E0229",
            "CRAB-E0230",
            "CRAB-E0231",
            "CRAB-E0232",
            "CRAB-E0233",
            "CRAB-E0234",
            "CRAB-E0240",
            "CRAB-E0241",
            "CRAB-E0242",
            "CRAB-E0243",
            "CRAB-E0244",
            "CRAB-E0245",
            "CRAB-E0246",
            "CRAB-E0247",
            "CRAB-E0248",
        ];

        let variants = all_variants();
        let observed: std::collections::HashSet<&str> = variants
            .iter()
            .map(|e| e.code())
            .filter(|c| c.starts_with("CRAB-E02"))
            .collect();

        for want in &expected {
            assert!(
                observed.contains(want),
                "missing workflow code {want}; present codes: {observed:?}"
            );
        }

        // Uniqueness: each workflow code appears on exactly one variant.
        let mut seen = std::collections::HashSet::new();
        for err in &variants {
            let c = err.code();
            if c.starts_with("CRAB-E02") {
                assert!(seen.insert(c), "duplicate workflow code {c}");
            }
        }
    }

    #[test]
    fn workflow_variants_classify_into_expected_categories() {
        assert_eq!(
            CrabError::WorkflowParse {
                path: PathBuf::from("w.yaml"),
                source: serde_yaml::from_str::<serde_yaml::Value>(":\n  -").unwrap_err(),
            }
            .category(),
            ErrorCategory::Config,
        );
        assert_eq!(
            CrabError::StageExecFailed {
                stage: "s".into(),
                exit_code: 1,
            }
            .category(),
            ErrorCategory::Permanent,
        );
        assert_eq!(
            CrabError::StageOverwriteConflict {
                stage: "s".into(),
                path: PathBuf::from("out"),
                reason: "conflict",
            }
            .category(),
            ErrorCategory::Conflict,
        );
        assert_eq!(
            CrabError::WorkflowJournalCorrupt {
                run_id: "r".into(),
                detail: "d".into(),
            }
            .category(),
            ErrorCategory::Integrity,
        );
        assert_eq!(
            CrabError::StageExecTimeout {
                stage: "s".into(),
                elapsed_ms: 1000,
            }
            .category(),
            ErrorCategory::Transient,
        );
        assert_eq!(
            CrabError::WorkflowJournalOpen {
                path: PathBuf::from("j.db"),
                source: rusqlite::Error::InvalidQuery,
            }
            .category(),
            ErrorCategory::Transport,
        );
    }

    #[test]
    fn workflow_variants_details_json() {
        let err = CrabError::StageOutTooLarge {
            stage: "train".into(),
            path: PathBuf::from("out/model.bin"),
            size: 2_000_000,
            limit: 1_000_000,
        };
        let d = err.details_json();
        assert_eq!(d["stage"], "train");
        assert_eq!(d["path"], "out/model.bin");
        assert_eq!(d["size"], 2_000_000);
        assert_eq!(d["limit"], 1_000_000);

        let drift = CrabError::WorkflowResumeFilesystemDrift {
            stage: "train".into(),
            expected: "blake3:abc".into(),
            observed: "blake3:def".into(),
        };
        let d = drift.details_json();
        assert_eq!(d["stage"], "train");
        assert_eq!(d["expected"], "blake3:abc");
        assert_eq!(d["observed"], "blake3:def");

        // Unit variants serialize as null so they round-trip to an
        // omitted `details` key in the JSON envelope.
        assert!(CrabError::WorkflowDisabled.details_json().is_null());
        assert!(
            CrabError::StageRemoteExecutionUnsupported
                .details_json()
                .is_null()
        );

        // `StageHermeticNotImplemented` carries the offending stage
        // name so structured-output consumers can surface it directly.
        let hnm = CrabError::StageHermeticNotImplemented {
            stage: "train".into(),
        };
        let d = hnm.details_json();
        assert_eq!(d["stage"], "train");

        // Lock timeout carries an optional PID.
        let lt = CrabError::WorkflowLockTimeout {
            held_by: Some(4321),
            waited_ms: 30_000,
        };
        let d = lt.details_json();
        assert_eq!(d["held_by"], 4321);
        assert_eq!(d["waited_ms"], 30_000);
    }

    /// Helper: one instance of every `CrabError` variant for exhaustive tests.
    fn all_variants() -> Vec<CrabError> {
        vec![
            CrabError::from(crab_read::ReadError::availability(CrabError::NoCredentials)),
            CrabError::OriginIntegrity {
                path: "p".into(),
                source: crab_cache::CacheError::CorruptObject {
                    path: "p".into(),
                    reason: "r".into(),
                },
            },
            CrabError::NetworkTransient(object_store::Error::Generic {
                store: "test",
                source: "net".into(),
            }),
            CrabError::Throttled { retry_after: None },
            CrabError::CasConflict {
                path: "p".into(),
                expected_etag: None,
            },
            CrabError::NonFastForward {
                ref_name: "r".into(),
                have: "a".into(),
                want: "b".into(),
            },
            CrabError::RefAlreadyExists { name: "r".into() },
            CrabError::PushLockHeld {
                ref_name: "r".into(),
                holder: "h".into(),
                expires_at_unix: None,
            },
            CrabError::FileChangedDuringStaging {
                path: "model.bin".into(),
                first_hash: "aaa".into(),
                second_hash: "bbb".into(),
                first_size: 1,
                second_size: 2,
            },
            CrabError::CorruptObject {
                path: "p".into(),
                reason: "r".into(),
            },
            CrabError::ChunkNotFound { hash: "h".into() },
            CrabError::NotFound { path: "p".into() },
            CrabError::Forbidden { path: "p".into() },
            CrabError::NoCredentials,
            CrabError::InsufficientSpace {
                needed: 1,
                available: 0,
            },
            CrabError::AuthFailed { path: "p".into() },
            CrabError::AuthExpired { path: "p".into() },
            CrabError::Configuration {
                key: "k".into(),
                origin: "o".into(),
            },
            CrabError::IncompatibleFormat {
                required: "2".into(),
                found: "1".into(),
            },
            CrabError::Protocol("x".into()),
            CrabError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x")),
            CrabError::Storage(object_store::Error::Generic {
                store: "test",
                source: "s".into(),
            }),
            CrabError::StagingCorrupt("bad".into()),
            CrabError::StagingLocked { holder_pid: None },
            CrabError::HashMismatch {
                requested: "a".into(),
                actual: "b".into(),
            },
            CrabError::CrcMismatch {
                segment_id: 0,
                offset: 0,
            },
            CrabError::PackIntegrity {
                expected: "a".into(),
                computed: "b".into(),
            },
            CrabError::IncompleteShardReconstruction {
                file_hash: "f".into(),
                path: Some("dir/file.bin".into()),
                uncovered_chunks: 1,
                example_chunk_hash: "c".into(),
                example_chunk_index: 0,
            },
            CrabError::BeyondShallowBoundary { oid: "x".into() },
            CrabError::PackTooLarge {
                size: 3 * 1024 * 1024 * 1024,
                limit: 2 * 1024 * 1024 * 1024,
            },
            CrabError::PushIntegrationFailed {
                command: "git pull --rebase --autostash origin main".into(),
                message: "CONFLICT (content): Merge conflict".into(),
            },
            CrabError::Cancelled,
            CrabError::Internal("bug".into()),
            CrabError::InvalidLfsPointer {
                reason: "bad".into(),
            },
            CrabError::LfsObjectCorrupt {
                oid: "abc123".into(),
            },
            CrabError::LfsObjectMissing {
                oid: "abc123".into(),
            },
            CrabError::LfsLockConflict {
                path: "f".into(),
                owner: "o".into(),
            },
            CrabError::LfsTransferProtocol("bad".into()),
            CrabError::LfsMigrationFailed {
                reason: "dirty".into(),
            },
            CrabError::CacheService {
                reason: "timeout".into(),
            },
            CrabError::ImportSourceMustBeRaw {
                url: "crab://bucket/repo".into(),
            },
            CrabError::ImportSchemeMismatch {
                from_scheme: "s3".into(),
                to_scheme: "az".into(),
            },
            CrabError::ImportPlanMismatch {
                recorded: "a".into(),
                provided: "b".into(),
            },
            CrabError::ImportVersioningUnavailable {
                url: "s3://bucket/prefix".into(),
            },
            CrabError::ImportCommitCeilingExceeded {
                planned: 150_000,
                ceiling: 100_000,
            },
            CrabError::ImportInvalidHistoryRange {
                since: "2025-02-01T00:00:00Z".into(),
                until: "2025-01-01T00:00:00Z".into(),
            },
            CrabError::ImportTargetNotEmpty {
                path: "./repo".into(),
            },
            CrabError::ImportMissingGitIdentity,
            CrabError::ImportRemoteExists {
                existing_url: "https://old.example/repo".into(),
                new_url: "crab://bucket/repo".into(),
            },
            CrabError::WorkflowParse {
                path: PathBuf::from("workflow.yaml"),
                source: serde_yaml::from_str::<serde_yaml::Value>(":\n  -").unwrap_err(),
            },
            CrabError::WorkflowCycle {
                stages: vec!["a".into(), "b".into(), "a".into()],
            },
            CrabError::WorkflowUndefinedOut {
                consumer: "train".into(),
                out: "dataset".into(),
            },
            CrabError::WorkflowStageNameInvalid {
                name: "bad name".into(),
                reason: "whitespace not allowed",
            },
            CrabError::WorkflowDiscoveryAmbiguous {
                candidates: vec![
                    PathBuf::from("a/workflow.yaml"),
                    PathBuf::from("b/workflow.yaml"),
                ],
            },
            CrabError::StageDepMissing {
                stage: "train".into(),
                path: PathBuf::from("data/input.csv"),
            },
            CrabError::StageDepMalformed {
                stage: "train".into(),
                path: PathBuf::from("data/input.csv"),
                reason: "not utf-8",
            },
            CrabError::StageOutMalformed {
                stage: "train".into(),
                path: PathBuf::from("out/model.bin"),
                reason: "zero bytes",
            },
            CrabError::StageOutTooLarge {
                stage: "train".into(),
                path: PathBuf::from("out/model.bin"),
                size: 2_000_000,
                limit: 1_000_000,
            },
            CrabError::StageOutCountExceeded {
                stage: "sweep".into(),
                count: 1024,
                limit: 256,
            },
            CrabError::StageEnvMissing {
                stage: "train".into(),
                var: "OPENAI_API_KEY".into(),
            },
            CrabError::StageExecFailed {
                stage: "train".into(),
                exit_code: 1,
            },
            CrabError::StageExecSignaled {
                stage: "train".into(),
                signal: 9,
            },
            CrabError::StageExecTimeout {
                stage: "train".into(),
                elapsed_ms: 60_000,
            },
            CrabError::StageDiskFull {
                stage: "train".into(),
                path: PathBuf::from("/tmp/out"),
            },
            CrabError::StageCacheMiss {
                stage: "train".into(),
                reason: "cold cache".into(),
            },
            CrabError::StageRetryExhausted {
                stage: "train".into(),
                attempts: 3,
            },
            CrabError::StageOverwriteConflict {
                stage: "train".into(),
                path: PathBuf::from("out/model.bin"),
                reason: "different content",
            },
            CrabError::StageSideEffectsRetryLimit {
                stage: "publish".into(),
            },
            CrabError::StageSideEffectHookFailed {
                stage: "notify".into(),
                exit_code: 1,
            },
            CrabError::LockfileStale {
                stage: "train".into(),
            },
            CrabError::LockfileCanonicalizationFailed {
                path: PathBuf::from("workflow.lock"),
                source: serde_yaml::from_str::<serde_yaml::Value>(":\n  -").unwrap_err(),
            },
            CrabError::LockfileMergeConflict {
                path: PathBuf::from("workflow.lock"),
            },
            CrabError::ExperimentNotFound {
                id: "exp-42".into(),
            },
            CrabError::ExperimentCollision {
                id: "exp-42".into(),
            },
            CrabError::MetricsSchemaMismatch {
                path: PathBuf::from("metrics.json"),
                source: serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
            },
            CrabError::WorkflowJournalOpen {
                path: PathBuf::from("journal.db"),
                source: rusqlite::Error::InvalidQuery,
            },
            CrabError::WorkflowJournalCorrupt {
                run_id: "run-1".into(),
                detail: "unexpected column".into(),
            },
            CrabError::WorkflowJournalSchemaNewer {
                run_id: "run-1".into(),
                found: 5,
                supported: 3,
            },
            CrabError::WorkflowResumeFilesystemDrift {
                stage: "train".into(),
                expected: "blake3:abc".into(),
                observed: "blake3:def".into(),
            },
            CrabError::WorkflowStateTransitionIllegal {
                stage: "train".into(),
                from: "completed".into(),
                to: "pending".into(),
            },
            CrabError::WorkflowLockTimeout {
                held_by: Some(4321),
                waited_ms: 30_000,
            },
            CrabError::WorkflowDisabled,
            CrabError::WorkflowHermeticViolation {
                stage: "train".into(),
                path: PathBuf::from("/etc/passwd"),
            },
            CrabError::CacheEntrySchemaNewer {
                stage_hash: "abc123".into(),
                found: 9,
                supported: 4,
            },
            CrabError::StageRemoteExecutionUnsupported,
            CrabError::StageHermeticNotImplemented {
                stage: "train".into(),
            },
            CrabError::WorkflowDuplicateOutput {
                first: "clean".into(),
                second: "transform".into(),
                path: PathBuf::from("data/clean.parquet"),
            },
            CrabError::WorkflowExperimentIdInvalid {
                raw: "not-a-uuid".into(),
                reason: "invalid format",
            },
            CrabError::WorkflowExperimentMetadataSchemaNewer {
                id: "exp-1".into(),
                found: 5,
                supported: 3,
            },
            CrabError::CacheEntryCorrupt {
                stage_hash: "abc123".into(),
                path: "model.bin".into(),
                expected: "b3:expected".into(),
                actual: "b3:actual".into(),
            },
            CrabError::CacheEntryHashMismatch {
                manifest_hash: "b3:manifest".into(),
                local_hash: "b3:local".into(),
            },
            CrabError::RemoteCacheReadonly,
            CrabError::WorkflowValidationError {
                field: "stage 'train' retry.max_attempts".into(),
                value: "0".into(),
                expected: "integer >= 1".into(),
            },
            CrabError::WorkflowSelfLoop {
                stage: "train".into(),
                path: PathBuf::from("data.csv"),
            },
            CrabError::JournalDiskFull {
                path: PathBuf::from("/tmp/journal.db"),
            },
            CrabError::WorkflowTemplateUndefined {
                key: "model.lr".into(),
                field: "cmd".into(),
                stage: "train".into(),
            },
            CrabError::WorkflowForeachEmpty {
                stage: "preprocess".into(),
            },
            CrabError::WorkflowMatrixEmpty {
                stage: "train".into(),
                variable: "dataset".into(),
            },
            CrabError::TierLifecycleConflict {
                prefix: ".crab/xorbs/".into(),
                existing_id: "crab-ia-30".into(),
                new_id: "crab-ia-60".into(),
            },
            CrabError::TierApplyUnauthorized {
                required_permission: "s3:PutLifecycleConfiguration".into(),
            },
            CrabError::TierProviderUnsupported {
                provider: "r2".into(),
            },
            CrabError::ArchiveRestoreRequired {
                xorb: "abc123".into(),
                class: "GlacierDeepArchive".into(),
                estimated_eta: Some("2026-04-01T12:00:00Z".into()),
            },
            CrabError::ArchiveRestoreTimeout {
                xorb: "abc123".into(),
                class: "GlacierFlexible".into(),
                elapsed_secs: 21600,
            },
            CrabError::RestoreTierUnsupported {
                tier: "Expedited".into(),
                class: "GlacierDeepArchive".into(),
                supported: vec!["Standard".into(), "Bulk".into()],
            },
            CrabError::GcEarlyDeleteBlocked {
                class: "Standard-IA".into(),
                age_days: 15,
                min_days: 30,
                penalty_usd: "1.23".into(),
            },
            CrabError::ObjectLockedRetention {
                path: ".crab/xorbs/abc123".into(),
                until: "2026-12-31T00:00:00Z".into(),
            },
            CrabError::GcPartialFailure {
                objects_deleted: 3,
                delete_failures: 1,
                reconciliation_failed: false,
                source: Box::new(CrabError::Internal("delete failed".into())),
            },
            CrabError::OptimizeXorbsProfileOutOfRange {
                name: "custom".into(),
                bytes: 3_221_225_472,
            },
            CrabError::OptimizeXorbsCorruptSource {
                xorb: "def456".into(),
            },
            CrabError::OptimizeXorbsAlreadyInProgress {
                pid: 12345,
                started_at: "2026-03-15T10:00:00Z".into(),
            },
            CrabError::ConcurrentMaintenance { other: "gc" },
            CrabError::CostPricingMissing {
                provider: "aws".into(),
                region: "ap-southeast-3".into(),
            },
            CrabError::CostInventoryReportStale {
                provider: "aws".into(),
                report_at: "2026-01-01T00:00:00Z".into(),
                max_hours: 48,
            },
        ]
    }

    #[test]
    fn hint_returns_none_for_storage_economy_variants() {
        let cases: Vec<CrabError> = vec![
            CrabError::TierLifecycleConflict {
                prefix: ".crab/xorbs/".into(),
                existing_id: "crab-ia".into(),
                new_id: "crab-ia-v2".into(),
            },
            CrabError::ArchiveRestoreRequired {
                xorb: "abc".into(),
                class: "Glacier".into(),
                estimated_eta: None,
            },
            CrabError::OptimizeXorbsProfileOutOfRange {
                name: "ml".into(),
                bytes: 3_000_000_000,
            },
            CrabError::CostPricingMissing {
                provider: "aws".into(),
                region: "us-east-1".into(),
            },
        ];
        for err in &cases {
            assert_eq!(err.hint(), None, "hint() should be None for {}", err.code());
            assert_eq!(
                err.docs_anchor(),
                None,
                "docs_anchor() should be None for {}",
                err.code()
            );
        }
    }

    #[test]
    fn setup_failures_have_actionable_help() {
        let cases = [
            CrabError::NotFound {
                path: "team/models/layout".into(),
            },
            CrabError::Forbidden {
                path: "team/models/layout".into(),
            },
            CrabError::NoCredentials,
            CrabError::AuthExpired {
                path: "team/models/layout".into(),
            },
            CrabError::Configuration {
                key: "remote".into(),
                origin: "crab.toml".into(),
            },
        ];

        for err in &cases {
            assert!(err.hint().is_some(), "{} should have a hint", err.code());
            assert!(
                err.docs_anchor().is_some(),
                "{} should have a docs path",
                err.code()
            );
        }
    }

    // --- gitoxide surface wrappers ---
    //
    // Every new `Gix*` variant wraps a `gix-*` error via `#[from]`.
    // The tests below assert two guarantees:
    //
    //   1. Display renders with the `"gix <crate>: <inner>"` prefix
    //      declared by the `#[error]` attribute, and includes the
    //      wrapped error's message (so the source chain is visible
    //      in logs).
    //   2. `std::error::Error::source()` returns `Some(_)` pointing
    //      at the wrapped gix error, proving the chain is preserved
    //      for programmatic branching (downcast).
    //
    // Each test constructs a minimal, real instance of the wrapped
    // gix error via a public constructor — no `Debug`-only hacks.

    /// Assert the wrapped error surfaces through `Error::source()`
    /// with the expected concrete type.
    fn assert_source_is<T: std::error::Error + 'static>(err: &CrabError) {
        let src = std::error::Error::source(err)
            .expect("wrapped gix error should be exposed via source()");
        assert!(
            src.downcast_ref::<T>().is_some(),
            "source() should downcast to the wrapped gix type ({}); got: {src}",
            std::any::type_name::<T>()
        );
    }

    #[test]
    fn git_walk_preserves_lookup_source_and_scan_outcomes() {
        let error: CrabError = crab_git::walk::WalkError::Git {
            operation: "read pointer candidate".to_owned(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            )),
        }
        .into();
        let CrabError::Io(wrapper) = &error else {
            panic!("expected retained I/O source")
        };
        let walk = wrapper
            .get_ref()
            .unwrap()
            .downcast_ref::<crab_git::walk::WalkError>()
            .unwrap();
        let source = std::error::Error::source(walk)
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(error.code(), "CRAB-E0070");
        assert!(matches!(
            CrabError::from(crab_git::walk::WalkError::Cancelled),
            CrabError::Cancelled
        ));
        assert!(matches!(
            CrabError::from(crab_git::walk::WalkError::LookupLimitExceeded { maximum: 10 }),
            CrabError::Protocol(_)
        ));
    }

    #[test]
    fn gix_ref_display_and_source() {
        // `file::find::Error::ReadFileContents` carries a concrete
        // `io::Error` plus a path — constructable in plain code.
        let inner = gix_ref::file::find::Error::ReadFileContents {
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            path: std::path::PathBuf::from("refs/heads/main"),
        };
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix ref: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0600");
        assert_source_is::<gix_ref::file::find::Error>(&err);
    }

    #[test]
    fn gix_object_display_and_source() {
        // A genuine `gix_object::decode::Error` comes out of parsing
        // garbage bytes as a commit.
        let inner = gix_object::CommitRef::from_bytes(b"not a commit", gix_hash::Kind::Sha1)
            .expect_err("parsing garbage as a commit must fail");
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix object: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0601");
        assert_source_is::<gix_object::decode::Error>(&err);
    }

    #[test]
    fn gix_pack_display_and_source() {
        let inner = gix_pack::data::decode::Error::OutOfMemory;
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix pack: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0602");
        assert_source_is::<gix_pack::data::decode::Error>(&err);
    }

    #[test]
    fn pack_and_repack_preserve_corrupt_index_diagnostics() {
        use crab_git::pack_locator::{PackLocatorError, write_pack_reverse_index};

        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("pack.idx");
        let reverse = dir.path().join("pack.rev");
        std::fs::write(&index, b"not a valid git pack index\n").unwrap();
        for repack in [false, true] {
            let source = write_pack_reverse_index(&index, &reverse).unwrap_err();
            let error = if repack {
                CrabError::from(crab_git::repack::RepackError::Locator { source })
            } else {
                CrabError::from(crab_git::pack::PackError::ReverseIndex { source })
            };
            assert_eq!(error.code(), "CRAB-E0020");
            assert_eq!(crate::core::error_catalog::error_code(&error), error.code());
            assert_eq!(error.exit_code(), 4);
            assert_eq!(error.category(), ErrorCategory::Integrity);
            assert!(!error.is_retryable());
            assert!(
                error
                    .to_string()
                    .contains("too small for even an empty index")
            );
            assert_source_is::<PackLocatorError>(&error);
            let source = std::error::Error::source(&error).unwrap();
            assert!(matches!(
                source.source().unwrap().downcast_ref(),
                Some(gix_pack::index::init::Error::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn pack_locator_io_retains_kind_and_typed_cause() {
        use crab_git::pack_locator::{PackLocatorError, write_pack_reverse_index};

        let dir = tempfile::tempdir().unwrap();
        let missing = write_pack_reverse_index(
            &dir.path().join("missing.idx"),
            &dir.path().join("pack.rev"),
        )
        .unwrap_err();
        let denied = PackLocatorError::ReverseIndexIo {
            path: dir.path().join("denied.rev"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        for (source, kind) in [
            (missing, std::io::ErrorKind::NotFound),
            (denied, std::io::ErrorKind::PermissionDenied),
        ] {
            let error = CrabError::from(source);
            let CrabError::Io(wrapper) = &error else {
                panic!("expected I/O classification: {error}");
            };
            assert_eq!(wrapper.kind(), kind);
            assert!(wrapper.get_ref().unwrap().is::<PackLocatorError>());
        }
    }

    #[test]
    fn pack_index_checksum_distinguishes_interruption_from_corruption() {
        use crab_git::pack_locator::PackLocatorError;
        use gix_pack::index::verify::checksum::Error;

        let interrupted = CrabError::from(PackLocatorError::IndexChecksum {
            path: "pack.idx".into(),
            source: Error::Interrupted,
        });
        assert!(matches!(interrupted, CrabError::Cancelled));
        let error = CrabError::from(PackLocatorError::IndexChecksum {
            path: "pack.idx".into(),
            source: Error::Verify(gix_hash::verify::Error {
                actual: gix_hash::ObjectId::from_bytes_or_panic(&[1; 20]),
                expected: gix_hash::ObjectId::from_bytes_or_panic(&[2; 20]),
            }),
        });
        assert_eq!(error.code(), "CRAB-E0020");
        assert_source_is::<PackLocatorError>(&error);
        let source = std::error::Error::source(&error).unwrap();
        assert!(matches!(
            source.source().unwrap().downcast_ref(),
            Some(Error::Verify(_))
        ));
    }

    #[test]
    fn gix_transport_display_and_source() {
        let inner = gix_transport::client::Error::MissingHandshake;
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix transport: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0603");
        assert_source_is::<gix_transport::client::Error>(&err);
    }

    #[test]
    fn gix_protocol_display_and_source() {
        let inner = gix_protocol::fetch::Error::ReadRemainingBytes(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short read",
        ));
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix protocol: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0604");
        assert_source_is::<gix_protocol::fetch::Error>(&err);
    }

    #[test]
    fn gix_filter_handshake_display_and_source() {
        let inner = gix_filter::driver::process::server::handshake::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "client disconnected",
        ));
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix filter handshake: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0605");
        assert_source_is::<gix_filter::driver::process::server::handshake::Error>(&err);
    }

    #[test]
    fn gix_filter_request_display_and_source() {
        let inner = gix_filter::driver::process::server::next_request::Error::Io(
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"),
        );
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix filter request: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0606");
        assert_source_is::<gix_filter::driver::process::server::next_request::Error>(&err);
    }

    #[test]
    fn gix_worktree_display_and_source() {
        let inner = gix_worktree_state::checkout::Error::IllformedUtf8 {
            path: b"bad\xffpath".as_slice().into(),
        };
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix worktree: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0607");
        assert_source_is::<gix_worktree_state::checkout::Error>(&err);
    }

    #[test]
    fn gix_config_display_and_source() {
        // Unterminated section header is a reliable parse failure.
        let inner = gix_config::parse::Events::from_bytes_owned(b"[section", None)
            .expect_err("malformed config must fail to parse");
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix config: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0608");
        assert_source_is::<gix_config::parse::Error>(&err);
    }

    #[test]
    fn gix_credentials_display_and_source() {
        let inner = gix_credentials::protocol::Error::UrlMissing;
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix credentials: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E0609");
        assert_source_is::<gix_credentials::protocol::Error>(&err);
    }

    #[test]
    fn gix_status_display_and_source() {
        let inner = gix_status::index_as_worktree::Error::IllformedUtf8;
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix status: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E060A");
        assert_source_is::<gix_status::index_as_worktree::Error>(&err);
    }

    #[test]
    fn gix_revwalk_display_and_source() {
        // `insert_parents::Error::Decode` wraps a `gix_object::decode::Error`,
        // which we can construct the same way as the `GixObject` test.
        let inner_decode = gix_object::CommitRef::from_bytes(b"not a commit", gix_hash::Kind::Sha1)
            .expect_err("parsing garbage must fail");
        let inner: gix_revwalk::graph::insert_parents::Error = inner_decode.into();
        let err: CrabError = inner.into();
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("gix revwalk: "),
            "unexpected display: {rendered}"
        );
        assert_eq!(err.code(), "CRAB-E060B");
        assert_source_is::<gix_revwalk::graph::insert_parents::Error>(&err);
    }

    #[test]
    fn git_tag_display_and_source() {
        let inner = crab_git::tag::TagPeelError::MissingObject {
            tag_oid: "a".repeat(40),
            object_oid: "b".repeat(40),
        };
        let err: CrabError = inner.into();

        assert!(err.to_string().starts_with("git tag discovery: "));
        assert_eq!(err.code(), "CRAB-E060C");
        assert_source_is::<crab_git::tag::TagPeelError>(&err);
    }

    #[test]
    fn git_wrapper_codes_are_in_e06xx_range_and_unique() {
        // Construct one of each via `#[from]` to confirm the code
        // catalog entries line up.
        let variants: Vec<CrabError> = vec![
            gix_ref::file::find::Error::ReadFileContents {
                source: std::io::Error::new(std::io::ErrorKind::Other, "x"),
                path: "refs/heads/main".into(),
            }
            .into(),
            gix_object::CommitRef::from_bytes(b"x", gix_hash::Kind::Sha1)
                .unwrap_err()
                .into(),
            gix_pack::data::decode::Error::OutOfMemory.into(),
            gix_transport::client::Error::MissingHandshake.into(),
            gix_protocol::fetch::Error::ReadRemainingBytes(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "x",
            ))
            .into(),
            gix_filter::driver::process::server::handshake::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "x",
            ))
            .into(),
            gix_filter::driver::process::server::next_request::Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "x",
            ))
            .into(),
            gix_worktree_state::checkout::Error::IllformedUtf8 {
                path: b"x".as_slice().into(),
            }
            .into(),
            gix_config::parse::Events::from_bytes_owned(b"[section", None)
                .unwrap_err()
                .into(),
            gix_credentials::protocol::Error::UrlMissing.into(),
            gix_status::index_as_worktree::Error::IllformedUtf8.into(),
            {
                let decode =
                    gix_object::CommitRef::from_bytes(b"x", gix_hash::Kind::Sha1).unwrap_err();
                let walk: gix_revwalk::graph::insert_parents::Error = decode.into();
                walk.into()
            },
            crab_git::tag::TagPeelError::MissingObject {
                tag_oid: "a".repeat(40),
                object_oid: "b".repeat(40),
            }
            .into(),
        ];

        let mut seen = std::collections::HashSet::new();
        for err in &variants {
            let c = err.code();
            assert!(
                c.starts_with("CRAB-E060"),
                "git wrapper code should live in E060x range; got {c}"
            );
            assert!(seen.insert(c), "duplicate code {c} in git wrapper catalog");
        }
        assert_eq!(seen.len(), 13);
    }

    #[test]
    fn managed_repository_diagnostics_are_structured_and_redacted() {
        let err = CrabError::ManagedRepository {
            diagnostic: crab_auth_store::ManagedRepositoryDiagnostic::Forbidden {
                canonical_url: "crab://crab.build/acme/models".to_owned(),
            },
        };

        assert_eq!(err.code(), "CRAB-E0140");
        assert_eq!(err.category(), ErrorCategory::Permanent);
        assert_eq!(err.exit_code(), 7);
        assert!(!err.is_retryable());
        assert_eq!(err.details_json()["kind"], "forbidden");
        let rendered = format!("{err:?} {err}");
        assert!(rendered.contains("crab://crab.build/acme/models"));
        assert!(!rendered.contains("bucket"));
        assert!(!rendered.contains("prefix"));
        assert!(!rendered.contains("token"));
    }

    #[test]
    fn managed_service_unavailable_is_retryable_transient() {
        let err = CrabError::ManagedRepository {
            diagnostic: crab_auth_store::ManagedRepositoryDiagnostic::ServiceUnavailable {
                authority: "crab.build".to_owned(),
            },
        };

        assert_eq!(err.category(), ErrorCategory::Transient);
        assert!(err.is_retryable());
    }
}
