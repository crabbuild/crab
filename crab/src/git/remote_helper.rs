//! Git remote helper protocol loop for `crab://` URLs.
//!
//! Implements the line-framed stdin/stdout protocol that Git uses to
//! communicate with remote helpers. Commands are read line-by-line,
//! batched until a blank line, then dispatched.

use std::fmt;
use std::future::Future;
use std::io::Stderr;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::{Instrument, warn};

use crate::audit::default_log_path;
use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode};
use crate::git::fetch::{CommitGraphProvider, FetchConfig, PackInfo, PackStore, run_fetch_batch};
use crate::git::push::{
    PushConfig, PushRejectReason, PushResult, RefPushOutcome,
    configure_active_active_push_coordinator, record_push_audit_event,
};
use crate::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
use crate::git::push_state::PushState;
use crate::storage::StoreLayout;
use crab_metadata::commit_graph::CommitGraphSummary;
use crab_metadata::manifests::{Manifest, PackEntry, PackList};
use crab_metadata::pack_metadata::PackMetadata;

pub(crate) const AGENT_REBASE_FETCH_REF_FILTERING_ENV: &str =
    "CRAB_INTERNAL_AGENT_FETCH_REF_FILTERING";

#[derive(Debug, Serialize)]
struct PushResultEventPayload {
    refs: Vec<PushResultRefPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordinator_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_state: Option<String>,
}

#[derive(Debug, Serialize)]
struct PushResultRefPayload {
    dst: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A ref entry returned by the object store: a SHA and its ref name.
///
/// `peeled` carries the target-commit SHA for annotated tags so the
/// `list` response can emit the `{peeled} {ref}^{{}}\n` line git
/// expects for tag disambiguation. Non-tag refs and lightweight tags
/// leave it `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub sha: String,
    pub ref_name: String,
    pub peeled: Option<String>,
}

/// Output of the `list` command: concrete refs plus an optional HEAD symref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListOutput {
    /// Concrete refs (e.g. `refs/heads/main` → SHA).
    pub refs: Vec<RefEntry>,
    /// If HEAD is a symref, the target (e.g. `refs/heads/main`).
    pub head_symref: Option<String>,
}

/// Parse a HEAD object body to extract the symref target.
///
/// Expected format: `ref: refs/heads/{name}\n`. Returns the full
/// target ref path (e.g. `refs/heads/main`).
///
/// # Errors
///
/// Returns [`CrabError::Protocol`] if the body doesn't match the
/// expected symref format.
pub fn parse_head_symref(body: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(body)
        .map_err(|_| CrabError::Protocol("HEAD object body is not valid UTF-8".into()))?;

    let trimmed = text.strip_suffix('\n').unwrap_or(text);

    let target = trimmed.strip_prefix("ref: ").ok_or_else(|| {
        CrabError::Protocol(format!(
            "HEAD object body does not start with 'ref: ': {trimmed:?}"
        ))
    })?;

    if !target.starts_with("refs/") {
        return Err(CrabError::Protocol(format!(
            "HEAD symref target is not under refs/: {target:?}"
        )));
    }

    Ok(target.to_owned())
}

/// Format a [`ListOutput`] into the git remote helper `list` response.
///
/// Each concrete ref is emitted as `{sha} {ref_name}\n`. Annotated tags
/// carrying a `peeled` target additionally emit `{peeled} {ref}^{{}}\n`
/// immediately after so clients can resolve the tag's target without a
/// second round trip. If a HEAD symref is present, `@{target} HEAD\n`
/// is emitted first. The response is terminated by a blank line.
pub fn format_list_output(output: &ListOutput) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    if let Some(target) = &output.head_symref {
        let _ = writeln!(buf, "@{target} HEAD");
    }
    for entry in &output.refs {
        let _ = writeln!(buf, "{} {}", entry.sha, entry.ref_name);
        if let Some(peeled) = &entry.peeled {
            let _ = writeln!(buf, "{} {}^{{}}", peeled, entry.ref_name);
        }
    }
    buf.push('\n');
    buf
}

/// Apply `list for-push` filtering to a [`ListOutput`].
///
/// When `for_push` is true, omits the HEAD symref and annotated-tag peeled
/// targets because neither is an addressable push destination. Concrete refs
/// are preserved in both cases.
pub fn filter_list_for_push(mut output: ListOutput, for_push: bool) -> ListOutput {
    if for_push {
        output.head_symref = None;
        for entry in &mut output.refs {
            entry.peeled = None;
        }
    }
    output
}

/// Format a [`PushResult`] into the git remote helper push response.
///
/// Each spec's destination ref is looked up in the result's outcomes map.
/// Refs are emitted in the order of the original `specs` slice (important
/// for git). Missing outcomes are treated as errors. The response is
/// terminated by a blank line.
#[allow(
    deprecated,
    reason = "pattern-matches the deprecated RefPushOutcome::Error variant for backward compat"
)]
pub fn format_push_response(result: &PushResult, specs: &[PushSpec]) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    for spec in specs {
        match result.outcomes.get(&spec.dst) {
            Some(RefPushOutcome::Ok) => {
                let _ = writeln!(buf, "ok {}", spec.dst);
            }
            Some(RefPushOutcome::Error(reason)) => {
                // Backward-compat path: opaque string reasons still emit
                // cleanly. New code should prefer `Rejected` so clients
                // can parse a stable tag.
                let _ = writeln!(buf, "error {} {}", spec.dst, one_line_protocol_text(reason));
            }
            Some(RefPushOutcome::Rejected(reason)) => {
                // Structured reason: emit the stable protocol tag first
                // so scripts can parse reliably, followed by human
                // detail on the same line. Git's receive-pack uses the
                // same convention (e.g. `error refs/heads/main
                // non-fast-forward`).
                let detail = one_line_protocol_text(&reason.to_string());
                let _ = writeln!(
                    buf,
                    "error {} {} ({})",
                    spec.dst,
                    reason.protocol_tag(),
                    detail
                );
            }
            None => {
                let _ = writeln!(buf, "error {} missing outcome", spec.dst);
            }
        }
    }
    buf.push('\n');
    buf
}

fn one_line_protocol_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut previous_was_separator = false;
    for character in value.chars() {
        if character.is_control() {
            if !previous_was_separator && !output.is_empty() {
                output.push(' ');
            }
            previous_was_separator = true;
        } else {
            output.push(character);
            previous_was_separator = character.is_whitespace();
        }
    }
    output.trim().to_owned()
}

/// Abstraction over stdin/stdout for testing.
///
/// Splits into a reader/writer pair so the protocol loop can read and
/// write concurrently without borrow conflicts.
pub trait StdIo {
    type Reader: tokio::io::AsyncBufRead + Unpin;
    type Writer: tokio::io::AsyncWrite + Unpin;

    fn split(self) -> (Self::Reader, Self::Writer);
}

/// A single fetch entry from a `fetch` command batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchEntry {
    pub sha: String,
    pub ref_name: String,
}

/// A single push refspec from a `push` command batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSpec {
    pub force: bool,
    pub src: String,
    pub dst: String,
}

/// Partial-clone filter values retained for API compatibility.
///
/// Crab does not currently produce promisor packs, so fetch rejects every
/// filter request instead of silently installing a complete repository. This
/// type is deprecated and scheduled for removal in the next major version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterSpec {
    /// `blob:none` — exclude all blob objects.
    BlobNone,
}

impl fmt::Display for FilterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlobNone => write!(f, "blob:none"),
        }
    }
}

/// Fetch constraints passed to the pack download pipeline.
///
/// The remote helper populates `depth`. The public `filter` field is retained
/// for compatibility, but any populated value is rejected by the fetch
/// pipeline because Crab does not implement partial-clone promisor semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchOptions {
    /// Shallow clone depth (`--depth N`). `None` means full clone.
    pub depth: Option<u32>,
    /// Whether `depth` extends the repository's current shallow boundary.
    pub deepen_relative: bool,
    /// Deprecated compatibility field; every populated value is rejected
    /// before filesystem or object-store I/O and will be removed next major.
    pub filter: Option<FilterSpec>,
}

impl FetchOptions {
    /// Whether any shallow or filter constraint is active.
    pub fn has_constraints(&self) -> bool {
        self.depth.is_some() || self.deepen_relative || self.filter.is_some()
    }
}

/// Options set by Git via the `option` command.
#[derive(Debug, Clone)]
pub struct HelperOptions {
    pub progress: bool,
    pub verbosity: u32,
    /// Git asked the helper to prove that a clone is self-contained and
    /// connected. Crab acknowledges only after an unconstrained fetch installs
    /// the complete, producer-validated manifest inventory.
    pub check_connectivity: bool,
    /// Fetch constraints accumulated from supported option commands.
    pub fetch_options: FetchOptions,
    /// When `true`, the whole batch either commits or rolls back — no
    /// partial writes when any ref is rejected. Set by git via
    /// `option atomic true` during smart-HTTP receive-pack.
    pub atomic: bool,
    /// When `true`, Git asked fetch to include annotated tag objects whose
    /// targets are transferred. Crab fetches complete packs, so accepting this
    /// hint requires no separate object-transfer path.
    pub followtags: bool,
    /// Deprecated field retained for compatibility with the released public
    /// type and scheduled for removal in the next major version.
    ///
    /// Git's remote-helper protocol has no `include-tag` option. Crab leaves
    /// this false and reports that protocol option as unsupported.
    pub include_tag: bool,
}

impl Default for HelperOptions {
    fn default() -> Self {
        Self {
            progress: true,
            verbosity: 1,
            check_connectivity: false,
            fetch_options: FetchOptions::default(),
            atomic: false,
            followtags: false,
            include_tag: false,
        }
    }
}

/// Parsed helper command. Commands are batched until a blank line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HelperCommand {
    Capabilities,
    List {
        for_push: bool,
    },
    Fetch(FetchEntry),
    Push(PushSpec),
    /// A `push` line whose ref name failed `gix_validate::reference::
    /// name_partial`. The batch collector appends this as a pre-rejected
    /// marker so one bad refname rejects only that ref, not the whole
    /// batch. Genuine protocol syntax errors (missing `:`, unknown
    /// command) still abort via [`CrabError::Protocol`] — those
    /// signal a broken client, not a bad ref.
    PushRejected {
        dst: String,
        reason: PushRejectReason,
    },
    Option {
        key: String,
        value: String,
    },
}

impl fmt::Display for HelperCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capabilities => write!(f, "capabilities"),
            Self::List { for_push } => {
                if *for_push {
                    write!(f, "list for-push")
                } else {
                    write!(f, "list")
                }
            }
            Self::Fetch(entry) => write!(f, "fetch {} {}", entry.sha, entry.ref_name),
            Self::Push(spec) => {
                let prefix = if spec.force { "+" } else { "" };
                write!(f, "push {prefix}{}:{}", spec.src, spec.dst)
            }
            Self::PushRejected { dst, reason } => {
                write!(f, "push-rejected {dst} ({reason})")
            }
            Self::Option { key, value } => write!(f, "option {key} {value}"),
        }
    }
}

/// One entry of a push batch: either a real [`PushSpec`] forwarded to
/// the pipeline, or a ref rejected at parse time (bad refname) that
/// short-circuits straight to the response map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PushItem {
    Spec(PushSpec),
    Rejected {
        dst: String,
        reason: PushRejectReason,
    },
}

/// Dispatched batch of commands after collecting until a blank line.
#[derive(Debug)]
enum Batch {
    Capabilities,
    List { for_push: bool },
    Fetch(Vec<FetchEntry>),
    Push(Vec<PushItem>),
}

#[derive(Debug, Clone, Copy)]
enum BatchKind {
    Fetch,
    Push,
}

/// Parse a single line into a `HelperCommand`.
fn parse_command(line: &str) -> Result<HelperCommand> {
    let line = line.trim_end_matches('\n');

    if line == "capabilities" {
        return Ok(HelperCommand::Capabilities);
    }
    if line == "list" {
        return Ok(HelperCommand::List { for_push: false });
    }
    if line == "list for-push" {
        return Ok(HelperCommand::List { for_push: true });
    }
    if let Some(rest) = line.strip_prefix("fetch ") {
        let (sha, ref_name) = rest
            .split_once(' ')
            .ok_or_else(|| CrabError::Protocol(format!("malformed fetch: {line}")))?;
        return Ok(HelperCommand::Fetch(FetchEntry {
            sha: sha.to_owned(),
            ref_name: ref_name.to_owned(),
        }));
    }
    if let Some(rest) = line.strip_prefix("push ") {
        let (force, refspec) = if let Some(stripped) = rest.strip_prefix('+') {
            (true, stripped)
        } else {
            (false, rest)
        };
        let (src, dst) = refspec
            .split_once(':')
            .ok_or_else(|| CrabError::Protocol(format!("malformed push refspec: {line}")))?;

        // An empty `dst` cannot be turned into a per-ref rejection —
        // there is no address to report `error {dst} bad-refname`
        // against — so it stays a genuine protocol error that aborts
        // the batch, matching the remote-helper spec.
        if dst.is_empty() {
            return Err(CrabError::Protocol(format!(
                "empty dst in push refspec: {line}"
            )));
        }

        // `dst` is the addressable ref on the remote; a bad-refname
        // rejection is keyed on it. A non-empty but malformed `src`
        // also rejects against `dst` (the target is the reportable
        // identifier). An empty `src` is a legal `git push :ref`
        // delete and must not be validated as a refname.
        if crate::git::refname::validate_push_refname(dst).is_err() {
            tracing::debug!(name = %dst, "rejected bad refname");
            return Ok(HelperCommand::PushRejected {
                dst: dst.to_owned(),
                reason: PushRejectReason::BadRefname(dst.to_owned()),
            });
        }
        if !src.is_empty() && crate::git::refname::validate_push_refname(src).is_err() {
            tracing::debug!(name = %src, "rejected bad refname");
            return Ok(HelperCommand::PushRejected {
                dst: dst.to_owned(),
                reason: PushRejectReason::BadRefname(src.to_owned()),
            });
        }

        return Ok(HelperCommand::Push(PushSpec {
            force,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }));
    }
    if let Some(rest) = line.strip_prefix("option ") {
        let (key, value) = rest
            .split_once(' ')
            .ok_or_else(|| CrabError::Protocol(format!("malformed option: {line}")))?;
        return Ok(HelperCommand::Option {
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }

    Err(CrabError::Protocol(format!("unknown command: {line}")))
}

fn finalize_batch(
    kind: Option<BatchKind>,
    fetch_entries: Vec<FetchEntry>,
    push_items: Vec<PushItem>,
) -> Result<Batch> {
    match kind {
        Some(BatchKind::Fetch) => Ok(Batch::Fetch(fetch_entries)),
        Some(BatchKind::Push) => Ok(Batch::Push(push_items)),
        None => Err(CrabError::Protocol("empty batch".into())),
    }
}

/// Per-session cache that avoids redundant config resolution, pack-list
/// fetches, and commit-graph probes within a single `run_remote_helper`
/// invocation.
struct SessionCache {
    /// Config resolved before any remote operation starts.
    config: crate::core::config::Config,
    /// PackList from the most recent fetch, reused by `check_repack_threshold`.
    pack_list: Option<crab_metadata::manifests::PackList>,
    /// Cached result of the `has_commit_graph_summary` probe.
    has_commit_graph: Option<bool>,
}

impl SessionCache {
    fn new(config: crate::core::config::Config) -> Self {
        Self {
            config,
            pack_list: None,
            has_commit_graph: None,
        }
    }

    fn config(&self) -> &crate::core::config::Config {
        &self.config
    }

    /// Clears the cached commit-graph flag so the next access re-probes
    /// the store. Called after a push that creates or updates the summary.
    fn invalidate_commit_graph(&mut self) {
        self.has_commit_graph = None;
    }
}

fn resolve_remote_helper_config() -> Result<crate::core::config::Config> {
    let mut config = crate::core::config::Config::resolve_local()?;
    if let Ok(cwd) = std::env::current_dir()
        && let Some(project) = crate::core::project_config::ProjectConfig::discover(&cwd)
    {
        if config.remote_url.is_none() {
            config.remote_url = Some(project.remote.url.clone());
        }
        if let Some(replication) = project.replication {
            config.replication = Some(replication);
        }
    }
    apply_internal_remote_helper_overrides(&mut config);
    Ok(config)
}

fn apply_internal_remote_helper_overrides(config: &mut crate::core::config::Config) {
    apply_internal_remote_helper_overrides_for_agent_rebase(
        config,
        internal_bool_env(AGENT_REBASE_FETCH_REF_FILTERING_ENV),
    );
}

fn apply_internal_remote_helper_overrides_for_agent_rebase(
    config: &mut crate::core::config::Config,
    enabled: bool,
) {
    if enabled {
        config.fetch_ref_filtering = true;
    }
}

fn internal_bool_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .as_deref()
        .is_some_and(internal_bool_value)
}

fn internal_bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Run the git remote helper protocol loop.
///
/// Reads line-framed commands from stdin, batches them until a blank line,
/// dispatches each batch, and writes responses to stdout. Returns when
/// stdin reaches EOF.
///
/// The caller owns the shutdown token so process-level SIGINT/SIGTERM
/// cancellation reaches fetch and push operations instead of only
/// terminating after the current batch returns.
///
/// # Errors
///
/// Returns [`CrabError::Protocol`] on malformed input or
/// [`CrabError::Io`] on I/O failures.
pub async fn run_remote_helper(
    remote_name: &str,
    url: &gix_url::Url,
    io: impl StdIo,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Serialize the helper-invocation URL once so fetch_packs can write
    // it into `.crab/remote` without re-deriving from the prefix or
    // shelling out to `git remote get-url`. `gix_url::Url::to_bstring`
    // produces the same canonical form git itself used to spawn us.
    let invocation_url: String = url.to_bstring().to_string();

    // Check CRAB_PROGRESS_FORMAT env var for structured output mode.
    // When set to "jsonl", progress events are emitted as JSONL to stderr.
    // Git owns stdout in the remote helper context, so JSONL MUST go to
    // stderr. The Electron UI sets this env var when spawning `git push`
    // and reads JSONL from the child's stderr.
    let progress_mode = match std::env::var("CRAB_PROGRESS_FORMAT").as_deref() {
        Ok("jsonl") => OutputMode::Jsonl,
        _ => OutputMode::Text,
    };

    // Build the shared JSONL stderr stream when in JSONL mode.
    // JsonlStream<Stderr> writes structured events to stderr because git
    // owns stdout — this is the only context where JSONL goes to stderr
    // instead of stdout.
    let jsonl_stderr_stream: Option<Arc<Mutex<JsonlStream<Stderr>>>> =
        if progress_mode == OutputMode::Jsonl {
            Some(Arc::new(Mutex::new(JsonlStream::new(
                "push.event",
                "1.0",
                std::io::stderr(),
            ))))
        } else {
            None
        };

    // Resolve config early — needed for store construction and CachingStore.
    let config = resolve_remote_helper_config()?;

    // Classify before constructing any store. A corrupt configured profile is
    // an error, never a reason to reinterpret a logical authority as a bucket.
    let cache_dir = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let resolver = crab_auth_store::ManagedRepositoryResolver::new(cache_dir);
    let locator = resolver.classify(&url.to_bstring().to_string())?;
    let managed_repository = match &locator {
        crab_git::RepositoryLocator::Managed(repository) => Some(repository.clone()),
        crab_git::RepositoryLocator::Direct(_) => None,
    };

    let resolved = crate::auth::build_repository_store(
        &config,
        locator,
        crab_auth::TransferOperation::Fetch,
        &cancel,
    )
    .await?;

    // Wrap with CachingStore when a cache service is configured.
    // The caching store routes immutable reads (packs, shards) through
    // the local disk cache and optional remote cache service.
    let caching_store =
        match crab_cache_store::CachingStore::new(resolved.store.clone(), &config.cache) {
            Ok(cache) => Some(cache),
            Err(error) => {
                tracing::warn!(error = %error, "failed to build CachingStore, using origin only");
                None
            }
        };

    // Open the staging area for the push pipeline.
    let staging = open_staging_for_push().await;

    // Load push state for incremental walk (used by native push pipeline).
    let repo_root = push_state_repo_root();
    let context = RemoteHelperContext {
        store: resolved.store,
        staging,
        prefix: resolved.repository_prefix,
        cache: SessionCache::new(config),
        push_state: PushState::load(&repo_root),
        push_state_repo_root: repo_root,
        progress_mode,
        jsonl_stderr_stream,
        invocation_url,
        managed_repository,
        caching_store,
    };

    run_remote_helper_with_context(remote_name, io, context, cancel).await
}

struct RemoteHelperContext {
    store: crate::storage::store::Store,
    staging: Option<std::sync::Arc<crab_staging::StagingAreaReadOnly>>,
    prefix: String,
    cache: SessionCache,
    push_state: PushState,
    push_state_repo_root: std::path::PathBuf,
    progress_mode: OutputMode,
    jsonl_stderr_stream: Option<Arc<Mutex<JsonlStream<Stderr>>>>,
    invocation_url: String,
    managed_repository: Option<crab_git::ManagedRepository>,
    caching_store: Option<crab_cache_store::CachingStore>,
}

async fn run_remote_helper_with_context(
    remote_name: &str,
    io: impl StdIo,
    context: RemoteHelperContext,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let (mut reader, mut writer) = io.split();
    let mut options = HelperOptions::default();
    let mut line_buf = String::new();
    let RemoteHelperContext {
        store,
        staging,
        prefix,
        mut cache,
        mut push_state,
        push_state_repo_root,
        progress_mode,
        jsonl_stderr_stream,
        invocation_url,
        managed_repository,
        caching_store,
    } = context;

    loop {
        let Some(batch) = read_batch(&mut reader, &mut line_buf, &mut options, &mut writer).await?
        else {
            // Save push state on clean exit.
            if let Err(e) = push_state.save(&push_state_repo_root) {
                tracing::warn!(error = %e, "failed to save push state");
            }
            return Ok(());
        };
        dispatch_batch(
            &batch,
            &options,
            &mut writer,
            Some(&store),
            staging.as_ref(),
            &prefix,
            &mut cache,
            remote_name,
            &mut push_state,
            progress_mode,
            jsonl_stderr_stream.as_ref(),
            Some(invocation_url.as_str()),
            managed_repository.as_ref(),
            caching_store.as_ref(),
            &cancel,
        )
        .await?;
    }
}

/// Open the staging area for the push pipeline (read-only).
///
/// Uses a shared lock so the push can read staged chunks without
/// blocking concurrent filter-process writers.
///
/// A failure to acquire the read lock (e.g. because another process
/// holds an exclusive write lock) collapses to `None`. The push
/// pipeline's step 2 (`lookup_staging`) is responsible for refusing
/// the push when `None` is combined with a non-empty pointer set
/// discovered in step 1 — otherwise we'd silently advance the ref to
/// commits whose pointer blobs reference never-uploaded xorbs.
async fn open_staging_for_push() -> Option<std::sync::Arc<crab_staging::StagingAreaReadOnly>> {
    let staging_root = push_staging_root()?;

    if !staging_root.exists() {
        return None;
    }

    match crab_staging::StagingAreaReadOnly::open_blocking_default(staging_root).await {
        Ok(sa) => Some(std::sync::Arc::new(sa)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "staging area unavailable for push; pipeline will refuse to \
                 upload any new pointer blobs discovered in this push. \
                 Resolve the lock holder and retry."
            );
            None
        }
    }
}

fn push_staging_root() -> Option<std::path::PathBuf> {
    crate::git::worktree::WorktreeContext::resolve()
        .ok()
        .map(|ctx| ctx.shared_staging_dir())
        .or_else(|| super::discover::resolve_crab_dir().map(|dir| dir.join("staging")))
        .or_else(|| {
            let git_dir = super::discover::discover_git_dir().ok()?;
            shared_staging_dir_from_git_dir(&git_dir)
        })
}

fn shared_staging_dir_from_git_dir(git_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let common_dir = super::discover::resolve_common_dir(git_dir);
    let repo_root = common_dir.parent()?;
    Some(repo_root.join(".crab").join("staging"))
}

fn push_state_repo_root() -> std::path::PathBuf {
    crate::git::worktree::WorktreeContext::resolve()
        .ok()
        .map(|ctx| ctx.main_worktree_root)
        .or_else(super::discover::resolve_main_worktree_root)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// Read commands until a blank line, returning the collected batch.
///
/// `option` commands are handled inline (they produce immediate responses
/// and are not batched). Returns `None` on EOF before any command is read.
async fn read_batch<R, W>(
    reader: &mut R,
    line_buf: &mut String,
    options: &mut HelperOptions,
    writer: &mut W,
) -> Result<Option<Batch>>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut fetch_entries: Vec<FetchEntry> = Vec::new();
    let mut push_items: Vec<PushItem> = Vec::new();
    let mut batch_kind: Option<BatchKind> = None;

    loop {
        line_buf.clear();
        let n = reader.read_line(line_buf).await?;
        if n == 0 {
            return if batch_kind.is_some() {
                Ok(Some(finalize_batch(batch_kind, fetch_entries, push_items)?))
            } else {
                Ok(None)
            };
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            return if let Some(kind) = batch_kind {
                Ok(Some(finalize_batch(Some(kind), fetch_entries, push_items)?))
            } else {
                continue;
            };
        }

        let cmd = parse_command(trimmed)?;

        match cmd {
            HelperCommand::Option { key, value } => {
                handle_option(&key, &value, options, writer).await?;
            }
            HelperCommand::Capabilities => {
                return Ok(Some(Batch::Capabilities));
            }
            HelperCommand::List { for_push } => {
                return Ok(Some(Batch::List { for_push }));
            }
            HelperCommand::Fetch(entry) => {
                if batch_kind.is_none() {
                    batch_kind = Some(BatchKind::Fetch);
                }
                if !matches!(batch_kind, Some(BatchKind::Fetch)) {
                    return Err(CrabError::Protocol("mixed command types in batch".into()));
                }
                fetch_entries.push(entry);
            }
            HelperCommand::Push(spec) => {
                if batch_kind.is_none() {
                    batch_kind = Some(BatchKind::Push);
                }
                if !matches!(batch_kind, Some(BatchKind::Push)) {
                    return Err(CrabError::Protocol("mixed command types in batch".into()));
                }
                push_items.push(PushItem::Spec(spec));
            }
            HelperCommand::PushRejected { dst, reason } => {
                if batch_kind.is_none() {
                    batch_kind = Some(BatchKind::Push);
                }
                if !matches!(batch_kind, Some(BatchKind::Push)) {
                    return Err(CrabError::Protocol("mixed command types in batch".into()));
                }
                push_items.push(PushItem::Rejected { dst, reason });
            }
        }
    }
}

/// Handle an `option` command: acknowledge known keys, reject unknown.
async fn handle_option<W: tokio::io::AsyncWrite + Unpin>(
    key: &str,
    value: &str,
    options: &mut HelperOptions,
    writer: &mut W,
) -> Result<()> {
    match key {
        "progress" => {
            options.progress = value == "true";
            writer.write_all(b"ok\n").await?;
        }
        "verbosity" => {
            options.verbosity = value
                .parse()
                .map_err(|_| CrabError::Protocol(format!("invalid verbosity: {value}")))?;
            writer.write_all(b"ok\n").await?;
        }
        "depth" => {
            let depth: u32 = value
                .parse()
                .map_err(|_| CrabError::Protocol(format!("invalid depth: {value}")))?;
            options.fetch_options.depth = Some(depth);
            tracing::debug!(depth, "shallow depth set");
            writer.write_all(b"ok\n").await?;
        }
        "deepen-relative" => match value {
            "true" => {
                options.fetch_options.deepen_relative = true;
                writer.write_all(b"ok\n").await?;
            }
            "false" => {
                options.fetch_options.deepen_relative = false;
                writer.write_all(b"ok\n").await?;
            }
            other => {
                writer
                    .write_all(format!("error invalid deepen-relative value: {other}\n").as_bytes())
                    .await?;
            }
        },
        // The published summary has generation numbers but no commit
        // timestamps or excluded-ref ancestry. Reject these selectors so Git
        // cannot silently turn a requested shallow clone into a full clone.
        "deepen-since" => {
            let reason = "deepen-since is not supported; use --depth";
            writer
                .write_all(format!("error {reason}\n").as_bytes())
                .await?;
            writer.flush().await?;
            // Git treats an option-level `error` as advisory and otherwise
            // continues with an unconstrained fetch. End the helper session
            // as well so the requested history bound cannot be discarded.
            return Err(CrabError::Protocol(reason.to_owned()));
        }
        "deepen-not" => {
            let reason = "deepen-not is not supported; use --depth";
            writer
                .write_all(format!("error {reason}\n").as_bytes())
                .await?;
            writer.flush().await?;
            return Err(CrabError::Protocol(reason.to_owned()));
        }
        // Crab remotes are never shallow themselves. The option therefore
        // cannot expose additional upstream history, and either valid value
        // is already honored by the normal transactional shallow update.
        "update-shallow" => match value {
            "true" | "false" => writer.write_all(b"ok\n").await?,
            other => {
                writer
                    .write_all(format!("error invalid update-shallow value: {other}\n").as_bytes())
                    .await?;
            }
        },
        "filter" => {
            tracing::debug!(filter = %value, "partial clone filters are not supported");
            writer.write_all(b"unsupported\n").await?;
        }
        "check-connectivity" => match value {
            "true" => {
                options.check_connectivity = true;
                writer.write_all(b"ok\n").await?;
            }
            "false" => {
                options.check_connectivity = false;
                writer.write_all(b"ok\n").await?;
            }
            other => {
                let msg = format!("invalid check-connectivity value: {other}");
                writer
                    .write_all(format!("error {msg}\n").as_bytes())
                    .await?;
            }
        },
        "atomic" => match value {
            "true" => {
                options.atomic = true;
                tracing::debug!(atomic = true, "atomic push mode enabled");
                writer.write_all(b"ok\n").await?;
            }
            "false" => {
                options.atomic = false;
                tracing::debug!(atomic = false, "atomic push mode disabled");
                writer.write_all(b"ok\n").await?;
            }
            // Invalid bool — git's send-pack only ever sends
            // `true`/`false`, so anything else is a protocol violation
            // rather than "merely unknown". Report `error` per the
            // remote-helper spec so the client surfaces it rather
            // than silently assuming the default.
            other => {
                let msg = format!("invalid atomic value: {other}");
                tracing::warn!(value = other, "invalid atomic option value");
                writer
                    .write_all(format!("error {msg}\n").as_bytes())
                    .await?;
            }
        },
        "followtags" => match value {
            "true" => {
                options.followtags = true;
                tracing::debug!(followtags = true, "follow-tags mode enabled");
                writer.write_all(b"ok\n").await?;
            }
            "false" => {
                options.followtags = false;
                tracing::debug!(followtags = false, "follow-tags mode disabled");
                writer.write_all(b"ok\n").await?;
            }
            // Same reasoning as `atomic`: git only ever sends the
            // canonical boolean tokens, so anything else is a client
            // bug the pusher deserves to see.
            other => {
                let msg = format!("invalid followtags value: {other}");
                tracing::warn!(value = other, "invalid followtags option value");
                writer
                    .write_all(format!("error {msg}\n").as_bytes())
                    .await?;
            }
        },
        _ => {
            writer.write_all(b"unsupported\n").await?;
        }
    }
    writer.flush().await?;
    Ok(())
}

/// Dispatch a collected batch and write the response.
#[expect(
    clippy::too_many_arguments,
    reason = "remote-helper batch dispatch carries protocol state, stores, cache, progress, and cancellation handles"
)]
async fn dispatch_batch<W: tokio::io::AsyncWrite + Unpin>(
    batch: &Batch,
    options: &HelperOptions,
    writer: &mut W,
    store: Option<&crate::storage::store::Store>,
    staging: Option<&std::sync::Arc<crab_staging::StagingAreaReadOnly>>,
    prefix: &str,
    cache: &mut SessionCache,
    remote_name: &str,
    push_state: &mut PushState,
    progress_mode: OutputMode,
    jsonl_stderr_stream: Option<&Arc<Mutex<JsonlStream<Stderr>>>>,
    remote_url: Option<&str>,
    managed_repository: Option<&crab_git::ManagedRepository>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    match batch {
        Batch::Capabilities => {
            tracing::debug!("responding to capabilities");
            let router = store.map(|s| StoreLayout::new(s.clone(), prefix.to_owned()));
            let has_graph = has_commit_graph_summary(store, prefix, router.as_ref(), cache).await;
            let caps = format_capabilities(has_graph);
            writer.write_all(caps.as_bytes()).await?;
            writer.flush().await?;
        }
        Batch::List { for_push } => {
            tracing::debug!(for_push, "list requested");
            let output = if let Some(s) = store {
                let cfg = cache.config();
                let (read_store, router) =
                    read_store_for_list_batch(s, prefix, remote_url, cfg, *for_push, cancel).await;
                read_remote_refs_for_advertisement(&read_store, &router, &cfg.transfer_hide_refs)
                    .await?
            } else {
                ListOutput {
                    refs: Vec::new(),
                    head_symref: None,
                }
            };
            let output = filter_list_for_push(output, *for_push);
            let response = format_list_output(&output);
            writer.write_all(response.as_bytes()).await?;
            writer.flush().await?;
        }
        Batch::Fetch(entries) => {
            let span = tracing::info_span!("fetch", refs = entries.len());
            async {
                dispatch_fetch_batch_with_selector(
                    entries,
                    options,
                    writer,
                    store,
                    prefix,
                    cache,
                    remote_url,
                    caching_store,
                    cancel,
                    |config, parsed, cancel| async move {
                        crate::replication::select_read_store(&config, &parsed, "fetch", &cancel)
                            .await
                    },
                )
                .await
            }
            .instrument(span)
            .await?;
        }
        Batch::Push(items) => {
            let span = tracing::info_span!("push", refs = items.len());
            async {
                tracing::debug!("push batch");

                // Split the ordered push batch into real specs (which
                // flow into the pipeline) and parse-time rejections
                // (which short-circuit to the response map). The
                // original `items` ordering drives response emission
                // so clients see `ok`/`error` lines in the same order
                // they pushed.
                let mut specs: Vec<PushSpec> = Vec::with_capacity(items.len());
                let mut pre_rejected: Vec<(String, PushRejectReason)> = Vec::new();
                // Synthetic spec sequence preserving per-line order so
                // `format_push_response` emits one line per original
                // `push` input. Pre-rejected entries use an empty
                // `src` since they never reach the pipeline.
                let mut ordered_specs: Vec<PushSpec> = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        PushItem::Spec(spec) => {
                            specs.push(spec.clone());
                            ordered_specs.push(spec.clone());
                        }
                        PushItem::Rejected { dst, reason } => {
                            pre_rejected.push((dst.clone(), reason.clone()));
                            ordered_specs.push(PushSpec {
                                force: false,
                                src: String::new(),
                                dst: dst.clone(),
                            });
                        }
                    }
                }

                let config = cache.config().clone();
                let mut base_config = PushConfig::from_config(&config);
                let mut push_setup_error = if specs.is_empty() {
                    None
                } else {
                    configure_active_active_push_coordinator(
                        &config,
                        remote_url,
                        prefix,
                        &mut base_config,
                    )
                    .await
                    .err()
                };
                let mut native_config = native_push_config_for_helper(
                    base_config,
                    options,
                    progress_mode,
                    jsonl_stderr_stream,
                );
                let mut push_store = store.cloned();
                if !specs.is_empty() && push_setup_error.is_none() {
                    if let Some(repository) = managed_repository {
                        match store {
                            Some(read_store) => {
                                match crate::git::protected_push::prepare_managed_push(
                                    &config, repository, read_store, prefix, &specs, staging,
                                    cancel,
                                )
                                .await
                                {
                                    Ok(protected) => {
                                        native_config.push.atomic = true;
                                        native_config.push.protected_push = Some(protected.session);
                                        push_store = Some(protected.store);
                                    }
                                    Err(error) => push_setup_error = Some(error),
                                }
                            }
                            None => {
                                push_setup_error = Some(CrabError::Configuration {
                                    key: "managed push store".to_owned(),
                                    origin:
                                        "managed protected push requires its resolved read store"
                                            .to_owned(),
                                });
                            }
                        }
                    } else if matches!(
                        config.auth.provider,
                        crate::core::config::AuthProvider::CrabAuth
                    ) {
                        match remote_url
                            .ok_or_else(|| CrabError::Protocol("missing remote URL".into()))
                            .and_then(crate::git::url::CrabUrl::parse)
                        {
                            Ok(parsed) => match crate::git::protected_push::prepare_crab_auth_push(
                                &config, &parsed, &specs, cancel,
                            )
                            .await
                            {
                                Ok(protected) => {
                                    native_config.push.atomic = true;
                                    native_config.push.protected_push = Some(protected.session);
                                    push_store = Some(protected.store);
                                }
                                Err(e) => push_setup_error = Some(e),
                            },
                            Err(e) => push_setup_error = Some(e),
                        }
                    }
                }

                // Build CachingStore when a cache service is configured and healthy.
                let caching_store = if let Some(s) = push_store.as_ref() {
                    crab_cache_store::CachingStore::try_build_healthy(
                        s.as_storage().clone(),
                        &config.cache,
                    )
                    .await
                } else {
                    None
                };

                let router = if let Some(s) = push_store.as_ref() {
                    StoreLayout::new(s.clone(), prefix.to_owned())
                } else {
                    StoreLayout::new(
                        crate::storage::store::Store::new(std::sync::Arc::new(
                            object_store::memory::InMemory::new(),
                        )),
                        prefix.to_owned(),
                    )
                };

                // Parse-time rejections participate in the atomic contract;
                // valid siblings must not reach a pipeline that could commit.
                let atomic_parse_blocker = if native_config.push.atomic {
                    pre_rejected.first().map(|(dst, _)| dst.clone())
                } else {
                    None
                };
                let mut result = if let Some(blocked_by) = atomic_parse_blocker {
                    let outcomes = specs
                        .iter()
                        .map(|spec| {
                            (
                                spec.dst.clone(),
                                RefPushOutcome::Rejected(PushRejectReason::AtomicAbort {
                                    blocked_by: blocked_by.clone(),
                                }),
                            )
                        })
                        .collect();
                    PushResult::new(outcomes)
                } else if specs.is_empty() {
                    PushResult::empty()
                } else if let Some(e) = push_setup_error {
                    tracing::error!(error = %e, "push setup failed");
                    reject_specs_for_error(&specs, &e)
                } else {
                    let push_state_remote_url = remote_url.ok_or_else(|| {
                        CrabError::Protocol(
                            "push batch is missing its invocation remote URL".to_owned(),
                        )
                    })?;
                    match run_native_push(
                        &native_config,
                        &specs,
                        NativePushInputs::new(
                            push_store,
                            caching_store,
                            staging.cloned(),
                            router,
                            push_state,
                            remote_name,
                            push_state_remote_url,
                            None,
                            cancel.clone(),
                        ),
                    )
                    .await
                    {
                        Ok(r) => r,
                        // Partial outcomes carry per-ref state the pipeline
                        // already computed — unwrap so siblings keep the
                        // outcomes they earned instead of collapsing to the
                        // aggregate error string below.
                        Err(CrabError::PushPartialOutcome { outcomes, .. }) => *outcomes,
                        Err(e) => {
                            // Structured per-ref rejection for truly batch-
                            // global failures (protocol, cancellation,
                            // store open). Using the structured `Internal`
                            // variant keeps the `error {ref} internal`
                            // response shape and preserves the stringified
                            // chain for logs while dropping the old
                            // `Error(String)` shape.
                            tracing::error!(error = %e, "push pipeline failed");
                            reject_specs_for_error(&specs, &e)
                        }
                    }
                };

                // Merge parse-time rejections into the outcome map.
                // Pipeline outcomes never key on a pre-rejected `dst`
                // (those specs were filtered out before
                // `run_native_push`), so there is no collision to
                // resolve — a simple insert preserves both sides.
                for (dst, reason) in pre_rejected {
                    result
                        .outcomes
                        .insert(dst, RefPushOutcome::Rejected(reason));
                }
                if let Err(err) = record_push_audit_event(
                    &default_log_path(),
                    remote_url,
                    prefix,
                    &ordered_specs,
                    &result,
                    None,
                ) {
                    warn!(%err, "failed to append push audit event");
                }

                // A successful push may have updated the CommitGraphSummary,
                // so invalidate the cached probe result.
                let any_ref_succeeded = result
                    .outcomes
                    .values()
                    .any(|o| matches!(o, RefPushOutcome::Ok));
                if any_ref_succeeded {
                    cache.invalidate_commit_graph();
                }

                let response = format_push_response(&result, &ordered_specs);
                writer.write_all(response.as_bytes()).await?;
                writer.flush().await?;
                if progress_mode == OutputMode::Jsonl
                    && let Some(stream) = jsonl_stderr_stream
                    && let Ok(mut s) = stream.lock()
                {
                    s.emit_result(build_push_result_event(&result, &ordered_specs));
                }
                Ok::<(), CrabError>(())
            }
            .instrument(span)
            .await?;
        }
    }
    Ok(())
}

fn reject_specs_for_error(specs: &[PushSpec], err: &CrabError) -> PushResult {
    let reason = PushRejectReason::from_error(err);
    let mut outcomes = std::collections::HashMap::new();
    for spec in specs {
        outcomes.insert(spec.dst.clone(), RefPushOutcome::Rejected(reason.clone()));
    }
    PushResult::new(outcomes)
}

fn native_push_config_for_helper(
    base_config: PushConfig,
    options: &HelperOptions,
    progress_mode: OutputMode,
    jsonl_stderr_stream: Option<&Arc<Mutex<JsonlStream<Stderr>>>>,
) -> NativePushConfig {
    let mut native_config = NativePushConfig::new(base_config);
    native_config.progress = options.progress;
    native_config.color = options.progress && crate::git::progress::is_tty();
    native_config.emit_summary = false;
    native_config.push.atomic = options.atomic;

    // Git owns stdout for remote helpers. Human-readable phase progress
    // can go to stderr, but the final stdout summary would be parsed as
    // helper protocol instead of user text.
    if progress_mode == OutputMode::Jsonl {
        native_config.output_mode = Some(progress_mode);
        native_config.jsonl_stderr_stream = jsonl_stderr_stream.map(Arc::clone);
    }
    native_config.mirror_git_only =
        std::env::var_os(crate::git::push_native::MIRROR_GIT_ONLY_ENV).is_some();

    native_config
}

#[allow(
    deprecated,
    reason = "remote-helper summaries still report deprecated Error outcomes for compatibility"
)]
fn build_push_result_event(
    result: &PushResult,
    ordered_specs: &[PushSpec],
) -> PushResultEventPayload {
    let refs = ordered_specs
        .iter()
        .map(|spec| {
            let outcome = result.outcomes.get(&spec.dst);
            let (status, error) = match outcome {
                Some(RefPushOutcome::Ok) => ("ok".to_owned(), None),
                Some(RefPushOutcome::Error(reason)) => {
                    ("internal".to_owned(), Some(reason.clone()))
                }
                Some(RefPushOutcome::Rejected(reason)) => {
                    (reason.protocol_tag().to_owned(), Some(reason.to_string()))
                }
                None => ("internal".to_owned(), Some("missing outcome".to_owned())),
            };
            PushResultRefPayload {
                dst: spec.dst.clone(),
                status,
                error,
            }
        })
        .collect();

    PushResultEventPayload {
        refs,
        operation_id: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.operation_id.clone()),
        coordinator_epoch: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.coordinator_epoch),
        writer_region: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.writer_region.clone()),
        commit_state: result
            .active_active_commit
            .as_ref()
            .map(|commit| commit.commit_state_name().to_owned()),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "testable fetch dispatch boundary includes selector injection plus protocol state"
)]
async fn dispatch_fetch_batch_with_selector<W, F, Fut>(
    entries: &[FetchEntry],
    options: &HelperOptions,
    writer: &mut W,
    store: Option<&crate::storage::store::Store>,
    prefix: &str,
    cache: &mut SessionCache,
    remote_url: Option<&str>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    cancel: &tokio_util::sync::CancellationToken,
    select_read: F,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    F: FnOnce(
        crate::core::config::Config,
        crate::git::url::CrabUrl,
        tokio_util::sync::CancellationToken,
    ) -> Fut,
    Fut: Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    tracing::debug!(entries = entries.len(), "fetch batch");
    let mut connectivity_lock = None;
    if let Some(s) = store {
        let cfg = cache.config().clone();
        let (read_store, router) =
            read_store_for_batch_with_selector(s, prefix, remote_url, &cfg, cancel, select_read)
                .await;
        let read_caching_store =
            crab_cache_store::CachingStore::new(read_store.clone(), &cfg.cache).ok();
        connectivity_lock = fetch_packs(
            &read_store,
            &router,
            entries,
            &options.fetch_options,
            remote_url,
            &cfg,
            writer,
            read_caching_store.as_ref().or(caching_store),
            cache,
            cancel,
            options.check_connectivity,
        )
        .await?;
        let primary_router = StoreLayout::new(s.clone(), prefix.to_owned());
        check_repack_threshold(s, &primary_router, cache).await;
    } else {
        tracing::warn!("no store available for fetch");
    }
    let response = async {
        if let Some(keep_path) = &connectivity_lock {
            let line = format!("lock {}\nconnectivity-ok\n", keep_path.display());
            writer.write_all(line.as_bytes()).await?;
        }
        writer.write_all(b"\n").await?;
        writer.flush().await
    }
    .await;
    if response.is_err()
        && let Some(keep_path) = &connectivity_lock
    {
        let _ = std::fs::remove_file(keep_path);
    }
    response?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListReadAuthority {
    Primary,
    ReplicaEligible,
}

fn list_read_authority(for_push: bool) -> ListReadAuthority {
    if for_push {
        ListReadAuthority::Primary
    } else {
        ListReadAuthority::ReplicaEligible
    }
}

async fn read_store_for_list_batch(
    primary_store: &crate::storage::store::Store,
    prefix: &str,
    remote_url: Option<&str>,
    config: &crate::core::config::Config,
    for_push: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> (crate::storage::store::Store, StoreLayout) {
    read_store_for_list_batch_with_selector(
        primary_store,
        prefix,
        remote_url,
        config,
        for_push,
        cancel,
        |config, parsed, cancel| async move {
            crate::replication::select_read_store(&config, &parsed, "fetch", &cancel).await
        },
    )
    .await
}

async fn read_store_for_list_batch_with_selector<F, Fut>(
    primary_store: &crate::storage::store::Store,
    prefix: &str,
    remote_url: Option<&str>,
    config: &crate::core::config::Config,
    for_push: bool,
    cancel: &tokio_util::sync::CancellationToken,
    select_read: F,
) -> (crate::storage::store::Store, StoreLayout)
where
    F: FnOnce(
        crate::core::config::Config,
        crate::git::url::CrabUrl,
        tokio_util::sync::CancellationToken,
    ) -> Fut,
    Fut: Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    match list_read_authority(for_push) {
        // Git asks `list for-push` before sending updates. That ref
        // advertisement is part of write admission, so stale replicas are
        // unsafe even when they are healthy enough for clone/fetch reads.
        ListReadAuthority::Primary => primary_store_for_batch(primary_store, prefix),
        ListReadAuthority::ReplicaEligible => {
            read_store_for_batch_with_selector(
                primary_store,
                prefix,
                remote_url,
                config,
                cancel,
                select_read,
            )
            .await
        }
    }
}

fn primary_store_for_batch(
    primary_store: &crate::storage::store::Store,
    prefix: &str,
) -> (crate::storage::store::Store, StoreLayout) {
    (
        primary_store.clone(),
        StoreLayout::new(primary_store.clone(), prefix.to_owned()),
    )
}

async fn read_store_for_batch_with_selector<F, Fut>(
    primary_store: &crate::storage::store::Store,
    prefix: &str,
    remote_url: Option<&str>,
    config: &crate::core::config::Config,
    cancel: &tokio_util::sync::CancellationToken,
    select_read: F,
) -> (crate::storage::store::Store, StoreLayout)
where
    F: FnOnce(
        crate::core::config::Config,
        crate::git::url::CrabUrl,
        tokio_util::sync::CancellationToken,
    ) -> Fut,
    Fut: Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    // Managed grants already bind this store to one physical repository view.
    // Re-parsing the logical URL as a direct bucket would bypass that scope.
    if primary_store.storage_scope().is_some() {
        return primary_store_for_batch(primary_store, prefix);
    }
    if config
        .replication
        .as_ref()
        .is_none_or(|replication| !replication.has_read_replicas())
    {
        return primary_store_for_batch(primary_store, prefix);
    }
    let Some(url) = remote_url else {
        return primary_store_for_batch(primary_store, prefix);
    };
    let Ok(parsed) = crate::git::url::CrabUrl::parse(url) else {
        return primary_store_for_batch(primary_store, prefix);
    };

    match select_read(config.clone(), parsed, cancel.clone()).await {
        Ok(selection) => {
            if let crate::replication::ReadSource::Replica { name } = &selection.source {
                tracing::debug!(replica = %name, "selected read replica for remote-helper batch");
            }
            (selection.store, selection.router)
        }
        Err(e) => {
            tracing::debug!(error = %e, "replica read selection failed; using primary");
            primary_store_for_batch(primary_store, prefix)
        }
    }
}

/// Check whether the remote has a `CommitGraphSummary`.
///
/// Returns the cached result when available; otherwise probes the store
/// via a HEAD request and caches the outcome for the rest of the session.
async fn has_commit_graph_summary(
    store: Option<&crate::storage::store::Store>,
    prefix: &str,
    router: Option<&crate::storage::StoreLayout>,
    cache: &mut SessionCache,
) -> bool {
    if let Some(cached) = cache.has_commit_graph {
        return cached;
    }

    let result = if let Some(store) = store {
        let obj_path = if let Some(router) = router {
            router.repo_path("commit-graph-summary")
        } else {
            let path = format!("{prefix}/commit-graph-summary");
            object_store::path::Path::from(path.as_str())
        };
        store.head(&obj_path).await.is_ok()
    } else {
        false
    };

    cache.has_commit_graph = Some(result);
    result
}

/// Build the capabilities response string.
///
/// Always advertises `fetch`, `push`, `option`, and `check-connectivity`.
/// When the remote has a `CommitGraphSummary`, also advertises `shallow` so git
/// knows it can send `--depth`. Partial-clone filtering is not advertised
/// because Crab does not implement promisor-object fetches.
///
/// ## v2 capabilities under `gix-transport`
///
/// The `gix-transport` feature makes the machinery to drive protocol
/// v2 stateless-connect available in
/// [`crate::git::fetch_transport`]. It does **not** currently flip
/// the advertisement here. Advertising `connect` / `stateless-connect`
/// signals to git that the helper is ready to proxy a full
/// stateless-connect session; wiring that session requires
/// `gix_protocol::handshake`, a refmap, negotiation, and a pack
/// install pipeline that reads from the stdio transport instead of
/// S3. Until those pieces are hooked up, emitting the capabilities
/// would break existing fetch clients that currently drive the
/// batched `fetch` command path.
///
/// The gate point is kept here (rather than at the caller) so the
/// advertisement lights up in exactly one place once the rest of
/// the pipeline is ready. When that happens the two `connect\n` and
/// `stateless-connect\n` lines go behind a
/// `#[cfg(feature = "gix-transport")]` block just before the final
/// newline.
pub fn format_capabilities(has_commit_graph: bool) -> String {
    let mut caps = String::from("fetch\npush\noption\ncheck-connectivity\n");
    if has_commit_graph {
        caps.push_str("shallow\n");
    }
    // DO NOT emit `connect` / `stateless-connect` yet — see the
    // doc comment above for the gating rationale. The scaffold in
    // `fetch_transport` is ready to receive requests, but the rest
    // of the fetch-over-stateless-connect pipeline is not wired.
    //
    // `agent=...` goes last so clients parsing line-by-line see
    // the standard capability keywords first, matching git's own
    // `receive-pack` / `upload-pack` ordering.
    caps.push_str("agent=crab/");
    caps.push_str(env!("CARGO_PKG_VERSION"));
    caps.push('\n');
    caps.push('\n');
    caps
}

/// Read refs from the unified manifest pointer.
///
/// Reads `{repo}/manifest` in one small GET, then delegates hidden-ref and
/// HEAD fallback policy to the read-domain manifest advertisement helper.
async fn read_remote_refs(
    store: &crate::storage::store::Store,
    router: &StoreLayout,
    hidden_ref_patterns: &[String],
) -> Result<ListOutput> {
    let (manifest, _etag) = crate::metadata::manifest::read_manifest(store, router).await?;
    let advertisement = crab_read::manifest_ref_advertisement(&manifest, hidden_ref_patterns);

    let refs = advertisement
        .refs
        .into_iter()
        .map(|entry| RefEntry {
            sha: entry.sha,
            ref_name: entry.ref_name,
            peeled: entry.peeled,
        })
        .collect();

    tracing::debug!(
        ref_count = manifest.refs.len(),
        head_symref = ?advertisement.head_symref,
        "read remote refs from manifest"
    );

    Ok(ListOutput {
        refs,
        head_symref: advertisement.head_symref,
    })
}

async fn read_remote_refs_for_advertisement(
    store: &crate::storage::store::Store,
    router: &StoreLayout,
    hidden_ref_patterns: &[String],
) -> Result<ListOutput> {
    match read_remote_refs(store, router, hidden_ref_patterns).await {
        Ok(output) => Ok(output),
        Err(CrabError::NotFound { path }) if path == router.manifest_path().as_ref() => {
            tracing::debug!(
                manifest_path = %path,
                "remote manifest missing; advertising empty refs"
            );
            Ok(ListOutput {
                refs: Vec::new(),
                head_symref: None,
            })
        }
        Err(error) => Err(error),
    }
}

/// Adapts remote-helper fetch entries to the read-domain upload-pack policy.
#[must_use]
pub fn validate_fetch_entries_with_manifest(
    entries: &[FetchEntry],
    manifest: &crate::metadata::manifest::Manifest,
    summary: Option<&crab_metadata::commit_graph::CommitGraphSummary>,
    config: &crate::core::config::Config,
) -> Vec<(
    FetchEntry,
    std::result::Result<(), crate::git::reject_reason::FetchRejectReason>,
)> {
    let wants = entries
        .iter()
        .map(|entry| crab_read::FetchWant::new(&entry.sha, &entry.ref_name))
        .collect::<Vec<_>>();
    let policy = fetch_admission_policy(config);
    crab_read::validate_fetch_wants_with_manifest(&wants, manifest, summary, &policy)
        .into_iter()
        .map(|(want, outcome)| {
            (
                FetchEntry {
                    sha: want.sha,
                    ref_name: want.ref_name,
                },
                outcome.map_err(map_fetch_admission_reject),
            )
        })
        .collect()
}

fn fetch_admission_policy(config: &crate::core::config::Config) -> crab_read::FetchAdmissionPolicy {
    crab_read::FetchAdmissionPolicy {
        allow_any_sha_in_want: config.uploadpack_allow_any_sha_in_want,
        allow_tip_sha_in_want: config.uploadpack_allow_tip_sha_in_want,
        allow_reachable_sha_in_want: config.uploadpack_allow_reachable_sha_in_want,
        transfer_hide_refs: config.transfer_hide_refs.clone(),
    }
}

fn map_fetch_admission_reject(
    reason: crab_read::FetchAdmissionReject,
) -> crate::git::reject_reason::FetchRejectReason {
    use crate::git::reject_reason::FetchRejectReason;

    match reason {
        crab_read::FetchAdmissionReject::NotReachable { sha } => {
            FetchRejectReason::NotReachable { sha }
        }
        crab_read::FetchAdmissionReject::NotAtTip { sha } => FetchRejectReason::NotAtTip { sha },
        crab_read::FetchAdmissionReject::NotAllowed { sha, reason } => {
            FetchRejectReason::NotAllowed { sha, reason }
        }
    }
}

#[derive(Clone)]
struct RemoteFetchStore {
    store: crate::storage::store::Store,
    router: StoreLayout,
    manifest: Manifest,
    caching_store: Option<crab_cache_store::CachingStore>,
    pack_list: Arc<tokio::sync::Mutex<Option<PackList>>>,
    enrich_ref_tips: bool,
    metadata_concurrency: usize,
}

impl RemoteFetchStore {
    fn new(
        store: crate::storage::store::Store,
        router: StoreLayout,
        manifest: Manifest,
        caching_store: Option<crab_cache_store::CachingStore>,
        enrich_ref_tips: bool,
        metadata_concurrency: usize,
    ) -> Self {
        Self {
            store,
            router,
            manifest,
            caching_store,
            pack_list: Arc::new(tokio::sync::Mutex::new(None)),
            enrich_ref_tips,
            metadata_concurrency: metadata_concurrency.max(1),
        }
    }

    async fn cached_pack_list(&self) -> Option<PackList> {
        self.pack_list.lock().await.clone()
    }

    async fn load_pack_list(&self) -> Result<PackList> {
        if let Some(cached) = self.pack_list.lock().await.clone() {
            return Ok(cached);
        }

        let entries = if self.manifest.pack_index_hash.is_empty() {
            Vec::new()
        } else {
            crate::metadata::manifest::read_bulk_pack_list(
                &self.store,
                &self.router,
                &self.manifest.pack_index_hash,
            )
            .await?
            .into_iter()
            .map(|entry| PackEntry::with_ref_tips(entry.pack_id, entry.size, entry.ref_tips))
            .collect()
        };

        let entries =
            if self.enrich_ref_tips && entries.iter().any(|entry| entry.ref_tips.is_none()) {
                self.enrich_pack_ref_tips(entries).await
            } else {
                entries
            };

        let pack_list = PackList {
            generation: self.manifest.generation,
            entries,
        };
        *self.pack_list.lock().await = Some(pack_list.clone());
        Ok(pack_list)
    }

    async fn enrich_pack_ref_tips(&self, entries: Vec<PackEntry>) -> Vec<PackEntry> {
        use futures_util::StreamExt;

        futures_util::stream::iter(entries.into_iter().map(|entry| {
            let this = self.clone();
            async move {
                match this.fetch_pack_metadata(&entry.pack_id).await {
                    Some(metadata) => {
                        PackEntry::with_ref_tips(entry.pack_id, entry.size, metadata.ref_tips)
                    }
                    None => entry,
                }
            }
        }))
        .buffered(self.metadata_concurrency)
        .collect()
        .await
    }

    async fn fetch_pack_metadata(&self, pack_id: &str) -> Option<PackMetadata> {
        let path = self.router.pack_metadata_path(pack_id);
        let body = if let Some(cs) = &self.caching_store {
            cs.get_with_etag(&path).await.ok()?.0
        } else {
            self.store.get_with_etag(&path).await.ok()?.0
        };

        match serde_json::from_slice::<PackMetadata>(&body) {
            Ok(metadata) if metadata.pack_id == pack_id => Some(metadata),
            Ok(metadata) => {
                tracing::debug!(
                    pack_id,
                    metadata_pack_id = %metadata.pack_id,
                    "pack metadata id mismatch; treating pack as legacy"
                );
                None
            }
            Err(e) => {
                tracing::debug!(
                    pack_id,
                    error = %e,
                    "pack metadata unreadable; treating pack as legacy"
                );
                None
            }
        }
    }

    async fn download_pack_to_local_path(
        &self,
        pack_id: &str,
        dest: &std::path::Path,
    ) -> Result<u64> {
        let path = self.router.pack_path(pack_id);
        self.store.download_to_path(&path, dest).await
    }
}

impl PackStore for RemoteFetchStore {
    async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
        let pack_list = self.load_pack_list().await?;
        Ok(pack_list
            .entries
            .into_iter()
            .map(|entry| PackInfo {
                pack_id: entry.pack_id,
                size: entry.size,
            })
            .collect())
    }

    async fn download_pack_to_path(&self, pack_id: &str, dest: &std::path::Path) -> Result<u64> {
        self.download_pack_to_local_path(pack_id, dest).await
    }

    async fn validate_pack_index(&self, pack_id: &str) -> Result<Option<String>> {
        let path = self.router.pack_index_path(pack_id);
        let temp = tempfile::NamedTempFile::new()?;
        self.store.download_to_path(&path, temp.path()).await?;
        let display_path = path.to_string();
        let checksum = tokio::task::spawn_blocking(move || {
            crab_git::pack::verify_pack_index_file(temp.path())
        })
        .await
        .map_err(|error| CrabError::Internal(format!("pack index validation join: {error}")))?
        .map_err(|error| CrabError::CorruptObject {
            path: display_path,
            reason: error.to_string(),
        })?;
        Ok(Some(checksum))
    }
}

impl CommitGraphProvider for RemoteFetchStore {
    async fn fetch_commit_graph_summary(&self) -> Result<Option<CommitGraphSummary>> {
        let path = self.router.repo_path("commit-graph-summary");
        match self.store.get_with_etag(&path).await {
            Ok((body, _etag)) => {
                let summary = serde_json::from_slice::<CommitGraphSummary>(&body).map_err(|e| {
                    CrabError::CorruptObject {
                        path: path.to_string(),
                        reason: format!("invalid commit-graph-summary JSON: {e}"),
                    }
                })?;
                Ok(Some(summary))
            }
            Err(CrabError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn fetch_pack_list(&self) -> Result<PackList> {
        self.load_pack_list().await
    }
}

/// Fetch packs from the remote and install them into the local git repo.
///
/// Keeps the remote-helper upload-pack policy gate local, then delegates
/// pack selection, concurrent download, and atomic installation to the shared
/// fetch pipeline.
///
/// `remote_url` is the full `crab://bucket/repo...` URL the remote helper was
/// invoked with, threaded from `dispatch_batch` so the `.crab/remote` file can
/// be written with the authoritative URL instead of reconstructing it from the
/// prefix or shelling out to `git remote get-url`.
#[expect(
    clippy::too_many_arguments,
    reason = "fetch pack transfer carries store, routing, protocol writer, cache, and cancellation state"
)]
async fn fetch_packs(
    store: &crate::storage::store::Store,
    router: &StoreLayout,
    entries: &[FetchEntry],
    fetch_options: &FetchOptions,
    remote_url: Option<&str>,
    config: &crate::core::config::Config,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    caching_store: Option<&crab_cache_store::CachingStore>,
    cache: &mut SessionCache,
    cancel: &tokio_util::sync::CancellationToken,
    check_connectivity: bool,
) -> Result<Option<std::path::PathBuf>> {
    // Read the manifest to get the pack list hash.
    let (manifest, _etag) = crate::metadata::manifest::read_manifest(store, router).await?;

    let fetch_store = Arc::new(RemoteFetchStore::new(
        store.clone(),
        router.clone(),
        manifest.clone(),
        caching_store.cloned(),
        config.fetch_ref_filtering,
        config.download_concurrency,
    ));

    // Upload-pack policy gate. Raw-SHA `fetch <sha> <ref>` lines
    // must be validated before we hand the client back any pack
    // bytes, otherwise a client can ask for an arbitrary interior
    // commit SHA and receive its pack data — the same
    // information-leak vector that git's
    // `uploadpack.allow*SHA1InWant` knobs were added to cover.
    //
    // A rejected entry produces a per-entry `error {ref}
    // {protocol-tag} ({detail})` line on the writer (matching the
    // push response shape), but does not fail the batch. If every
    // entry is rejected, we skip the pack download entirely — the
    // trailing `\n` that terminates the fetch response is emitted
    // by the caller in `dispatch_batch`.
    //
    if !entries.is_empty() {
        let summary = if config.uploadpack_allow_reachable_sha_in_want
            && !config.uploadpack_allow_any_sha_in_want
        {
            fetch_store.fetch_commit_graph_summary().await?
        } else {
            None
        };
        let validation =
            validate_fetch_entries_with_manifest(entries, &manifest, summary.as_ref(), config);
        let mut any_allowed = false;
        for (entry, outcome) in &validation {
            match outcome {
                Ok(()) => {
                    any_allowed = true;
                }
                Err(reason) => {
                    let detail = one_line_protocol_text(&reason.to_string());
                    let line = format!(
                        "error {} {} ({})\n",
                        entry.ref_name,
                        reason.protocol_tag(),
                        detail
                    );
                    writer.write_all(line.as_bytes()).await?;
                    tracing::warn!(
                        sha = %entry.sha,
                        ref_name = %entry.ref_name,
                        tag = reason.protocol_tag(),
                        "rejected fetch entry on upload-pack policy"
                    );
                }
            }
        }
        // Every entry was rejected — skip the pack download path so
        // a hostile client cannot induce any store reads.
        if !any_allowed {
            writer.flush().await?;
            return Ok(None);
        }
    }

    let git_dir = super::discover::discover_git_dir()?;
    let mut fetch_config = FetchConfig::from_config(config);
    fetch_config.git_dir = git_dir.clone();

    let installed = run_fetch_batch(
        entries,
        &manifest,
        &fetch_config,
        fetch_store.clone(),
        Some(fetch_store.as_ref()),
        fetch_options,
        cancel,
    )
    .await?;

    if let Some(pack_list) = fetch_store.cached_pack_list().await {
        cache.pack_list = Some(pack_list);
    }

    tracing::info!(
        installed_packs = installed.len(),
        depth = ?fetch_options.depth,
        "remote-helper fetch pipeline complete"
    );

    // After a successful fetch, ensure the filter driver is configured
    // in the local repo. This is critical for clones — without it, the
    // smudge filter won't run and pointer files won't be reconstructed.
    //
    // `git_dir` may be relative (e.g. `.git` when the remote helper is
    // invoked with `GIT_DIR=.git`), in which case `.parent()` returns
    // `Some("")` — an empty path that fails as `current_dir` for
    // spawned git subprocesses with ENOENT. Canonicalize first, then
    // fall back to the current working directory so `install_filter_driver`
    // always receives a usable repo root.
    let repo_root = repo_root_from_git_dir(&git_dir);
    if let Err(e) = crate::cmd::init::install_filter_driver(&repo_root) {
        tracing::warn!(error = %e, "failed to install filter driver after fetch");
    } else {
        tracing::debug!("filter driver installed after fetch");
    }
    if let Err(e) = crate::cmd::init::ensure_crab_dir_excluded(&repo_root) {
        tracing::warn!(error = %e, "failed to exclude local .crab state after fetch");
    }

    // Write the remote URL to .crab/remote so `crab hydrate` can
    // find the S3 bucket for reconstruction. Prefer the authoritative
    // URL threaded from the remote-helper invocation; only fall back
    // to `gix::Repository::find_remote("origin")` (under `gix-transport`)
    // or `git remote get-url origin` (legacy) when the caller didn't
    // provide one — older callers and test harnesses still hit that
    // branch.
    let crab_dir = repo_root.join(".crab");
    std::fs::create_dir_all(&crab_dir)?;
    let remote_file = crab_dir.join("remote");
    if let Some(url) = remote_url.filter(|url| !url.is_empty()) {
        std::fs::write(&remote_file, url.as_bytes())?;
        tracing::debug!(url = %url, "wrote remote URL from helper invocation");
    } else if !remote_file.exists() {
        #[cfg(feature = "gix-transport")]
        {
            match crate::git::fetch_transport::remote_origin_url(&repo_root) {
                Ok(Some(url)) if !url.is_empty() => {
                    let _ = std::fs::write(&remote_file, url.as_bytes());
                    tracing::debug!(
                        url = %url,
                        "wrote remote URL via gix::Repository::find_remote"
                    );
                }
                Ok(_) => {
                    tracing::debug!("no origin remote configured; leaving .crab/remote empty");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "gix find_remote failed; leaving .crab/remote empty");
                }
            }
        }
        #[cfg(not(feature = "gix-transport"))]
        {
            // SHELLOUT: fallback lookup for the remote URL. Under
            // `--features gix-transport` this path is replaced by
            // `crate::git::fetch_transport::remote_origin_url` above.
            if let Ok(output) = std::process::Command::new("git")
                .args(["remote", "get-url", "origin"])
                .output()
            {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !url.is_empty() {
                    let _ = std::fs::write(&remote_file, url.as_bytes());
                    tracing::debug!(url = %url, "wrote remote URL from git config (fallback)");
                }
            }
        }
    }
    ensure_lazy_checkout_config_for_new_helper_repo(&repo_root);

    // Git accepts the producer proof only when the helper also identifies one
    // pack containing every requested tip. Shallow or filtered selections
    // intentionally retain Git's boundary-aware connectivity check.
    if !check_connectivity || fetch_options.has_constraints() {
        return Ok(None);
    }
    let ref_tips = entries
        .iter()
        .map(|entry| entry.sha.clone())
        .collect::<Vec<_>>();
    crate::git::pack::create_connectivity_proof_pack(
        &git_dir,
        &ref_tips,
        &manifest.git_validation_digest,
    )
    .await
}

fn ensure_lazy_checkout_config_for_new_helper_repo(repo_root: &std::path::Path) {
    let config_path = repo_root.join(".crab").join("config.toml");
    if config_path.exists() {
        return;
    }

    if let Err(e) = crate::cmd::config::run_config_set_at("checkout.lazy", "true", &config_path) {
        tracing::warn!(
            path = %config_path.display(),
            error = %e,
            "failed to seed lazy checkout config after fetch"
        );
    } else {
        tracing::debug!(
            path = %config_path.display(),
            "seeded lazy checkout config after fetch"
        );
    }
}

/// Resolve a usable repo-root path from a discovered `git_dir`.
///
/// Prefer the current [`WorktreeContext`] so linked worktree private git dirs
/// resolve to the linked worktree root, not `$GIT_COMMON_DIR/worktrees`.
/// Falls back to the `gitdir` back-pointer and then the historical parent
/// logic for non-worktree test harnesses and partial repos.
///
/// Prior implementations returned `git_dir.parent().unwrap_or(&git_dir)`,
/// which produced an empty `PathBuf` when `git_dir == ".git"`. Passing
/// `""` as `current_dir` to a spawned subprocess fails with ENOENT on
/// POSIX, surfacing as the misleading "filter driver install failed"
/// warning during `git pull`.
fn repo_root_from_git_dir(git_dir: &std::path::Path) -> std::path::PathBuf {
    if let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve() {
        let resolved_git_dir = absolute_git_dir_path(git_dir);
        if same_path(&ctx.per_worktree_git_dir, &resolved_git_dir) {
            return ctx.current_worktree_root;
        }
    }

    if let Some(root) = linked_worktree_root_from_git_dir(git_dir) {
        return root;
    }

    if git_dir.is_absolute() {
        return git_dir
            .parent()
            .map_or_else(|| git_dir.to_path_buf(), std::path::Path::to_path_buf);
    }

    if let Ok(canonical) = git_dir.canonicalize() {
        if let Some(parent) = canonical.parent() {
            return parent.to_path_buf();
        }
        return canonical;
    }

    // Last-resort fallback: the remote helper's cwd is the repo work tree
    // when git invokes it for a non-bare repository.
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn absolute_git_dir_path(git_dir: &std::path::Path) -> std::path::PathBuf {
    if git_dir.is_absolute() {
        return git_dir.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| git_dir.to_path_buf(), |cwd| cwd.join(git_dir))
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn linked_worktree_root_from_git_dir(git_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let gitdir_text = std::fs::read_to_string(git_dir.join("gitdir")).ok()?;
    let gitfile_path = std::path::PathBuf::from(gitdir_text.trim());
    let gitfile_path = if gitfile_path.is_absolute() {
        gitfile_path
    } else {
        git_dir.join(gitfile_path)
    };
    let root = gitfile_path.parent()?.to_path_buf();
    Some(root.canonicalize().unwrap_or(root))
}

///
/// Reads the pack count from the manifest's bulk pack-list. Uses the
/// session cache for config resolution. Falls back silently on errors.
async fn check_repack_threshold(
    store: &crate::storage::store::Store,
    router: &StoreLayout,
    cache: &mut SessionCache,
) {
    let threshold = cache.config().repack_auto_threshold;

    let pack_count = if let Some(ref pl) = cache.pack_list {
        pl.entries.len()
    } else {
        // Read the manifest to get the pack list hash, then read the bulk pack-list.
        let Ok((manifest, _etag)) = crate::metadata::manifest::read_manifest(store, router).await
        else {
            return;
        };

        if manifest.pack_index_hash.is_empty() {
            return;
        }

        match crate::metadata::manifest::read_bulk_pack_list(
            store,
            router,
            &manifest.pack_index_hash,
        )
        .await
        {
            Ok(entries) => {
                let count = entries.len();
                // Cache as a PackList for compatibility with the session cache.
                cache.pack_list = Some(crab_metadata::manifests::PackList {
                    generation: manifest.generation,
                    entries: entries
                        .iter()
                        .map(|e| {
                            crab_metadata::manifests::PackEntry::with_ref_tips(
                                &e.pack_id,
                                e.size,
                                e.ref_tips.clone(),
                            )
                        })
                        .collect(),
                });
                count
            }
            Err(_) => return,
        }
    };

    if pack_count > threshold {
        eprintln!(
            "warning: repository has {pack_count} packs (threshold: {threshold}). \
             Consider running `crab repack` to consolidate."
        );
    }
}

#[cfg(test)]
#[allow(
    deprecated,
    reason = "tests still construct RefPushOutcome::Error for backward-compat regression coverage"
)]
mod tests {
    use super::*;
    use crate::test::git_repo::{CacheDirGuard, CleanGitEnvGuard, GIT_DIR_MUTEX, TEST_GIT_REPO};
    use std::io::Cursor;
    use std::sync::MutexGuard;
    use tokio::io::{AsyncReadExt, BufReader, duplex};

    struct DuplexIo {
        reader: BufReader<std::io::Cursor<Vec<u8>>>,
        writer: tokio::io::DuplexStream,
    }

    impl StdIo for DuplexIo {
        type Reader = BufReader<std::io::Cursor<Vec<u8>>>;
        type Writer = tokio::io::DuplexStream;

        fn split(self) -> (Self::Reader, Self::Writer) {
            (self.reader, self.writer)
        }
    }

    #[test]
    fn internal_bool_value_accepts_only_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(internal_bool_value(value), "{value}");
        }

        for value in ["", "0", "false", "no", "off", "maybe"] {
            assert!(!internal_bool_value(value), "{value}");
        }
    }

    #[test]
    fn internal_agent_rebase_override_only_enables_ref_filtering() {
        let mut config = crate::core::config::Config::default();
        config.fetch_ref_filtering = false;
        apply_internal_remote_helper_overrides_for_agent_rebase(&mut config, true);
        assert!(config.fetch_ref_filtering);

        let mut config = crate::core::config::Config::default();
        config.fetch_ref_filtering = false;
        apply_internal_remote_helper_overrides_for_agent_rebase(&mut config, false);
        assert!(!config.fetch_ref_filtering);
    }

    fn test_url() -> gix_url::Url {
        gix_url::Url::from_bytes(b"crab://bucket/repo".into()).unwrap()
    }

    struct GitWorktreeGuard {
        _lock: MutexGuard<'static, ()>,
        prev_git_dir: Option<String>,
        prev_cwd: Option<std::path::PathBuf>,
    }

    impl GitWorktreeGuard {
        fn new() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_git_dir = std::env::var("GIT_DIR").ok();
            let prev_cwd = std::env::current_dir().ok();
            let worktree = TEST_GIT_REPO
                .git_dir
                .parent()
                .expect("test git repo must have a worktree");
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            unsafe { std::env::set_var("GIT_DIR", &TEST_GIT_REPO.git_dir) };
            std::env::set_current_dir(worktree).expect("set test git repo cwd");
            Self {
                _lock: lock,
                prev_git_dir,
                prev_cwd,
            }
        }
    }

    impl Drop for GitWorktreeGuard {
        fn drop(&mut self) {
            if let Some(cwd) = &self.prev_cwd {
                let _ = std::env::set_current_dir(cwd);
            }
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            match &self.prev_git_dir {
                Some(value) => unsafe { std::env::set_var("GIT_DIR", value) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
            }
        }
    }

    struct GitEnvCwdGuard {
        _lock: MutexGuard<'static, ()>,
        prev_git_dir: Option<String>,
        prev_git_work_tree: Option<String>,
        prev_git_common_dir: Option<String>,
        prev_cwd: Option<std::path::PathBuf>,
    }

    impl GitEnvCwdGuard {
        fn set(
            cwd: &std::path::Path,
            git_dir: &std::path::Path,
            work_tree: &std::path::Path,
        ) -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_git_dir = std::env::var("GIT_DIR").ok();
            let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
            let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
            let prev_cwd = std::env::current_dir().ok();

            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            unsafe {
                std::env::set_var("GIT_DIR", git_dir);
                std::env::set_var("GIT_WORK_TREE", work_tree);
                std::env::remove_var("GIT_COMMON_DIR");
            }
            std::env::set_current_dir(cwd).expect("set git cwd");

            Self {
                _lock: lock,
                prev_git_dir,
                prev_git_work_tree,
                prev_git_common_dir,
                prev_cwd,
            }
        }
    }

    impl Drop for GitEnvCwdGuard {
        fn drop(&mut self) {
            if let Some(cwd) = &self.prev_cwd {
                let _ = std::env::set_current_dir(cwd);
            }
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            unsafe {
                match &self.prev_git_dir {
                    Some(value) => std::env::set_var("GIT_DIR", value),
                    None => std::env::remove_var("GIT_DIR"),
                }
                match &self.prev_git_work_tree {
                    Some(value) => std::env::set_var("GIT_WORK_TREE", value),
                    None => std::env::remove_var("GIT_WORK_TREE"),
                }
                match &self.prev_git_common_dir {
                    Some(value) => std::env::set_var("GIT_COMMON_DIR", value),
                    None => std::env::remove_var("GIT_COMMON_DIR"),
                }
            }
        }
    }

    fn test_context(
        store: crate::storage::store::Store,
        prefix: &str,
        push_state_repo_root: std::path::PathBuf,
    ) -> RemoteHelperContext {
        RemoteHelperContext {
            store,
            staging: None,
            prefix: prefix.to_owned(),
            cache: SessionCache::new(crate::core::config::Config::default()),
            push_state: PushState::default(),
            push_state_repo_root,
            progress_mode: OutputMode::Text,
            jsonl_stderr_stream: None,
            invocation_url: format!("crab://bucket/{prefix}"),
            managed_repository: None,
            caching_store: None,
        }
    }

    async fn run_with_context(
        input: &str,
        context: RemoteHelperContext,
        cancel: tokio_util::sync::CancellationToken,
    ) -> (String, Result<()>) {
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);
        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input.as_bytes().to_vec())),
            writer: writer_tx,
        };
        let mut output = Vec::new();
        let helper = run_remote_helper_with_context("origin", io, context, cancel);
        let reader = writer_rx.read_to_end(&mut output);
        let (helper_result, read_result) = tokio::join!(helper, reader);
        read_result.expect("read helper output");
        (
            String::from_utf8(output).expect("utf8 helper output"),
            helper_result,
        )
    }

    /// Run the production protocol loop with a resolved in-memory context.
    async fn run(input: &str) -> String {
        let push_state_root = tempfile::tempdir().expect("push state tempdir");
        let store = crate::storage::store::Store::new(std::sync::Arc::new(
            object_store::memory::InMemory::new(),
        ));
        let context = test_context(
            store,
            "remote-helper-unit",
            push_state_root.path().to_path_buf(),
        );
        let (output, result) =
            run_with_context(input, context, tokio_util::sync::CancellationToken::new()).await;
        result.expect("remote helper protocol session");
        output
    }

    #[tokio::test]
    async fn capabilities_response() {
        let output = run("capabilities\n").await;
        let expected = format!(
            "fetch\npush\noption\ncheck-connectivity\nagent=crab/{}\n\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(output, expected);
    }

    #[tokio::test]
    async fn option_progress_true() {
        let output = run("option progress true\n").await;
        assert_eq!(output, "ok\n");
    }

    #[tokio::test]
    async fn option_progress_false() {
        let output = run("option progress false\n").await;
        assert_eq!(output, "ok\n");
    }

    #[tokio::test]
    async fn option_verbosity() {
        let output = run("option verbosity 2\n").await;
        assert_eq!(output, "ok\n");
    }

    #[tokio::test]
    async fn option_unknown_yields_unsupported() {
        let output = run("option foobar baz\n").await;
        assert_eq!(output, "unsupported\n");
    }

    #[tokio::test]
    async fn list_returns_empty_stub() {
        let output = run("list\n").await;
        assert_eq!(output, "\n");
    }

    #[tokio::test]
    async fn list_for_push_returns_empty_stub() {
        let output = run("list for-push\n").await;
        assert_eq!(output, "\n");
    }

    #[test]
    fn list_for_push_reads_write_authoritative_refs() {
        assert_eq!(list_read_authority(true), ListReadAuthority::Primary);
        assert_eq!(
            list_read_authority(false),
            ListReadAuthority::ReplicaEligible
        );
    }

    #[tokio::test]
    async fn read_only_list_batch_uses_selected_replica_refs() {
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "1111111111111111111111111111111111111111",
        )
        .await;
        let (replica_store, replica_router) = memory_store_with_manifest(
            "refs/heads/main",
            "2222222222222222222222222222222222222222",
        )
        .await;
        let config = config_with_read_replica();
        let cancel = tokio_util::sync::CancellationToken::new();

        let (read_store, router) = read_store_for_list_batch_with_selector(
            &primary_store,
            "org/repo",
            Some("crab://primary/org/repo"),
            &config,
            false,
            &cancel,
            move |_, _, _| {
                let replica_store = replica_store.clone();
                let replica_router = replica_router.clone();
                async move {
                    Ok(crate::replication::ReadStoreSelection {
                        store: replica_store,
                        router: replica_router,
                        source: crate::replication::ReadSource::Replica {
                            name: "west".into(),
                        },
                    })
                }
            },
        )
        .await;

        let output = read_remote_refs(&read_store, &router, &[])
            .await
            .expect("read replica refs");

        assert_eq!(
            output.refs[0].sha,
            "2222222222222222222222222222222222222222"
        );
    }

    #[tokio::test]
    async fn list_for_push_batch_skips_replica_selector() {
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "3333333333333333333333333333333333333333",
        )
        .await;
        let config = crate::core::config::Config::default();
        let cancel = tokio_util::sync::CancellationToken::new();

        let (read_store, router) = read_store_for_list_batch_with_selector(
            &primary_store,
            "org/repo",
            Some("crab://primary/org/repo"),
            &config,
            true,
            &cancel,
            |_, _, _| async {
                Err(CrabError::Internal(
                    "list for-push must not select a read replica".into(),
                ))
            },
        )
        .await;

        let output = read_remote_refs(&read_store, &router, &[])
            .await
            .expect("read primary refs");

        assert_eq!(
            output.refs[0].sha,
            "3333333333333333333333333333333333333333"
        );
    }

    #[tokio::test]
    async fn scoped_managed_list_skips_direct_replica_selection() {
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "5555555555555555555555555555555555555555",
        )
        .await;
        let primary_store = primary_store.with_storage_scope(crab_types::storage::StorageScope {
            repo_prefix: "org/repo".to_owned(),
            global_prefix: "org/repo/.crab".to_owned(),
            source_repo: "org/repo".to_owned(),
            scope_hash: "5".repeat(64),
        });
        let config = crate::core::config::Config::default();
        let cancel = tokio_util::sync::CancellationToken::new();

        let (read_store, router) = read_store_for_list_batch_with_selector(
            &primary_store,
            "org/repo",
            Some("crab://crab.crab.test/acme/models"),
            &config,
            false,
            &cancel,
            |_, _, _| async {
                Err(CrabError::Internal(
                    "managed store must not enter direct replica selection".into(),
                ))
            },
        )
        .await;

        let output = read_remote_refs(&read_store, &router, &[])
            .await
            .expect("read scoped primary refs");

        assert_eq!(
            output.refs[0].sha,
            "5555555555555555555555555555555555555555"
        );
        assert_eq!(router.global_prefix(), "org/repo/.crab");
    }

    #[tokio::test]
    async fn direct_read_without_replicas_uses_resolved_primary_store() {
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "7777777777777777777777777777777777777777",
        )
        .await;
        let config = crate::core::config::Config::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let selector_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let selector_called_in_closure = std::sync::Arc::clone(&selector_called);

        let (read_store, router) = read_store_for_batch_with_selector(
            &primary_store,
            "org/repo",
            Some("crab://primary/org/repo"),
            &config,
            &cancel,
            move |_, _, _| {
                selector_called_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Err(CrabError::Internal("selector must not run".into())) }
            },
        )
        .await;

        let output = read_remote_refs(&read_store, &router, &[])
            .await
            .expect("read resolved primary refs");

        assert!(!selector_called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            output.refs[0].sha,
            "7777777777777777777777777777777777777777"
        );
    }

    #[tokio::test]
    async fn fetch_batch_accepts_refs_from_selected_replica_manifest() {
        let _guard = GitWorktreeGuard::new();
        let replica_tip = TEST_GIT_REPO.commit_sha.clone();
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "4444444444444444444444444444444444444444",
        )
        .await;
        let (replica_store, replica_router) =
            memory_store_with_manifest("refs/heads/main", &replica_tip).await;
        let options = HelperOptions {
            check_connectivity: true,
            ..HelperOptions::default()
        };
        let mut cache = fetch_test_cache_with_replica();
        let cancel = tokio_util::sync::CancellationToken::new();
        let entries = vec![FetchEntry {
            sha: replica_tip,
            ref_name: "refs/heads/main".into(),
        }];
        let mut writer = Vec::new();

        dispatch_fetch_batch_with_selector(
            &entries,
            &options,
            &mut writer,
            Some(&primary_store),
            "org/repo",
            &mut cache,
            Some("crab://primary/org/repo"),
            None,
            &cancel,
            move |_, _, _| {
                let replica_store = replica_store.clone();
                let replica_router = replica_router.clone();
                async move {
                    Ok(crate::replication::ReadStoreSelection {
                        store: replica_store,
                        router: replica_router,
                        source: crate::replication::ReadSource::Replica {
                            name: "west".into(),
                        },
                    })
                }
            },
        )
        .await
        .expect("fetch batch");

        let output = String::from_utf8(writer).expect("utf8 output");
        let mut lines = output.lines();
        let keep_path = lines
            .next()
            .and_then(|line| line.strip_prefix("lock "))
            .map(std::path::PathBuf::from)
            .expect("fetch response must identify its connectivity proof pack");
        assert!(keep_path.is_file());
        assert!(keep_path.with_extension("idx").is_file());
        assert_eq!(lines.next(), Some("connectivity-ok"));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), None);
    }

    #[tokio::test]
    async fn fetch_batch_selector_failure_uses_primary_manifest_policy() {
        let _guard = GitWorktreeGuard::new();
        let (primary_store, _primary_router) = memory_store_with_manifest(
            "refs/heads/main",
            "6666666666666666666666666666666666666666",
        )
        .await;
        let options = HelperOptions::default();
        let mut cache = fetch_test_cache_with_replica();
        let cancel = tokio_util::sync::CancellationToken::new();
        let entries = vec![FetchEntry {
            sha: "7777777777777777777777777777777777777777".into(),
            ref_name: "refs/heads/main".into(),
        }];
        let mut writer = Vec::new();

        dispatch_fetch_batch_with_selector(
            &entries,
            &options,
            &mut writer,
            Some(&primary_store),
            "org/repo",
            &mut cache,
            Some("crab://primary/org/repo"),
            None,
            &cancel,
            |_, _, _| async {
                Err(CrabError::Internal(
                    "replica selection unavailable in test".into(),
                ))
            },
        )
        .await
        .expect("fetch batch");

        let output = String::from_utf8(writer).expect("utf8 output");
        assert!(output.contains("error refs/heads/main not-at-tip"));
    }

    async fn memory_store_with_manifest(
        ref_name: &str,
        sha: &str,
    ) -> (crate::storage::store::Store, StoreLayout) {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());
        let refs = BTreeMap::from([(ref_name.to_owned(), sha.to_owned())]);
        let mut manifest = Manifest {
            version: 2,
            generation: 1,
            created_at: "2026-06-16T00:00:00Z".to_owned(),
            pusher: None,
            session_id: "remote-helper-test".to_owned(),
            refs,
            peeled_refs: BTreeMap::new(),
            head: ref_name.to_owned(),
            shard_index_hash: String::new(),
            pack_index_hash: String::new(),
            git_validation_digest: String::new(),
            commit_graph_hash: None,
            ref_registry_hash: None,
        };
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest)
            .await
            .expect("write manifest");
        (store, router)
    }

    #[tokio::test]
    async fn remote_fetch_store_uses_the_admitted_manifest_snapshot() {
        let (store, router) = memory_store_with_manifest(
            "refs/heads/main",
            "6666666666666666666666666666666666666666",
        )
        .await;
        let (admitted, etag) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("read admitted manifest");
        let fetch_store = RemoteFetchStore::new(
            store.clone(),
            router.clone(),
            admitted.clone(),
            None,
            false,
            1,
        );

        let mut newer = admitted;
        newer.generation += 1;
        newer.seal_git_validation();
        crate::metadata::manifest::write_manifest_cas(&store, &router, &newer, &etag)
            .await
            .expect("advance remote manifest");

        let pack_list = fetch_store
            .load_pack_list()
            .await
            .expect("load admitted pack list");
        assert_eq!(pack_list.generation, 1);
    }

    #[tokio::test]
    async fn remote_fetch_store_rejects_missing_empty_and_corrupt_pack_index() {
        let (store, router) = memory_store_with_manifest(
            "refs/heads/main",
            "6666666666666666666666666666666666666666",
        )
        .await;
        let (manifest, _) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("read manifest");
        let fetch_store =
            RemoteFetchStore::new(store.clone(), router.clone(), manifest, None, false, 1);
        let pack_id = "a".repeat(64);

        let missing = fetch_store
            .validate_pack_index(&pack_id)
            .await
            .expect_err("missing remote index must fail fetch");
        assert!(matches!(
            missing,
            CrabError::NotFound { path } if path == router.pack_index_path(&pack_id).as_ref()
        ));

        store
            .put_overwrite(&router.pack_index_path(&pack_id), bytes::Bytes::new())
            .await
            .expect("write empty index");
        let empty = fetch_store
            .validate_pack_index(&pack_id)
            .await
            .expect_err("empty remote index must fail fetch");
        assert!(matches!(
            empty,
            CrabError::CorruptObject { path, .. }
                if path == router.pack_index_path(&pack_id).as_ref()
        ));

        store
            .put_overwrite(
                &router.pack_index_path(&pack_id),
                bytes::Bytes::from_static(b"not a git pack index"),
            )
            .await
            .expect("write corrupt index");
        let corrupt = fetch_store
            .validate_pack_index(&pack_id)
            .await
            .expect_err("non-empty corrupt remote index must fail fetch");
        assert!(matches!(
            corrupt,
            CrabError::CorruptObject { path, .. }
                if path == router.pack_index_path(&pack_id).as_ref()
        ));
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn deterministic_bytes(size: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut data = Vec::with_capacity(size);
        for _ in 0..size {
            state ^= state << 7;
            state ^= state >> 9;
            state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            data.push((state >> 32) as u8);
        }
        data
    }

    async fn stage_content(
        staging_root: std::path::PathBuf,
        content: &[u8],
    ) -> crab_types::pointer::Pointer {
        use crab_staging::StagingArea;
        use crab_types::pointer::Pointer;
        use crab_xet::chunker::GearChunker;
        use crab_xet::hash::{MerkleHash, compute_data_hash};

        let raw_file_hash = *blake3::hash(content).as_bytes();
        let file_hash = MerkleHash::from(raw_file_hash);
        let mut chunker = GearChunker::new();
        let mut chunks = chunker.feed(content);
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }

        let staging = StagingArea::open(staging_root).await.expect("open staging");
        staging
            .pre_register_file(&file_hash, content.len() as u64)
            .expect("pre-register file");

        let batch: Vec<(MerkleHash, &[u8])> = chunks
            .iter()
            .map(|chunk| (compute_data_hash(&chunk.data), chunk.data.as_ref()))
            .collect();
        let refs: Vec<(&MerkleHash, &[u8])> =
            batch.iter().map(|(hash, data)| (hash, *data)).collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .expect("stage chunks");
        staging.flush_pending().await.expect("flush staging");
        let recipe_chunks = batch
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect::<Vec<_>>();
        let recipe = crab_staging::recipe::FileRecipe::from_staged_chunks(
            crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_hash,
            content.len() as u64,
            &recipe_chunks,
        )
        .expect("build staged recipe");
        staging
            .publish_verified_recipe_lease(std::path::Path::new("large.bin"), &recipe)
            .expect("publish staged recipe");
        staging.close().await.expect("close staging");

        Pointer {
            file_hash: raw_file_hash,
            size: content.len() as u64,
            shard_hint: None,
        }
    }

    fn fetch_test_cache_with_replica() -> SessionCache {
        let mut config = config_with_read_replica();
        config.uploadpack_allow_any_sha_in_want = false;
        config.uploadpack_allow_tip_sha_in_want = true;
        config.uploadpack_allow_reachable_sha_in_want = false;
        SessionCache::new(config)
    }

    fn config_with_read_replica() -> crate::core::config::Config {
        let mut config = crate::core::config::Config::default();
        config.replication = Some(crab_types::replication::ReplicationConfig {
            replicas: vec![crab_types::replication::ReplicaConfig {
                name: "test-replica".to_owned(),
                provider: crab_types::replication::ReplicationProviderKind::S3,
                url: "s3://test-replica".to_owned(),
                region: "us-west-2".to_owned(),
                backfill: false,
                read: true,
                rpo: crab_types::replication::ReplicationRpo::Standard,
            }],
            ..Default::default()
        });
        config
    }

    #[tokio::test]
    async fn fetch_batch_collected_until_blank_line() {
        let input = "fetch abc123 refs/heads/main\nfetch def456 refs/heads/dev\n\n";
        let mut reader = BufReader::new(Cursor::new(input.as_bytes()));
        let mut writer = Vec::new();
        let mut options = HelperOptions::default();
        let mut line_buf = String::new();

        let batch = read_batch(&mut reader, &mut line_buf, &mut options, &mut writer)
            .await
            .expect("read fetch batch")
            .expect("fetch batch");

        let Batch::Fetch(entries) = batch else {
            panic!("expected fetch batch");
        };
        assert_eq!(
            entries,
            vec![
                FetchEntry {
                    sha: "abc123".to_owned(),
                    ref_name: "refs/heads/main".to_owned(),
                },
                FetchEntry {
                    sha: "def456".to_owned(),
                    ref_name: "refs/heads/dev".to_owned(),
                },
            ]
        );
        assert!(writer.is_empty());
    }

    #[tokio::test]
    async fn push_batch_emits_ok_per_ref() {
        let _guard = GitWorktreeGuard::new();
        let input = "push refs/heads/main:refs/heads/main\n\n";
        let output = run(input).await;
        assert_eq!(output, "ok refs/heads/main\n\n");
    }

    #[tokio::test]
    async fn push_force_parsed_correctly() {
        let _guard = GitWorktreeGuard::new();
        let input = "push +refs/heads/main:refs/heads/main\n\n";
        let output = run(input).await;
        assert_eq!(output, "ok refs/heads/main\n\n");
    }

    #[tokio::test]
    async fn multi_push_batch() {
        let _guard = GitWorktreeGuard::new();
        let input = "push refs/heads/main:refs/heads/main\npush refs/heads/dev:refs/heads/dev\n\n";
        let output = run(input).await;
        assert_eq!(output, "ok refs/heads/main\nok refs/heads/dev\n\n");
    }

    #[tokio::test]
    async fn push_batch_honors_dispatch_cancellation_token() {
        let _guard = GitWorktreeGuard::new();
        let mut cache = SessionCache::new(crate::core::config::Config::default());
        let mut push_state = PushState::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let batch = Batch::Push(vec![PushItem::Spec(PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        })]);
        let mut writer = Vec::new();
        let store = crate::storage::store::Store::new(std::sync::Arc::new(
            object_store::memory::InMemory::new(),
        ));

        dispatch_batch(
            &batch,
            &HelperOptions::default(),
            &mut writer,
            Some(&store),
            None,
            "remote-helper-cancel",
            &mut cache,
            "origin",
            &mut push_state,
            OutputMode::Text,
            None,
            Some("crab://bucket/remote-helper-cancel"),
            None,
            None,
            &cancel,
        )
        .await
        .expect("dispatch push batch");

        let output = String::from_utf8(writer).expect("utf8 helper output");
        assert!(
            output.contains("error refs/heads/main internal"),
            "cancelled push must be rejected, got {output:?}"
        );
        assert!(
            output.contains("CRAB-E0090"),
            "cancelled push should preserve cancellation code, got {output:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_store_setup_fails_session_without_protocol_success() {
        use std::io::Cursor;
        use tokio::io::{AsyncReadExt, duplex};

        struct DuplexIo {
            reader: BufReader<Cursor<Vec<u8>>>,
            writer: tokio::io::DuplexStream,
        }

        impl StdIo for DuplexIo {
            type Reader = BufReader<Cursor<Vec<u8>>>;
            type Writer = tokio::io::DuplexStream;

            fn split(self) -> (Self::Reader, Self::Writer) {
                (self.reader, self.writer)
            }
        }

        let _guard = GitWorktreeGuard::new();
        let input = b"push refs/heads/main:refs/heads/main\n\n".to_vec();
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);
        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input)),
            writer: writer_tx,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let mut output = Vec::new();
        let url = test_url();

        let helper = run_remote_helper("origin", &url, io, cancel);
        let reader = writer_rx.read_to_end(&mut output);
        let (helper_result, read_result) =
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(helper, reader)
            })
            .await
            .expect("remote helper should exit after cancelled push");

        let error = helper_result.expect_err("cancelled setup must fail the helper session");
        assert!(matches!(error, CrabError::Cancelled));
        read_result.expect("read helper output");
        let output = String::from_utf8(output).expect("utf8 helper output");
        assert!(
            !output.contains("ok refs/heads/main"),
            "cancelled setup must not emit protocol success, got {output:?}"
        );
    }

    #[tokio::test]
    async fn push_dispatch_with_staged_pointer_hydrates_uploaded_content() {
        use crate::cache::LocalCache;
        use crate::cmd::hydrate::ShardHydrator;
        use crate::core::config::CacheConfig;
        use crate::metadata::manifest::Manifest;
        use crate::storage::store::Store;
        use crab_cache_store::CachingStore;
        use crab_staging::StagingAreaReadOnly;
        use object_store::memory::InMemory;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        let cache_dir = tmp.path().join("cache");
        let _cache_guard = CacheDirGuard::new(&cache_dir);

        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        run_git(&repo, &["config", "user.email", "helper-push@crab.local"]);
        run_git(&repo, &["config", "user.name", "Crab Helper Push"]);

        let content = deterministic_bytes(1_048_576);
        let pointer = stage_content(repo.join(".crab/staging"), &content).await;
        let pointer_bytes = pointer.serialize();
        std::fs::write(repo.join("large.bin"), &pointer_bytes).expect("write pointer");
        run_git(&repo, &["add", "large.bin"]);
        run_git(&repo, &["commit", "-qm", "store pointer"]);

        let git_dir = repo.join(".git");
        let _git_guard = GitEnvCwdGuard::set(&repo, &git_dir, &repo);
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "remote-helper-dispatch".to_owned());
        let staging = Arc::new(
            StagingAreaReadOnly::open(repo.join(".crab/staging"))
                .await
                .expect("open staging readonly"),
        );
        let mut config = crate::core::config::Config::default();
        config.metadb.chunk_index.local_path =
            Some(tmp.path().join("metadb-cache/chunk-index.sqlite"));
        let mut cache = SessionCache::new(config);
        let mut push_state = PushState::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut writer = Vec::new();
        let batch = Batch::Push(vec![PushItem::Spec(PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        })]);

        dispatch_batch(
            &batch,
            &HelperOptions::default(),
            &mut writer,
            Some(&store),
            Some(&staging),
            "remote-helper-dispatch",
            &mut cache,
            "origin",
            &mut push_state,
            OutputMode::Text,
            None,
            Some("crab://bucket/remote-helper-dispatch"),
            None,
            None,
            &cancel,
        )
        .await
        .expect("dispatch push batch");

        assert_eq!(
            String::from_utf8(writer).expect("utf8 helper output"),
            "ok refs/heads/main\n\n"
        );

        let (manifest_bytes, _) = store
            .get_with_etag(&router.manifest_path())
            .await
            .expect("manifest uploaded");
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).expect("manifest json");
        assert!(manifest.refs.contains_key("refs/heads/main"));
        assert!(!manifest.shard_index_hash.is_empty());
        assert!(
            !store
                .list_prefix(&router.global_path("xorbs", ""))
                .await
                .expect("list xorbs")
                .is_empty(),
            "push should upload xorbs"
        );
        assert!(
            !store
                .list_prefix(&router.global_path("shards", ""))
                .await
                .expect("list shards")
                .is_empty(),
            "push should upload shards"
        );

        let hydrate_cache = Arc::new(LocalCache::new(tmp.path().join("hydrate-cache")));
        let caching_store = CachingStore::new_with_local_cache(
            store.clone(),
            &CacheConfig::default(),
            hydrate_cache,
        )
        .expect("caching store");
        let hydrator =
            ShardHydrator::new_from_cli_layout(caching_store, router).expect("shard hydrator");
        let hydrated = hydrator
            .reconstruct_from_pointer(&pointer_bytes)
            .await
            .expect("hydrate pushed pointer");
        assert_eq!(hydrated, content);
    }

    #[tokio::test]
    async fn multi_command_session() {
        let input = "capabilities\noption progress false\nlist\n";
        let output = run(input).await;
        let expected = format!(
            "fetch\npush\noption\ncheck-connectivity\nagent=crab/{}\n\nok\n\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(output, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolved_protocol_session_pushes_lists_and_fetches_real_refs() {
        let tmp = tempfile::tempdir().expect("session tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        run_git(&repo, &["init", "-q", "--initial-branch=main"]);
        run_git(
            &repo,
            &["config", "user.email", "helper-session@crab.local"],
        );
        run_git(&repo, &["config", "user.name", "Crab Helper Session"]);
        std::fs::write(repo.join("tracked.txt"), b"resolved helper session\n")
            .expect("write tracked file");
        run_git(&repo, &["add", "tracked.txt"]);
        run_git(&repo, &["commit", "-qm", "session fixture"]);
        run_git(&repo, &["branch", "dev"]);
        run_git(&repo, &["tag", "-a", "v1", "-m", "version one"]);
        let commit = String::from_utf8(run_git(&repo, &["rev-parse", "HEAD"]))
            .expect("commit utf8")
            .trim()
            .to_owned();
        let tag_oid = String::from_utf8(run_git(&repo, &["rev-parse", "refs/tags/v1"]))
            .expect("tag utf8")
            .trim()
            .to_owned();
        let git_dir = repo.join(".git");
        let _git_guard = GitEnvCwdGuard::set(&repo, &git_dir, &repo);
        let store = crate::storage::store::Store::new(std::sync::Arc::new(
            object_store::memory::InMemory::new(),
        ));
        let prefix = "resolved-protocol-session";

        let push_context = test_context(store.clone(), prefix, tmp.path().join("push-state-push"));
        let push_input = concat!(
            "capabilities\n",
            "list for-push\n",
            "push refs/heads/main:refs/heads/main\n",
            "push refs/heads/dev:refs/heads/dev\n",
            "push refs/tags/v1:refs/tags/v1\n",
            "\n"
        );
        let (push_output, push_result) = run_with_context(
            push_input,
            push_context,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        push_result.expect("push protocol session");
        assert!(push_output.contains("ok refs/heads/main\n"));
        assert!(push_output.contains("ok refs/heads/dev\n"));
        assert!(push_output.contains("ok refs/tags/v1\n"));

        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let (manifest, _) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("pushed manifest");
        assert_eq!(manifest.refs.get("refs/heads/main"), Some(&commit));
        assert_eq!(manifest.refs.get("refs/heads/dev"), Some(&commit));
        assert_eq!(manifest.refs.get("refs/tags/v1"), Some(&tag_oid));

        run_git(&repo, &["update-ref", "-d", "refs/tags/v1"]);
        let loose_tag = git_dir
            .join("objects")
            .join(&tag_oid[..2])
            .join(&tag_oid[2..]);
        std::fs::remove_file(&loose_tag).expect("remove source tag object before fetch");

        let fetch_context = test_context(store, prefix, tmp.path().join("push-state-fetch"));
        let fetch_input = format!(
            "option followtags true\nlist\nfetch {commit} refs/heads/main\nfetch {commit} refs/heads/dev\n\n"
        );
        let (fetch_output, fetch_result) = run_with_context(
            &fetch_input,
            fetch_context,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        fetch_result.expect("fetch protocol session");
        assert!(fetch_output.contains(&format!("{commit} refs/heads/main\n")));
        assert!(fetch_output.contains(&format!("{commit} refs/heads/dev\n")));
        assert!(fetch_output.contains(&format!("{tag_oid} refs/tags/v1\n")));
        assert!(
            std::fs::read_dir(git_dir.join("objects/pack"))
                .expect("read installed pack directory")
                .filter_map(std::result::Result::ok)
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "pack")),
            "fetch batch must install the pushed pack"
        );
        assert_eq!(
            String::from_utf8(run_git(&repo, &["cat-file", "-t", &tag_oid]))
                .expect("tag type utf8")
                .trim(),
            "tag",
            "followtags fetch must install the annotated tag object from the complete pack"
        );
        assert!(
            !git_dir.join("refs/tags/v1").exists(),
            "the helper installs objects; the invoking Git client owns local ref updates"
        );
    }

    #[tokio::test]
    async fn protocol_session_store_failure_returns_error_without_success() {
        let tmp = tempfile::tempdir().expect("session tempdir");
        let store = crate::storage::store::Store::new(std::sync::Arc::new(ReadFailingStore {
            inner: object_store::memory::InMemory::new(),
        }));
        let context = test_context(store, "read-failure", tmp.path().to_path_buf());

        let (output, result) = run_with_context(
            "list\n",
            context,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            result,
            Err(CrabError::Storage(object_store::Error::NotSupported { .. }))
        ));
        assert!(
            output.is_empty(),
            "failed list must not emit empty-repo success"
        );
    }

    #[tokio::test]
    async fn eof_with_no_commands_returns_ok() {
        let output = run("").await;
        assert_eq!(output, "");
    }

    #[test]
    fn parse_fetch_command() {
        let cmd = parse_command("fetch abc123 refs/heads/main").unwrap();
        assert_eq!(
            cmd,
            HelperCommand::Fetch(FetchEntry {
                sha: "abc123".into(),
                ref_name: "refs/heads/main".into(),
            })
        );
    }

    #[test]
    fn parse_push_command_no_force() {
        let cmd = parse_command("push refs/heads/main:refs/heads/main").unwrap();
        assert_eq!(
            cmd,
            HelperCommand::Push(PushSpec {
                force: false,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            })
        );
    }

    #[test]
    fn parse_push_command_force() {
        let cmd = parse_command("push +refs/heads/main:refs/heads/main").unwrap();
        assert_eq!(
            cmd,
            HelperCommand::Push(PushSpec {
                force: true,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            })
        );
    }

    #[test]
    fn parse_option_command() {
        let cmd = parse_command("option progress true").unwrap();
        assert_eq!(
            cmd,
            HelperCommand::Option {
                key: "progress".into(),
                value: "true".into(),
            }
        );
    }

    #[test]
    fn parse_unknown_command_errors() {
        assert!(parse_command("bogus").is_err());
    }

    #[test]
    fn parse_malformed_fetch_errors() {
        assert!(parse_command("fetch onlyonearg").is_err());
    }

    #[test]
    fn parse_malformed_push_errors() {
        assert!(parse_command("push nocolon").is_err());
    }

    #[test]
    fn helper_options_defaults() {
        let opts = HelperOptions::default();
        assert!(opts.progress);
        assert_eq!(opts.verbosity, 1);
        assert!(!opts.check_connectivity);
        assert_eq!(opts.fetch_options, FetchOptions::default());
        assert!(!opts.fetch_options.has_constraints());
    }

    #[test]
    fn helper_native_push_config_does_not_treat_fetch_followtags_as_push_mode() {
        let options = HelperOptions {
            atomic: true,
            followtags: true,
            ..HelperOptions::default()
        };

        let config =
            native_push_config_for_helper(PushConfig::default(), &options, OutputMode::Text, None);

        assert!(config.progress);
        assert!(!config.emit_summary);
        assert!(config.push.atomic);
        assert!(!config.followtags);
    }

    #[test]
    fn push_spec_display() {
        let cmd = HelperCommand::Push(PushSpec {
            force: true,
            src: "refs/heads/main".into(),
            dst: "refs/heads/main".into(),
        });
        assert_eq!(cmd.to_string(), "push +refs/heads/main:refs/heads/main");
    }

    #[test]
    fn fetch_entry_display() {
        let cmd = HelperCommand::Fetch(FetchEntry {
            sha: "abc123".into(),
            ref_name: "refs/heads/main".into(),
        });
        assert_eq!(cmd.to_string(), "fetch abc123 refs/heads/main");
    }

    #[test]
    fn push_delete_refspec() {
        // Empty src means delete the remote ref
        let cmd = parse_command("push :refs/heads/old").unwrap();
        assert_eq!(
            cmd,
            HelperCommand::Push(PushSpec {
                force: false,
                src: String::new(),
                dst: "refs/heads/old".into(),
            })
        );
    }

    // --- HEAD symref parsing ---

    #[test]
    fn parse_head_symref_main() {
        let body = b"ref: refs/heads/main\n";
        let target = parse_head_symref(body).unwrap();
        assert_eq!(target, "refs/heads/main");
    }

    #[test]
    fn parse_head_symref_feature_branch() {
        let body = b"ref: refs/heads/feature/my-branch\n";
        let target = parse_head_symref(body).unwrap();
        assert_eq!(target, "refs/heads/feature/my-branch");
    }

    #[test]
    fn parse_head_symref_develop() {
        let body = b"ref: refs/heads/develop\n";
        let target = parse_head_symref(body).unwrap();
        assert_eq!(target, "refs/heads/develop");
    }

    #[test]
    fn parse_head_symref_without_trailing_newline() {
        // Tolerate missing trailing newline
        let body = b"ref: refs/heads/main";
        let target = parse_head_symref(body).unwrap();
        assert_eq!(target, "refs/heads/main");
    }

    #[test]
    fn parse_head_symref_rejects_missing_prefix() {
        let body = b"refs/heads/main\n";
        assert!(parse_head_symref(body).is_err());
    }

    #[test]
    fn parse_head_symref_rejects_bare_sha() {
        let body = b"abc123def456\n";
        assert!(parse_head_symref(body).is_err());
    }

    #[test]
    fn parse_head_symref_rejects_empty() {
        assert!(parse_head_symref(b"").is_err());
    }

    #[test]
    fn parse_head_symref_rejects_non_refs_target() {
        let body = b"ref: HEAD\n";
        assert!(parse_head_symref(body).is_err());
    }

    #[test]
    fn parse_head_symref_rejects_invalid_utf8() {
        let body: &[u8] = &[0xff, 0xfe, 0x00];
        assert!(parse_head_symref(body).is_err());
    }

    // --- ListOutput formatting ---

    #[test]
    fn format_list_output_with_head_and_refs() {
        let output = ListOutput {
            refs: vec![
                RefEntry {
                    sha: "abc123".into(),
                    ref_name: "refs/heads/main".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: "def456".into(),
                    ref_name: "refs/heads/dev".into(),
                    peeled: None,
                },
            ],
            head_symref: Some("refs/heads/main".into()),
        };
        let formatted = format_list_output(&output);
        assert_eq!(
            formatted,
            "@refs/heads/main HEAD\nabc123 refs/heads/main\ndef456 refs/heads/dev\n\n"
        );
    }

    #[test]
    fn format_list_output_empty() {
        let output = ListOutput {
            refs: Vec::new(),
            head_symref: None,
        };
        assert_eq!(format_list_output(&output), "\n");
    }

    #[test]
    fn format_list_output_refs_only_no_head() {
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: "aaa111".into(),
                ref_name: "refs/tags/v1.0".into(),
                peeled: None,
            }],
            head_symref: None,
        };
        assert_eq!(format_list_output(&output), "aaa111 refs/tags/v1.0\n\n");
    }

    #[test]
    fn format_list_output_head_symref_line_format() {
        // Verify the `@target HEAD` format specifically
        let output = ListOutput {
            refs: Vec::new(),
            head_symref: Some("refs/heads/release/v2".into()),
        };
        let formatted = format_list_output(&output);
        assert!(formatted.starts_with("@refs/heads/release/v2 HEAD\n"));
        assert!(formatted.ends_with("\n\n"));
    }

    // --- list for-push filtering ---

    #[test]
    fn filter_list_for_push_omits_head_symref() {
        let output = ListOutput {
            refs: vec![
                RefEntry {
                    sha: "abc123".into(),
                    ref_name: "refs/heads/main".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: "def456".into(),
                    ref_name: "refs/heads/dev".into(),
                    peeled: None,
                },
            ],
            head_symref: Some("refs/heads/main".into()),
        };
        let filtered = filter_list_for_push(output, true);
        assert!(filtered.head_symref.is_none());
        assert_eq!(filtered.refs.len(), 2);

        let formatted = format_list_output(&filtered);
        assert!(!formatted.contains("HEAD"));
        assert!(formatted.contains("abc123 refs/heads/main"));
        assert!(formatted.contains("def456 refs/heads/dev"));
    }

    #[test]
    fn filter_list_for_push_false_preserves_head_symref() {
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: "abc123".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            }],
            head_symref: Some("refs/heads/main".into()),
        };
        let filtered = filter_list_for_push(output, false);
        assert_eq!(filtered.head_symref.as_deref(), Some("refs/heads/main"));
        assert_eq!(filtered.refs.len(), 1);

        let formatted = format_list_output(&filtered);
        assert!(formatted.contains("@refs/heads/main HEAD"));
        assert!(formatted.contains("abc123 refs/heads/main"));
    }

    #[test]
    fn filter_list_for_push_no_head_is_noop() {
        // When there's no HEAD symref, for_push filtering is a no-op
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: "abc123".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            }],
            head_symref: None,
        };
        let filtered = filter_list_for_push(output, true);
        assert!(filtered.head_symref.is_none());
        assert_eq!(filtered.refs.len(), 1);
    }

    #[test]
    fn filter_list_for_push_omits_peeled_tag_pseudo_refs() {
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: "a".repeat(40),
                ref_name: "refs/tags/v1".into(),
                peeled: Some("b".repeat(40)),
            }],
            head_symref: None,
        };

        let filtered = filter_list_for_push(output, true);
        let formatted = format_list_output(&filtered);

        assert_eq!(formatted, format!("{} refs/tags/v1\n\n", "a".repeat(40)));
    }

    #[tokio::test]
    async fn atomic_batch_aborts_valid_refs_for_parse_time_rejection() {
        let output = run("option atomic true\n\
             push refs/heads/main:refs/heads/main\n\
             push refs/tags/v1^{}:refs/tags/v1^{}\n\n")
        .await;

        assert!(output.contains("error refs/heads/main atomic-abort"));
        assert!(output.contains("error refs/tags/v1^{} bad-refname"));
        assert!(!output.contains("ok refs/heads/main"));
    }

    // --- format_push_response ---

    #[test]
    fn format_push_response_all_ok() {
        use crate::git::push::{PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![
            PushSpec {
                force: false,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            },
            PushSpec {
                force: false,
                src: "refs/heads/dev".into(),
                dst: "refs/heads/dev".into(),
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert("refs/heads/dev".to_owned(), RefPushOutcome::Ok);
        let result = PushResult::new(outcomes);

        let response = format_push_response(&result, &specs);
        assert_eq!(response, "ok refs/heads/main\nok refs/heads/dev\n\n");
    }

    #[test]
    fn format_push_response_mixed_ok_and_error() {
        use crate::git::push::{PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![
            PushSpec {
                force: false,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            },
            PushSpec {
                force: false,
                src: "refs/heads/dev".into(),
                dst: "refs/heads/dev".into(),
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert(
            "refs/heads/dev".to_owned(),
            RefPushOutcome::Error("non-fast-forward".to_owned()),
        );
        let result = PushResult::new(outcomes);

        let response = format_push_response(&result, &specs);
        assert_eq!(
            response,
            "ok refs/heads/main\nerror refs/heads/dev non-fast-forward\n\n"
        );
    }

    #[test]
    fn format_push_response_error_reason_included() {
        use crate::git::push::{PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".into(),
            dst: "refs/heads/main".into(),
        }];
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "refs/heads/main".to_owned(),
            RefPushOutcome::Error("CAS conflict on ref".to_owned()),
        );
        let result = PushResult::new(outcomes);

        let response = format_push_response(&result, &specs);
        assert_eq!(response, "error refs/heads/main CAS conflict on ref\n\n");
    }

    #[test]
    fn format_push_response_keeps_failure_detail_on_one_protocol_line() {
        use crate::git::push::{PushRejectReason, PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".into(),
            dst: "refs/heads/main".into(),
        }];
        let mut outcomes = HashMap::new();
        outcomes.insert(
            "refs/heads/main".to_owned(),
            RefPushOutcome::Rejected(PushRejectReason::Internal(
                "git rev-list failed:\nfatal: missing blob\r\n".to_owned(),
            )),
        );

        let response = format_push_response(&PushResult::new(outcomes), &specs);

        assert_eq!(
            response,
            "error refs/heads/main internal (internal error: git rev-list failed: fatal: missing blob)\n\n"
        );
    }

    #[test]
    fn format_push_response_preserves_spec_order() {
        use crate::git::push::{PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![
            PushSpec {
                force: false,
                src: "refs/tags/v2.0".into(),
                dst: "refs/tags/v2.0".into(),
            },
            PushSpec {
                force: false,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            },
            PushSpec {
                force: true,
                src: "refs/heads/dev".into(),
                dst: "refs/heads/dev".into(),
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/tags/v2.0".to_owned(), RefPushOutcome::Ok);
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert(
            "refs/heads/dev".to_owned(),
            RefPushOutcome::Error("rejected".to_owned()),
        );
        let result = PushResult::new(outcomes);

        let response = format_push_response(&result, &specs);
        let lines: Vec<&str> = response.lines().collect();
        assert_eq!(lines[0], "ok refs/tags/v2.0");
        assert_eq!(lines[1], "ok refs/heads/main");
        assert_eq!(lines[2], "error refs/heads/dev rejected");
    }

    #[test]
    fn format_push_response_missing_outcome_treated_as_error() {
        use crate::git::push::{PushResult, RefPushOutcome};
        use std::collections::HashMap;

        let specs = vec![
            PushSpec {
                force: false,
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
            },
            PushSpec {
                force: false,
                src: "refs/heads/orphan".into(),
                dst: "refs/heads/orphan".into(),
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        // refs/heads/orphan intentionally missing from outcomes
        let result = PushResult::new(outcomes);

        let response = format_push_response(&result, &specs);
        assert!(response.contains("ok refs/heads/main"));
        assert!(response.contains("error refs/heads/orphan missing outcome"));
    }

    #[test]
    fn format_push_response_empty_batch() {
        use crate::git::push::PushResult;

        let specs: Vec<PushSpec> = Vec::new();
        let result = PushResult::empty();

        let response = format_push_response(&result, &specs);
        assert_eq!(response, "\n");
    }

    #[test]
    fn push_result_event_includes_active_active_commit_metadata() {
        use crate::git::push::{PushCommitMetadata, PushResult, RefPushOutcome};
        use crab_coordination::write_coordinator::PushTransactionState;
        use std::collections::HashMap;

        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".into(),
            dst: "refs/heads/main".into(),
        };
        let mut outcomes = HashMap::new();
        outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
        let result = PushResult::new(outcomes).with_active_active_commit(PushCommitMetadata {
            operation_id: "op-456".into(),
            coordinator_epoch: 9,
            writer: "west".into(),
            writer_region: "us-west-2".into(),
            manifest_generation: 3,
            commit_state: PushTransactionState::Materialized,
        });

        let payload = build_push_result_event(&result, &[spec]);

        assert_eq!(payload.operation_id.as_deref(), Some("op-456"));
        assert_eq!(payload.coordinator_epoch, Some(9));
        assert_eq!(payload.writer_region.as_deref(), Some("us-west-2"));
        assert_eq!(payload.commit_state.as_deref(), Some("materialized"));
        assert_eq!(payload.refs[0].status, "ok");
    }

    // --- FetchOptions and FilterSpec ---

    #[test]
    fn fetch_options_default_has_no_constraints() {
        let opts = FetchOptions::default();
        assert!(opts.depth.is_none());
        assert!(opts.filter.is_none());
        assert!(!opts.has_constraints());
    }

    #[test]
    fn fetch_options_depth_only_has_constraints() {
        let opts = FetchOptions {
            depth: Some(3),
            deepen_relative: false,
            filter: None,
        };
        assert!(opts.has_constraints());
    }

    #[test]
    fn fetch_options_filter_only_has_constraints() {
        let opts = FetchOptions {
            depth: None,
            deepen_relative: false,
            filter: Some(FilterSpec::BlobNone),
        };
        assert!(opts.has_constraints());
    }

    #[test]
    fn fetch_options_combined_depth_and_filter() {
        let opts = FetchOptions {
            depth: Some(5),
            deepen_relative: false,
            filter: Some(FilterSpec::BlobNone),
        };
        assert!(opts.has_constraints());
    }

    #[test]
    fn filter_spec_display() {
        assert_eq!(FilterSpec::BlobNone.to_string(), "blob:none");
    }

    // --- option depth / filter parsing ---

    #[tokio::test]
    async fn option_depth_accepted() {
        let output = run("option depth 3\n").await;
        assert_eq!(output, "ok\n");
    }

    #[tokio::test]
    async fn option_filter_blob_none_is_unsupported() {
        let output = run("option filter blob:none\n").await;
        assert_eq!(output, "unsupported\n");
    }

    #[tokio::test]
    async fn option_filter_unsupported_spec() {
        let output = run("option filter tree:0\n").await;
        assert_eq!(output, "unsupported\n");
    }

    #[tokio::test]
    async fn option_check_connectivity_controls_acknowledgement() {
        let mut options = HelperOptions::default();
        let mut writer = Vec::new();

        handle_option("check-connectivity", "true", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(options.check_connectivity);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_check_connectivity_rejects_invalid_boolean() {
        let output = run("option check-connectivity maybe\n").await;
        assert!(output.starts_with("error invalid check-connectivity value:"));
    }

    #[tokio::test]
    async fn option_depth_sets_fetch_options() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("depth", "5", &mut options, &mut writer)
            .await
            .unwrap();
        assert_eq!(options.fetch_options.depth, Some(5));
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_deepen_relative_sets_fetch_options() {
        let mut options = HelperOptions::default();
        let mut writer = Vec::new();

        handle_option("deepen-relative", "true", &mut options, &mut writer)
            .await
            .unwrap();

        assert!(options.fetch_options.deepen_relative);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_deepen_relative_rejects_invalid_boolean() {
        let output = run("option deepen-relative maybe\n").await;
        assert!(output.starts_with("error invalid deepen-relative value:"));
    }

    #[tokio::test]
    async fn unsupported_shallow_selectors_fail_instead_of_degrading_to_full_fetch() {
        for (key, value) in [
            ("deepen-since", "1700000000"),
            ("deepen-not", "refs/heads/archive"),
        ] {
            let mut options = HelperOptions::default();
            let mut output = Vec::new();
            let result = handle_option(key, value, &mut options, &mut output).await;

            assert!(matches!(result, Err(CrabError::Protocol(_))));
            assert!(
                String::from_utf8(output).unwrap().starts_with("error "),
                "option {key} did not emit an explicit protocol error"
            );
        }
    }

    #[tokio::test]
    async fn option_update_shallow_accepts_booleans_and_rejects_other_values() {
        assert_eq!(run("option update-shallow true\n").await, "ok\n");
        assert_eq!(run("option update-shallow false\n").await, "ok\n");
        assert!(
            run("option update-shallow maybe\n")
                .await
                .starts_with("error invalid update-shallow value:")
        );
    }

    #[tokio::test]
    async fn option_depth_invalid_value_errors() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        let result = handle_option("depth", "abc", &mut options, &mut writer).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn option_filter_does_not_set_fetch_options() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("filter", "blob:none", &mut options, &mut writer)
            .await
            .unwrap();
        assert_eq!(options.fetch_options.filter, None);
        assert_eq!(String::from_utf8(writer).unwrap(), "unsupported\n");
    }

    #[tokio::test]
    async fn unsupported_filter_does_not_change_depth_constraint() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("depth", "2", &mut options, &mut writer)
            .await
            .unwrap();
        handle_option("filter", "blob:none", &mut options, &mut writer)
            .await
            .unwrap();
        assert_eq!(options.fetch_options.depth, Some(2));
        assert_eq!(options.fetch_options.filter, None);
        assert!(options.fetch_options.has_constraints());
    }

    // --- option atomic parsing ---

    #[tokio::test]
    async fn option_atomic_true_sets_flag_and_replies_ok() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("atomic", "true", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(options.atomic);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_atomic_false_clears_flag_and_replies_ok() {
        let mut options = HelperOptions::default();
        options.atomic = true;
        let mut writer: Vec<u8> = Vec::new();
        handle_option("atomic", "false", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(!options.atomic);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_atomic_invalid_value_replies_error() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        // Invalid bool values must not silently answer `unsupported`
        // — git only ever sends true/false, so anything else is a
        // protocol violation the client deserves to see.
        handle_option("atomic", "maybe", &mut options, &mut writer)
            .await
            .unwrap();
        let reply = String::from_utf8(writer).unwrap();
        assert!(
            reply.starts_with("error "),
            "expected `error ...` reply for invalid atomic value; got {reply:?}"
        );
        assert!(!options.atomic, "atomic flag must stay at its default");
    }

    #[tokio::test]
    async fn helper_options_default_atomic_is_false() {
        let opts = HelperOptions::default();
        assert!(!opts.atomic);
    }

    // --- option followtags parsing ---

    #[tokio::test]
    async fn option_followtags_true_sets_flag_and_replies_ok() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("followtags", "true", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(options.followtags);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_followtags_false_clears_flag_and_replies_ok() {
        let mut options = HelperOptions::default();
        options.followtags = true;
        let mut writer: Vec<u8> = Vec::new();
        handle_option("followtags", "false", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(!options.followtags);
        assert_eq!(String::from_utf8(writer).unwrap(), "ok\n");
    }

    #[tokio::test]
    async fn option_followtags_invalid_value_replies_error() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        // Invalid bool values are a protocol violation — git only ever
        // sends `true`/`false`, so the helper must surface `error`
        // rather than silently answering `unsupported`.
        handle_option("followtags", "sometimes", &mut options, &mut writer)
            .await
            .unwrap();
        let reply = String::from_utf8(writer).unwrap();
        assert!(
            reply.starts_with("error "),
            "expected `error ...` reply for invalid followtags value; got {reply:?}"
        );
        assert!(
            !options.followtags,
            "followtags flag must stay at its default"
        );
    }

    #[tokio::test]
    async fn helper_options_default_followtags_is_false() {
        let opts = HelperOptions::default();
        assert!(!opts.followtags);
    }

    // --- unsupported include-tag parsing ---

    #[tokio::test]
    async fn option_include_tag_is_unsupported() {
        let mut options = HelperOptions::default();
        let mut writer: Vec<u8> = Vec::new();
        handle_option("include-tag", "true", &mut options, &mut writer)
            .await
            .unwrap();
        assert!(!options.include_tag);
        assert_eq!(String::from_utf8(writer).unwrap(), "unsupported\n");
    }

    #[tokio::test]
    async fn helper_options_default_include_tag_is_false() {
        let opts = HelperOptions::default();
        assert!(!opts.include_tag);
    }

    // --- read_remote_refs from manifest ---

    #[derive(Debug)]
    struct ReadFailingStore {
        inner: object_store::memory::InMemory,
    }

    impl std::fmt::Display for ReadFailingStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("ReadFailingStore")
        }
    }

    #[async_trait::async_trait]
    impl object_store::ObjectStore for ReadFailingStore {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            options: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            _location: &object_store::path::Path,
            _options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            Err(object_store::Error::NotSupported {
                source: Box::<dyn std::error::Error + Send + Sync>::from(
                    "injected manifest read failure",
                ),
            })
        }

        fn delete_stream(
            &self,
            locations: futures_util::stream::BoxStream<
                'static,
                object_store::Result<object_store::path::Path>,
            >,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
        {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn list_advertisement_treats_missing_manifest_as_empty_remote() {
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());

        let output = read_remote_refs_for_advertisement(&store, &router, &[])
            .await
            .unwrap();

        assert!(output.refs.is_empty());
        assert!(output.head_symref.is_none());
    }

    #[tokio::test]
    async fn list_advertisement_rejects_corrupt_manifest() {
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        store
            .put_overwrite(&router.manifest_path(), Bytes::from_static(b"not-json"))
            .await
            .unwrap();

        let error = read_remote_refs_for_advertisement(&store, &router, &[])
            .await
            .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn list_advertisement_preserves_non_not_found_store_error() {
        use crate::storage::store::Store;
        use std::sync::Arc;

        let store = Store::new(Arc::new(ReadFailingStore {
            inner: object_store::memory::InMemory::new(),
        }));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());

        let error = read_remote_refs_for_advertisement(&store, &router, &[])
            .await
            .expect_err("non-not-found storage errors must fail advertisement");

        match error {
            CrabError::Storage(object_store::Error::NotSupported { source }) => {
                assert_eq!(source.to_string(), "injected manifest read failure");
            }
            other => panic!("expected original storage error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_returns_correct_refs_from_manifest() {
        use crate::metadata::manifest::{Manifest, create_manifest};
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/repo".to_string());

        let mut refs = BTreeMap::new();
        refs.insert(
            "refs/heads/main".to_owned(),
            "aaaa1111bbbb2222cccc3333dddd4444eeee5555".to_owned(),
        );
        refs.insert(
            "refs/heads/dev".to_owned(),
            "5555eeee4444dddd3333cccc2222bbbb1111aaaa".to_owned(),
        );
        refs.insert(
            "refs/tags/v1.0".to_owned(),
            "1234567890abcdef1234567890abcdef12345678".to_owned(),
        );

        let mut manifest = Manifest {
            version: 2,
            generation: 3,
            created_at: "2025-07-01T00:00:00Z".to_owned(),
            pusher: Some("alice".to_owned()),
            session_id: "session-test".to_owned(),
            refs,
            peeled_refs: BTreeMap::new(),
            head: "refs/heads/main".to_owned(),
            shard_index_hash: String::new(),
            pack_index_hash: String::new(),
            git_validation_digest: String::new(),
            commit_graph_hash: None,
            ref_registry_hash: None,
        };
        manifest.seal_git_validation();

        create_manifest(&store, &router, &manifest).await.unwrap();

        let output = read_remote_refs_for_advertisement(&store, &router, &[])
            .await
            .unwrap();

        // HEAD symref should come from the manifest.
        assert_eq!(output.head_symref.as_deref(), Some("refs/heads/main"));

        // All three refs should be present.
        assert_eq!(output.refs.len(), 3);

        let ref_map: std::collections::HashMap<&str, &str> = output
            .refs
            .iter()
            .map(|r| (r.ref_name.as_str(), r.sha.as_str()))
            .collect();
        assert_eq!(
            ref_map["refs/heads/main"],
            "aaaa1111bbbb2222cccc3333dddd4444eeee5555"
        );
        assert_eq!(
            ref_map["refs/heads/dev"],
            "5555eeee4444dddd3333cccc2222bbbb1111aaaa"
        );
        assert_eq!(
            ref_map["refs/tags/v1.0"],
            "1234567890abcdef1234567890abcdef12345678"
        );
    }

    // --- format_capabilities ---

    #[test]
    fn capabilities_without_commit_graph() {
        let caps = format_capabilities(false);
        let expected = format!(
            "fetch\npush\noption\ncheck-connectivity\nagent=crab/{}\n\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(caps, expected);
        assert!(!caps.contains("shallow"));
        assert!(!caps.contains("filter"));
    }

    #[test]
    fn capabilities_with_commit_graph() {
        let caps = format_capabilities(true);
        let expected = format!(
            "fetch\npush\noption\ncheck-connectivity\nshallow\nagent=crab/{}\n\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(caps, expected);
        assert!(!caps.contains("filter"));
    }

    // --- validate_fetch_entries_with_manifest ---

    /// Build a minimal manifest carrying just the supplied refs. All
    /// other manifest fields are either empty strings or `None` —
    /// the upload-pack validator only reads `refs`.
    fn manifest_with_refs(pairs: &[(&str, &str)]) -> crate::metadata::manifest::Manifest {
        use std::collections::BTreeMap;
        let mut refs = BTreeMap::new();
        for (name, sha) in pairs {
            refs.insert((*name).to_owned(), (*sha).to_owned());
        }
        let mut manifest = crate::metadata::manifest::Manifest {
            version: 2,
            generation: 1,
            created_at: String::new(),
            pusher: None,
            session_id: String::new(),
            refs,
            peeled_refs: BTreeMap::new(),
            head: "refs/heads/main".to_owned(),
            shard_index_hash: String::new(),
            pack_index_hash: String::new(),
            git_validation_digest: String::new(),
            commit_graph_hash: None,
            ref_registry_hash: None,
        };
        manifest.seal_git_validation();
        manifest
    }

    fn base_config() -> crate::core::config::Config {
        crate::core::config::Config::default()
    }

    #[test]
    fn fetch_tip_sha_accepted_when_tip_policy_in_effect() {
        let tip = "a".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let entries = vec![FetchEntry {
            sha: tip.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let cfg = base_config();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        assert!(
            result[0].1.is_ok(),
            "tip SHA must be accepted under default policy"
        );
    }

    #[test]
    fn fetch_non_tip_sha_rejected_not_at_tip_when_only_tip_allowed() {
        use crate::git::reject_reason::FetchRejectReason;

        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let entries = vec![FetchEntry {
            sha: non_tip.clone(),
            ref_name: "refs/heads/feature".into(),
        }];
        let mut cfg = base_config();
        cfg.uploadpack_allow_any_sha_in_want = false;
        cfg.uploadpack_allow_tip_sha_in_want = true;
        cfg.uploadpack_allow_reachable_sha_in_want = false;

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        match &result[0].1 {
            Err(FetchRejectReason::NotAtTip { sha }) => assert_eq!(sha, &non_tip),
            other => panic!("expected NotAtTip rejection, got {other:?}"),
        }
    }

    #[test]
    fn fetch_allow_any_sha_in_want_bypasses_validation() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let entries = vec![FetchEntry {
            sha: non_tip.clone(),
            ref_name: "refs/heads/hidden".into(),
        }];
        let mut cfg = base_config();
        cfg.uploadpack_allow_any_sha_in_want = true;
        // `allow_tip` / `allow_reachable` values are irrelevant under
        // `allow_any` — assert both plausible settings pass to lock
        // in the precedence ordering.
        cfg.uploadpack_allow_tip_sha_in_want = false;
        cfg.uploadpack_allow_reachable_sha_in_want = false;

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        assert!(
            result[0].1.is_ok(),
            "allow_any_sha_in_want must accept non-tip SHA"
        );
    }

    #[test]
    fn fetch_reachable_sha_accepted_when_reachable_allowed() {
        use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};

        // A → B (tip). `B` is the tip; `A` is reachable via summary.
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![
                CommitEntry {
                    oid: sha_a.clone(),
                    gen_number: 0,
                    parents: vec![],
                },
                CommitEntry {
                    oid: sha_b.clone(),
                    gen_number: 1,
                    parents: vec![sha_a.clone()],
                },
            ],
        };
        let manifest = manifest_with_refs(&[("refs/heads/main", &sha_b)]);
        let entries = vec![FetchEntry {
            sha: sha_a.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let mut cfg = base_config();
        cfg.uploadpack_allow_any_sha_in_want = false;
        cfg.uploadpack_allow_tip_sha_in_want = true;
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        assert_eq!(result.len(), 1);
        assert!(
            result[0].1.is_ok(),
            "reachable ancestor must be accepted under allow_reachable"
        );
    }

    #[test]
    fn fetch_unreachable_sha_rejected_when_reachable_allowed() {
        use crate::git::reject_reason::FetchRejectReason;
        use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};

        let sha_tip = "a".repeat(40);
        let sha_unknown = "f".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![CommitEntry {
                oid: sha_tip.clone(),
                gen_number: 0,
                parents: vec![],
            }],
        };
        let manifest = manifest_with_refs(&[("refs/heads/main", &sha_tip)]);
        let entries = vec![FetchEntry {
            sha: sha_unknown.clone(),
            ref_name: "refs/heads/foo".into(),
        }];
        let mut cfg = base_config();
        cfg.uploadpack_allow_any_sha_in_want = false;
        cfg.uploadpack_allow_tip_sha_in_want = true;
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        match &result[0].1 {
            Err(FetchRejectReason::NotReachable { sha }) => assert_eq!(sha, &sha_unknown),
            other => panic!("expected NotReachable rejection, got {other:?}"),
        }
    }

    #[test]
    fn fetch_mixed_batch_preserves_per_entry_outcomes() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &tip)]);
        let entries = vec![
            FetchEntry {
                sha: tip.clone(),
                ref_name: "refs/heads/main".into(),
            },
            FetchEntry {
                sha: non_tip.clone(),
                ref_name: "refs/heads/hidden".into(),
            },
        ];
        let cfg = base_config();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 2);
        assert!(result[0].1.is_ok(), "first entry (tip) must be accepted");
        assert!(
            result[1].1.is_err(),
            "second entry (non-tip) must be rejected"
        );
    }

    #[test]
    fn repo_root_from_absolute_git_dir_returns_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        let root = repo_root_from_git_dir(&git_dir);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn helper_fetch_seeds_lazy_checkout_when_config_missing() {
        let tmp = tempfile::tempdir().unwrap();

        ensure_lazy_checkout_config_for_new_helper_repo(tmp.path());

        let config = std::fs::read_to_string(tmp.path().join(".crab/config.toml")).unwrap();
        assert!(
            config.contains("[checkout]"),
            "config should contain checkout table: {config}"
        );
        assert!(
            config.contains("lazy = true"),
            "helper-created config should default plain clones to lazy checkout: {config}"
        );
    }

    #[test]
    fn helper_fetch_preserves_existing_checkout_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join(".crab/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[checkout]\nlazy = false\n").unwrap();

        ensure_lazy_checkout_config_for_new_helper_repo(tmp.path());

        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(
            config.contains("lazy = false"),
            "helper must not overwrite explicit checkout policy: {config}"
        );
    }

    #[test]
    fn repo_root_from_relative_git_dir_does_not_return_empty() {
        // Simulate the remote-helper environment: cwd is the repo work
        // tree, and git passes `GIT_DIR=.git`. The old `git_dir.parent()`
        // path returned an empty `PathBuf`, which poisoned every
        // downstream `current_dir(...)` call with ENOENT.
        let _lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        let saved_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(tmp.path()).unwrap();

        let root = repo_root_from_git_dir(std::path::Path::new(".git"));

        // Restore cwd before any assertion can panic the test runner.
        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert!(
            !root.as_os_str().is_empty(),
            "relative .git must not collapse to an empty path"
        );
        assert!(
            root.is_absolute() || root == std::path::PathBuf::from("."),
            "expected absolute path or `.`, got {root:?}"
        );
    }

    #[test]
    fn push_state_repo_root_uses_repo_root_from_nested_cwd() {
        let _git_env = CleanGitEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .expect("run git init");
        assert!(status.success());

        let nested = tmp.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let saved_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&nested).unwrap();
        let root = push_state_repo_root().canonicalize().unwrap();
        let expected = tmp.path().canonicalize().unwrap();

        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(root, expected);
    }

    #[test]
    fn push_staging_root_uses_shared_staging_from_nested_cwd() {
        let _git_env = CleanGitEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .expect("run git init");
        assert!(status.success());

        let nested = tmp.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let saved_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&nested).unwrap();
        let root = push_staging_root().unwrap();
        let expected = tmp.path().canonicalize().unwrap().join(".crab/staging");

        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(root, expected);
    }

    #[test]
    fn repo_root_from_nonexistent_relative_git_dir_falls_back_to_cwd() {
        let _lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let saved_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(tmp.path()).unwrap();

        let root = repo_root_from_git_dir(std::path::Path::new(".git"));

        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert!(
            !root.as_os_str().is_empty(),
            "fallback must produce a non-empty path"
        );
    }

    #[test]
    fn repo_root_from_linked_worktree_private_git_dir_returns_linked_root() {
        let git_env = CleanGitEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");

        let init = std::process::Command::new("git")
            .args(["init"])
            .arg(&repo)
            .output();
        let Ok(init) = init else {
            eprintln!("SKIP: git unavailable");
            return;
        };
        if !init.status.success() {
            eprintln!("SKIP: git init failed");
            return;
        }
        for args in [
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test User"].as_slice(),
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("git config");
            assert!(status.success());
        }
        std::fs::write(repo.join("README.md"), b"init\n").expect("readme");
        let add = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo)
            .status()
            .expect("git add");
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&repo)
            .status()
            .expect("git commit");
        if !commit.success() {
            eprintln!("SKIP: git commit failed");
            return;
        }
        let add_worktree = std::process::Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&linked)
            .arg("HEAD")
            .current_dir(&repo)
            .status()
            .expect("git worktree add");
        if !add_worktree.success() {
            eprintln!("SKIP: git worktree add failed");
            return;
        }

        let git_file = std::fs::read_to_string(linked.join(".git")).expect("linked .git");
        let gitdir = git_file
            .trim()
            .strip_prefix("gitdir: ")
            .expect("gitdir prefix");
        let admin_dir = {
            let path = std::path::PathBuf::from(gitdir);
            if path.is_absolute() {
                path
            } else {
                linked.join(path)
            }
        };
        let admin_dir = admin_dir.canonicalize().expect("admin dir");
        let linked = linked.canonicalize().expect("linked root");

        drop(git_env);
        let _env = GitEnvCwdGuard::set(&linked, &admin_dir, &linked);
        let root = repo_root_from_git_dir(&admin_dir);

        assert_eq!(root.canonicalize().unwrap(), linked);
    }
}
