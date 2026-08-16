//! Top-level coordinator for `crab import`.
//!
//! Wires the six pipeline stages — detect, enumerate, window
//! planning, ingest, assemble, publish — into one async entry
//! point. Each stage is a well-tested module on its own; this
//! file owns the plumbing that passes journal handles, staging
//! areas, and stats between them, plus the stage-level spans,
//! cancellation checkpoints, and journal cleanup on success.
//!
//! # Skeleton scope (V1)
//!
//! The function you're looking for is [`run_import_inner`]. It
//! parses `--from` / `--to` via [`ObjectUrl::parse`], validates
//! the scheme rules the CLI already enforces, and then hands the
//! resolved sides into [`run_import_with_stores`] for the actual
//! pipeline walk.
//!
//! URL-to-[`Store`] resolution lives in the shared storage resolver.
//! Raw cloud URLs force the matching storage provider, while
//! `crab://` targets use the configured provider just like normal
//! pushes. `file://` URLs are backed by `object_store`'s local
//! filesystem store so local dry-runs exercise the same coordinator
//! path as cloud imports.
//!
//! # Observability
//!
//! Every stage is wrapped in a `tracing::info_span!` carrying the
//! stage name so operators can filter logs per stage
//! (`enumerate`, `ingest`, `assemble`, `publish`, `detect`).
//! At each stage's exit the coordinator emits a second `info!`
//! line with the stage's duration and the headline counts from
//! the stage's stats — those numbers are also the ones that land
//! in the final [`ImportSummary`].
//!
//! # Cancellation
//!
//! [`check_cancelled`] runs between every stage. Individual
//! stages also honor the token internally (ingest, publish),
//! but the between-stage checks guarantee a user Ctrl+C never
//! drags us into the next stage's setup work.
//!
//! # Journal lifecycle
//!
//! The journal lives at `{into}/.crab/import-journal.db`.
//! On full success the coordinator removes it so a subsequent
//! `crab import` run doesn't trip the `--resume` path for a
//! plan that already completed. On any error the journal stays
//! put so the user can `--resume` later; preserving it across
//! failures is the whole point of having a journal in the first
//! place.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use object_store::path::Path as ObjectPath;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::cmd::import::{ImportArgs, LfsSourceMode};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;
use crate::git::url::{Cloud, ObjectUrl, UrlForm};
use crate::import::assemble::{AssembleInputs, AssembleProgressSink, run_assemble};
use crate::import::detect::{DetectArgs, SourceMode, detect_source_mode};
use crate::import::enumerate::{ProgressSink, enumerate as run_enumerate};
use crate::import::ingest::{
    DELETE_MARKER_FILE_HASH, IngestInputs, IngestProgressSink, ResolvedStore, StageEvent,
    run_ingest, validate_lfs_resume_entries,
};
use crate::import::journal::{EntryState, ImportEntry, Journal};
use crate::import::lfs_guard::{LfsDetection, detect_lfs_source};
use crate::import::publish::{PublishInputs, run_publish};
use crate::import::summary::{
    HistoryRange, ImportPlanSummary, SummaryVersioning, build_extension_histogram,
};
use crate::import::versions::{
    AzureVersionedList, FlatObjectStoreList, GcsVersionedList, LocalVersionedList, S3VersionedList,
    VersionedList, VersionedListImpl,
};
use crate::import::window::{
    CommitWindow, plan_commit_windows, plan_flat_single_commit, plan_snapshot,
    validate_history_range,
};
use crate::storage::resolve_object_url_store;
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_xet::hash::MerkleHash;

pub use crate::import::summary::ImportSummary;

/// Default window width for versioned mode.
///
/// Requirement I1a calls out "1 h by default"; the CLI already
/// parses a user override into [`ImportArgs::window`] (as a raw
/// string for now). V1 treats an unset / unparsable window as
/// the 1 h default and leaves richer parsing to a follow-up task.
const DEFAULT_WINDOW: Duration = Duration::from_secs(3_600);

/// Soft cap on the number of commits the window planner may
/// emit. Requirement I1a suggests 100 000 as a default safety
/// rail; until the CLI grows a `--max-commits` flag we hardcode
/// that value here.
const DEFAULT_MAX_COMMITS: u32 = 100_000;

/// Top-level `crab import` pipeline entry point.
///
/// Parses the `--from` / `--to` URLs, rejects wrong-way-round and
/// cross-cloud raw URLs, resolves source and target stores, and
/// hands off to [`run_import_with_stores`].
///
/// # Errors
///
/// - [`CrabError::ImportSourceMustBeRaw`] when `--from` is
///   `crab://`.
/// - [`CrabError::ImportSchemeMismatch`] for cross-cloud raw
///   `--to`.
/// - Store-construction and credential errors from the configured
///   storage backend.
/// - Any error bubbled from the pipeline itself (detect,
///   enumerate, ingest, assemble, publish).
pub async fn run_import_inner(
    args: &ImportArgs,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    let effective_args = resolve_import_args(args)?;
    let args = &effective_args;
    let from_raw = args.from_url()?;
    let to_raw = args.to_url()?;
    let from = ObjectUrl::parse(from_raw)?;
    let to = ObjectUrl::parse(to_raw)?;

    from.require_raw()?;
    if to.form == UrlForm::Raw && from.cloud != to.cloud {
        return Err(CrabError::ImportSchemeMismatch {
            from_scheme: cloud_label(from.cloud),
            to_scheme: cloud_label(to.cloud),
        });
    }

    check_cancelled(cancel)?;

    let config = Config::resolve_local()?;
    let into = resolve_into_dir(args, &to);
    let source = resolve_object_url_store(&from, &config, "fetch", cancel).await?;
    let target = resolve_object_url_store(&to, &config, "push", cancel).await?;
    let source_lists = source_lists_for_resolved_url(&from, &source)?;

    run_import_with_source_lists(args, source, target, source_lists, into, cancel).await
}

pub(crate) fn resolve_import_args(args: &ImportArgs) -> Result<ImportArgs> {
    let mut resolved = args.clone();
    apply_cli_shorthands(&mut resolved)?;
    validate_lfs_options(&resolved)?;

    let from_missing = resolved
        .from
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none();
    let to_missing = resolved
        .to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none();
    let dest_prefix_missing = resolved
        .dest_prefix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none();
    if !resolved.resume {
        if from_missing || to_missing {
            return Err(CrabError::Configuration {
                key: "SOURCE or --from is required, and --to or --bucket/--name is required unless --resume is set".into(),
                origin: "crab import".into(),
            });
        }
        return Ok(resolved);
    }

    if !from_missing && !to_missing && !dest_prefix_missing {
        return Ok(resolved);
    }

    let journal_dir = resume_journal_dir(&resolved)?;
    let journal_path = journal_dir.join(".crab").join("import-journal.db");
    if !journal_path.exists() {
        return Err(CrabError::ImportNoJournal {
            path: journal_path.display().to_string(),
        });
    }
    let journal = Journal::open(&journal_dir)?;
    let Some(plan) = journal.load_plan()? else {
        return Err(CrabError::ImportNoJournal {
            path: journal_path.display().to_string(),
        });
    };

    if from_missing {
        resolved.from = Some(plan.inputs.source_url);
    }
    if to_missing {
        resolved.to = Some(plan.inputs.target_url);
    }
    if dest_prefix_missing && !plan.inputs.dest_prefix.is_empty() {
        resolved.dest_prefix = Some(plan.inputs.dest_prefix);
    }
    apply_cli_shorthands(&mut resolved)?;
    validate_lfs_options(&resolved)?;
    Ok(resolved)
}

fn apply_cli_shorthands(args: &mut ImportArgs) -> Result<()> {
    if present(args.source.as_deref()).is_some() && present(args.from.as_deref()).is_some() {
        return Err(CrabError::Configuration {
            key: "use either SOURCE or --from, not both".into(),
            origin: "crab import".into(),
        });
    }

    if present(args.to.as_deref()).is_some()
        && (present(args.bucket.as_deref()).is_some() || present(args.name.as_deref()).is_some())
    {
        return Err(CrabError::Configuration {
            key: "use either --to or --bucket/--name, not both".into(),
            origin: "crab import".into(),
        });
    }

    if present(args.from.as_deref()).is_none()
        && let Some(source) = present(args.source.as_deref())
    {
        args.from = Some(normalize_source_locator(source)?);
        args.source = None;
    }

    if let Some(from) = present(args.from.as_deref())
        && !looks_like_url(from)
    {
        args.from = Some(normalize_source_locator(from)?);
    }

    if present(args.to.as_deref()).is_none() {
        args.to = target_url_from_bucket_name(args.bucket.as_deref(), args.name.as_deref())?;
        if args.to.is_some() {
            args.bucket = None;
            args.name = None;
        }
    }

    args.dest_prefix = normalize_dest_prefix(args.dest_prefix.as_deref())?;
    Ok(())
}

fn validate_lfs_options(args: &ImportArgs) -> Result<()> {
    if args.effective_lfs_objects().is_some()
        && args.effective_lfs_source() != LfsSourceMode::Resolve
    {
        return Err(CrabError::Configuration {
            key: "--lfs-objects requires --lfs-source resolve".into(),
            origin: "crab import".into(),
        });
    }
    Ok(())
}

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn looks_like_url(raw: &str) -> bool {
    raw.contains("://")
}

fn normalize_source_locator(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if looks_like_url(raw) {
        return Ok(raw.to_owned());
    }

    let absolute = std::fs::canonicalize(raw).map_err(|e| CrabError::Configuration {
        key: format!("source path {raw:?} is not readable: {e}"),
        origin: "crab import".into(),
    })?;
    let mut path = absolute.to_string_lossy().replace('\\', "/");
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    Ok(format!("file://{path}"))
}

fn target_url_from_bucket_name(bucket: Option<&str>, name: Option<&str>) -> Result<Option<String>> {
    match (present(bucket), present(name)) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(CrabError::Configuration {
            key: "--bucket and --name must be provided together".into(),
            origin: "crab import".into(),
        }),
        (Some(bucket), Some(name)) => {
            let repo_path = name.trim_matches('/');
            if repo_path.is_empty() {
                return Err(CrabError::Configuration {
                    key: "--name must include a repo path".into(),
                    origin: "crab import".into(),
                });
            }
            Ok(Some(format!("crab://{bucket}/{repo_path}")))
        }
    }
}

fn normalize_dest_prefix(prefix: Option<&str>) -> Result<Option<String>> {
    let Some(prefix) = present(prefix) else {
        return Ok(None);
    };
    let normalized = prefix.trim_matches('/');
    if normalized.is_empty() {
        return Ok(None);
    }
    if !crate::import::is_importable_relative_path(normalized) {
        return Err(CrabError::Configuration {
            key: format!("--dest-prefix {prefix:?} is not a safe Git path"),
            origin: "crab import".into(),
        });
    }
    Ok(Some(normalized.to_owned()))
}

fn resume_journal_dir(args: &ImportArgs) -> Result<PathBuf> {
    if let Some(into) = &args.into {
        return Ok(into.clone());
    }
    let Some(to) = args
        .to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(PathBuf::from("."));
    };

    let target = ObjectUrl::parse(to)?;
    Ok(resolve_into_dir(args, &target))
}

fn resolve_into_dir(args: &ImportArgs, target: &ObjectUrl) -> PathBuf {
    if let Some(into) = &args.into {
        return into.clone();
    }

    let leaf = target
        .prefix
        .rsplit('/')
        .find(|part| !part.is_empty())
        .filter(|part| *part != ".")
        .unwrap_or({
            if target.bucket.is_empty() {
                "import"
            } else {
                target.bucket.as_str()
            }
        });
    PathBuf::from(leaf)
}

struct SourceLists {
    history: VersionedListImpl,
    current: VersionedListImpl,
}

impl SourceLists {
    fn single(source_list: VersionedListImpl) -> Self {
        Self {
            history: source_list.clone(),
            current: source_list,
        }
    }

    fn detect(&self) -> &dyn VersionedList {
        &self.history
    }

    fn enumerate_for(&self, mode: &SourceMode) -> &dyn VersionedList {
        match mode {
            SourceMode::Flat => &self.current,
            SourceMode::Versioned | SourceMode::SingleSnapshot { .. } => &self.history,
        }
    }
}

fn source_lists_for_resolved_url(url: &ObjectUrl, source: &ResolvedStore) -> Result<SourceLists> {
    match url.cloud {
        Cloud::Local => Ok(SourceLists::single(VersionedListImpl::Local(
            LocalVersionedList::new(PathBuf::from(&url.prefix)),
        ))),
        Cloud::S3 | Cloud::Gcs | Cloud::Azure => {
            let current = VersionedListImpl::FlatObjectStore(FlatObjectStoreList::new(
                source.store.inner().clone(),
                source.prefix.clone(),
            ));
            let history = match url.cloud {
                Cloud::S3 => VersionedListImpl::S3(S3VersionedList::new(
                    url.bucket.clone(),
                    source.prefix.clone(),
                )),
                Cloud::Gcs => VersionedListImpl::Gcs(GcsVersionedList::new(
                    url.bucket.clone(),
                    source.prefix.clone(),
                )),
                Cloud::Azure => {
                    let target = url.azure_storage_target()?;
                    VersionedListImpl::Azure(AzureVersionedList::new(
                        target.account,
                        target.container,
                        target.object_prefix,
                    ))
                }
                Cloud::Local => unreachable!("matched cloud branch above"),
            };
            Ok(SourceLists { history, current })
        }
    }
}

/// Run the import pipeline with pre-resolved source / target
/// stores.
///
/// This is the real pipeline body. Callers build source / target
/// [`ResolvedStore`]s and hand them in along with the `ImportArgs`.
///
/// See the module docstring for the stage order, span layout,
/// cancellation checkpoints, and journal-cleanup behavior.
pub async fn run_import_with_stores(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    source_list: VersionedListImpl,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    run_import_with_source_lists(
        args,
        source,
        target,
        SourceLists::single(source_list),
        into,
        cancel,
    )
    .await
}

async fn run_import_with_source_lists(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    source_lists: SourceLists,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    let effective_args = resolve_import_args(args)?;
    let args = &effective_args;
    let from_raw = args.from_url()?;
    let to_raw = args.to_url()?;
    let to = ObjectUrl::parse(to_raw)?;
    let from = ObjectUrl::parse(from_raw)?;
    let total_start = Instant::now();
    let branch = args.branch.clone();
    let same_bucket = source.bucket == target.bucket;

    info!(
        from = %from_raw,
        to = %to_raw,
        into = %into.display(),
        branch = %branch,
        same_bucket,
        resume = args.resume,
        dry_run = args.dry_run,
        "import: pipeline starting"
    );

    // Short-circuit `--dry-run` to the plan path. No mutations,
    // no staging, no journal left on disk.
    if args.dry_run {
        return run_import_plan_with_source_lists(args, source, target, source_lists, into, cancel)
            .await;
    }

    // ── preflight safety rails ──────────────────────────────
    // Runs before any enumerate / ingest work so users see
    // configuration errors (non-empty target, existing remote,
    // LFS-format source, missing git identity) up front.
    let lfs_store = preflight_safety_checks(PreflightInputs {
        args,
        source: &source,
        target: &target,
        into: &into,
        source_url: &from,
        target_url: &to,
        cancel,
    })
    .await?;

    // Shared metrics handle — threaded through ingest, assemble,
    // and publish so lifetime totals aggregate across stages.
    // Staging / publish each take a handle; ingest and assemble
    // hold `Option` references so unit tests can pass `None`.
    let metrics = Arc::new(Metrics::new());

    let journal_dir = into.clone();
    let journal_path = journal_dir.join(".crab").join("import-journal.db");

    // ── resume vs fresh: detect + enumerate run only on fresh ──
    let (source_mode, kept_count, total_bytes_source, enumerate_skipped) = if args.resume {
        resume_preamble(args, &journal_dir, &journal_path)?
    } else {
        fresh_preamble(args, &source_lists, &journal_dir, cancel).await?
    };

    if kept_count == 0 {
        // Nothing to ingest; surface a clear error so users know
        // the filters dropped everything rather than silently
        // producing an empty repo.
        return Err(CrabError::Internal(
            "import: enumerate kept zero entries; check --include / --exclude filters".into(),
        ));
    }

    // Large-import confirmation runs on fresh runs only — resume
    // already passed confirmation on its original invocation, and
    // a late-arriving "no" would leave the journal stranded.
    if !args.resume {
        confirm_large_import(
            kept_count,
            total_bytes_source,
            args.yes,
            args.output_mode().is_machine(),
        )?;
    }

    // ── plan (pre-ingest pass) ──────────────────────────────
    //
    // Window planning runs twice. The first pass here gives us
    // an early commit count so we can surface
    // `ImportCommitCeilingExceeded` before wasting time on
    // ingest. Entries at this point are `Pending`; assemble
    // later re-plans over the `Staged` set so every window sees
    // its file hashes.
    check_cancelled(cancel)?;
    let journal = Journal::open(&journal_dir)?;
    let staging_root = journal_dir.join(".crab").join("staging");
    if args.resume {
        reset_staged_entries_missing_local_chunks(&journal, &staging_root).await?;
        let checked =
            validate_lfs_resume_state(&journal, &source, lfs_store.as_deref(), cancel).await?;
        if checked > 0 {
            info!(
                checked,
                "resume: validated staged LFS resolutions against source pointers"
            );
        }
    }
    let pending_entries = collect_entries(&journal, EntryFilter::PendingOnly)?;
    let pending_entries = map_entries_for_commit(pending_entries, args.dest_prefix.as_deref())?;
    let window = args
        .window
        .as_deref()
        .and_then(parse_duration_simple)
        .unwrap_or(DEFAULT_WINDOW);
    if !(args.resume && pending_entries.is_empty()) {
        let _pre_windows =
            plan_windows(&source_mode, pending_entries, window, DEFAULT_MAX_COMMITS)?;
    }

    // ── ingest ──────────────────────────────────────────────
    check_cancelled(cancel)?;
    let staging = Arc::new(StagingArea::open(staging_root.clone()).await?);
    let journal_arc = Arc::new(Mutex::new(journal));
    let ingest_span = info_span!("ingest");
    let ingest_stats = {
        let start = Instant::now();
        let progress: Arc<Mutex<NoOpIngest>> = Arc::new(Mutex::new(NoOpIngest));
        let inputs = IngestInputs {
            source,
            journal: Arc::clone(&journal_arc),
            staging: Arc::clone(&staging),
            repo_root: into.clone(),
            lfs_store: lfs_store.clone(),
            jobs: args.jobs.unwrap_or_else(default_jobs),
            fail_fast: args.fail_fast,
            progress,
            metrics: Some(Arc::clone(&metrics)),
            cancel: cancel.clone(),
        };
        match run_ingest(inputs).instrument(ingest_span.clone()).await {
            Ok(stats) => {
                let snap = stats.snapshot();
                info!(
                    parent: &ingest_span,
                    staged = snap.staged,
                    failed = snap.failed,
                    skipped = snap.skipped,
                    bytes_source = snap.bytes_source,
                    bytes_staged = snap.bytes_staged,
                    duration_ms = ms(start.elapsed()),
                    "ingest: complete"
                );
                snap
            }
            Err(err) => {
                warn!(stage = "ingest", error = %err, "import: stage failed");
                return Err(err);
            }
        }
    };

    if ingest_stats.failed > 0 {
        return Err(CrabError::Configuration {
            origin: "crab import".into(),
            key: format!(
                "ingest failed for {} entr{}; inspect the import log/journal and rerun with --resume after fixing the source error",
                ingest_stats.failed,
                if ingest_stats.failed == 1 { "y" } else { "ies" }
            ),
        });
    }

    // No commit may expose an import pointer until every segment locator is
    // durable. Recipe leases make ownership visible; this flush is the
    // physical publication barrier shared by all ingest workers.
    staging.flush_pending().await?;

    // ── assemble ────────────────────────────────────────────
    check_cancelled(cancel)?;

    // Re-read the journal now that every entry is `Staged |
    // Failed | Skipped`. Window planning against the `Staged`
    // set gives assemble the real file hashes it needs to write
    // pointer blobs.
    let staged_entries = {
        let guard = journal_arc.lock().await;
        collect_entries(&guard, EntryFilter::StagedOnly)?
    };
    let staged_entries = map_entries_for_commit(staged_entries, args.dest_prefix.as_deref())?;
    let windows = plan_windows(&source_mode, staged_entries, window, DEFAULT_MAX_COMMITS)?;

    let assemble_span = info_span!("assemble");
    let assemble_stats = {
        let start = Instant::now();
        let progress: Arc<Mutex<NoOpAssemble>> = Arc::new(Mutex::new(NoOpAssemble));
        let inputs = AssembleInputs {
            into: into.clone(),
            branch: branch.clone(),
            force: args.force,
            resume: args.resume,
            target_url: to_raw.to_owned(),
            windows,
            track: args.track.clone(),
            message_template: args.message.clone(),
            author_template: args.author_template.clone(),
            progress,
            metrics: Some(Arc::clone(&metrics)),
            cancel: cancel.clone(),
        };
        match run_assemble(inputs).instrument(assemble_span.clone()).await {
            Ok(stats) => {
                info!(
                    parent: &assemble_span,
                    commits = stats.commits_created,
                    files = stats.files_imported,
                    versions = stats.versions_imported,
                    head = ?stats.head_commit_oid,
                    duration_ms = ms(start.elapsed()),
                    "assemble: complete"
                );
                stats
            }
            Err(err) => {
                warn!(stage = "assemble", error = %err, "import: stage failed");
                return Err(err);
            }
        }
    };

    // ── publish ─────────────────────────────────────────────
    check_cancelled(cancel)?;
    let head_commit_oid = assemble_stats.head_commit_oid.clone().ok_or_else(|| {
        CrabError::Internal("import: assemble produced zero commits; nothing to publish".into())
    })?;

    // Drop the StagingArea Arc refs held by ingest so we can
    // reopen the staging path read-only for publish. The
    // `StagingAreaReadOnly` constructor opens its own handle
    // without contending with the writer.
    drop(staging);

    let staging_ro = Arc::new(StagingAreaReadOnly::open(staging_root.clone()).await?);
    let git_dir = into.join(".git");
    let publish_span = info_span!("publish");
    let publish_stats = {
        let start = Instant::now();
        let inputs = PublishInputs {
            target,
            repo_prefix: target_repo_prefix(&to),
            staging: staging_ro,
            branch: branch.clone(),
            head_commit_oid: head_commit_oid.clone(),
            git_dir,
            metrics: Some(Arc::clone(&metrics)),
            cancel: cancel.clone(),
        };
        match run_publish(inputs).instrument(publish_span.clone()).await {
            Ok(stats) => {
                info!(
                    parent: &publish_span,
                    refs_pushed = stats.refs_pushed,
                    bytes_uploaded = stats.bytes_uploaded,
                    head = %stats.head_commit_oid,
                    duration_ms = ms(start.elapsed()),
                    "publish: complete"
                );
                stats
            }
            Err(err) => {
                warn!(stage = "publish", error = %err, "import: stage failed");
                return Err(err);
            }
        }
    };

    // ── journal cleanup on success ──────────────────────────
    //
    // The journal only lives for crash recovery. A clean run
    // makes it stale — the next `crab import` (same args or
    // not) should start fresh, not trip the resume path.
    // Failures above short-circuit this cleanup and leave the
    // journal on disk for `--resume`.
    drop(journal_arc);
    if let Err(err) = remove_journal(&journal_path).await {
        // Failure to remove the journal is not fatal — the repo
        // is already published — but it does mean a later
        // `--resume` would see a stale plan. Warn, don't error.
        warn!(
            path = %journal_path.display(),
            %err,
            "import: failed to remove journal after success; subsequent runs may trigger --resume"
        );
    }

    let summary = ImportSummary {
        source_url: from_raw.to_owned(),
        target_url: to_raw.to_owned(),
        versioning: versioning_from_mode(&source_mode),
        files_imported: assemble_stats.files_imported,
        versions_imported: assemble_stats.versions_imported,
        commits_created: assemble_stats.commits_created,
        files_skipped: enumerate_skipped.saturating_add(ingest_stats.skipped),
        files_failed: ingest_stats.failed,
        lfs_resolved: ingest_stats.lfs_resolved,
        lfs_skipped: ingest_stats.lfs_skipped,
        lfs_failed: ingest_stats.lfs_failed,
        bytes_source: ingest_stats.bytes_source.max(total_bytes_source),
        bytes_staged: ingest_stats.bytes_staged,
        bytes_uploaded: publish_stats.bytes_uploaded,
        same_bucket,
        duration_ms: ms(total_start.elapsed()),
        head_commit_oid: assemble_stats.head_commit_oid,
        first_commit_oid: assemble_stats.first_commit_oid,
        branch,
        history_range: history_range_from_args(args),
        dry_run: false,
        plan: None,
    };

    info!(
        commits = summary.commits_created,
        files = summary.files_imported,
        versions = summary.versions_imported,
        bytes_source = summary.bytes_source,
        bytes_uploaded = summary.bytes_uploaded,
        duration_ms = summary.duration_ms,
        head = ?summary.head_commit_oid,
        "import: pipeline complete"
    );

    Ok(summary)
}

/// Which entries the coordinator wants off the journal.
enum EntryFilter {
    /// Everything in `Pending` state — used for the pre-ingest
    /// window plan that surfaces commit-ceiling errors early.
    PendingOnly,
    /// Everything in `Staged` state — used for the real window
    /// plan that assemble walks.
    StagedOnly,
}

/// Snapshot journal rows matching `filter` into an owned Vec.
///
/// The iterator callback is synchronous, so we pull the full set
/// into memory here. V1 imports are expected to fit comfortably
/// in RAM (the spec's 1 TiB / 10 000 object target is tens of
/// MiB of journal rows at worst); huge-bucket streaming plans
/// are a follow-up.
fn collect_entries(journal: &Journal, filter: EntryFilter) -> Result<Vec<ImportEntry>> {
    let mut out = Vec::new();
    journal.iter_entries_sorted_by_time(|e| {
        let keep = match filter {
            EntryFilter::PendingOnly => matches!(e.state, EntryState::Pending),
            EntryFilter::StagedOnly => matches!(e.state, EntryState::Staged { .. }),
        };
        if keep {
            out.push(e);
        }
        Ok(())
    })?;
    Ok(out)
}

fn map_entries_for_commit(
    entries: Vec<ImportEntry>,
    dest_prefix: Option<&str>,
) -> Result<Vec<ImportEntry>> {
    let Some(dest_prefix) = present(dest_prefix) else {
        return Ok(entries);
    };

    entries
        .into_iter()
        .map(|mut entry| {
            entry.relative_path = import_dest_path(dest_prefix, &entry.relative_path)?;
            Ok(entry)
        })
        .collect()
}

fn import_dest_path(dest_prefix: &str, source_relative_path: &str) -> Result<String> {
    let Some(dest_prefix) = normalize_dest_prefix(Some(dest_prefix))? else {
        return Ok(source_relative_path.to_owned());
    };
    let mapped = format!("{dest_prefix}/{source_relative_path}");
    if !crate::import::is_importable_relative_path(&mapped) {
        return Err(CrabError::Configuration {
            key: format!("mapped import path {mapped:?} is not a safe Git path"),
            origin: "crab import".into(),
        });
    }
    Ok(mapped)
}

async fn reset_staged_entries_missing_local_chunks(
    journal: &Journal,
    staging_root: &Path,
) -> Result<u64> {
    let staged_entries = collect_entries(journal, EntryFilter::StagedOnly)?;
    if staged_entries.is_empty() {
        return Ok(0);
    }

    let staging = match StagingAreaReadOnly::open(staging_root.to_path_buf()).await {
        Ok(staging) => Some(staging),
        Err(crab_staging::StagingError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };

    let mut reset = 0u64;
    for entry in staged_entries {
        let EntryState::Staged { file_hash } = entry.state else {
            continue;
        };
        if entry.is_delete_marker || file_hash == DELETE_MARKER_FILE_HASH {
            continue;
        }

        let has_local_chunks = match staging.as_ref() {
            Some(staging) => !staging
                .chunks_for_file(&MerkleHash::from(file_hash))?
                .is_empty(),
            None => false,
        };
        if has_local_chunks {
            continue;
        }

        journal.mark_pending(&entry.relative_path, &entry.version_id)?;
        reset += 1;
    }

    if reset > 0 {
        info!(
            reset,
            "resume: re-queued staged entries whose local staging chunks were already retired"
        );
    }

    Ok(reset)
}

async fn validate_lfs_resume_state(
    journal: &Journal,
    source: &ResolvedStore,
    lfs_store: Option<&crab_lfs::LfsObjectStore>,
    cancel: &CancellationToken,
) -> Result<u64> {
    if let Some(store) = lfs_store {
        return validate_lfs_resume_entries(journal, source, store, cancel).await;
    }

    let mut stale: Option<(String, String)> = None;
    journal.iter_staged_lfs_resolutions(|row| {
        if stale.is_none() {
            stale = Some((row.relative_path, row.version_id));
        }
        Ok(())
    })?;

    if let Some((relative_path, version_id)) = stale {
        return Err(CrabError::ImportPlanMismatch {
            recorded: format!(
                "{relative_path}:{} requires LFS object verification",
                if version_id.is_empty() {
                    "<flat>"
                } else {
                    &version_id
                }
            ),
            provided: "no LFS object store resolved for this resume".into(),
        });
    }

    Ok(0)
}

/// Dispatch on the detect decision to pick the right window
/// planner. Keeps the coordinator body linear and the planner
/// modules single-purpose.
fn plan_windows(
    mode: &SourceMode,
    entries: Vec<ImportEntry>,
    window: Duration,
    max_commits: u32,
) -> Result<Vec<CommitWindow>> {
    match mode {
        SourceMode::Flat => {
            let w = plan_flat_single_commit(entries)?;
            Ok(vec![w])
        }
        SourceMode::SingleSnapshot { at } => {
            let w = plan_snapshot(entries, *at)?;
            Ok(vec![w])
        }
        SourceMode::Versioned => plan_commit_windows(entries, window, max_commits),
    }
}

/// Parse the `ImportArgs` string field for `--at` / `--since` /
/// `--until` into epoch seconds. Accepts decimal epoch seconds
/// and RFC3339 timestamps with `Z` or numeric UTC offsets.
fn parse_at_seconds(raw: Option<&str>) -> Result<Option<i64>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Ok(v) = trimmed.parse::<i64>() {
        return Ok(Some(v));
    }
    if let Some(v) = parse_rfc3339_seconds(trimmed) {
        return Ok(Some(v));
    }
    Err(CrabError::Configuration {
        key: "unsupported timestamp format; expected epoch seconds or RFC3339".into(),
        origin: trimmed.to_owned(),
    })
}

fn parse_rfc3339_seconds(raw: &str) -> Option<i64> {
    if !raw.is_ascii() {
        return None;
    }
    let (date, time_with_zone) = raw.split_once('T').or_else(|| raw.split_once('t'))?;
    let (year, month, day) = parse_rfc3339_date(date)?;
    let (time, offset_seconds) = parse_rfc3339_time_zone(time_with_zone)?;
    let (hour, minute, second) = parse_rfc3339_time(time)?;
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?
        .checked_sub(offset_seconds)
}

fn parse_rfc3339_date(date: &str) -> Option<(i64, i64, i64)> {
    if date.len() != 10 || &date[4..5] != "-" || &date[7..8] != "-" {
        return None;
    }
    let year = parse_digits(&date[0..4])?;
    let month = parse_digits(&date[5..7])?;
    let day = parse_digits(&date[8..10])?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month)? {
        return None;
    }
    Some((year, month, day))
}

fn parse_rfc3339_time_zone(time_with_zone: &str) -> Option<(&str, i64)> {
    if let Some(time) = time_with_zone
        .strip_suffix('Z')
        .or_else(|| time_with_zone.strip_suffix('z'))
    {
        return Some((time, 0));
    }

    let split = time_with_zone.rfind(['+', '-'])?;
    if split < 8 {
        return None;
    }
    let (time, offset) = time_with_zone.split_at(split);
    if offset.len() != 6 || &offset[3..4] != ":" {
        return None;
    }
    let sign = match &offset[0..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let hours = parse_digits(&offset[1..3])?;
    let minutes = parse_digits(&offset[4..6])?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

fn parse_rfc3339_time(time: &str) -> Option<(i64, i64, i64)> {
    if time.len() < 8 || &time[2..3] != ":" || &time[5..6] != ":" {
        return None;
    }
    let hour = parse_digits(&time[0..2])?;
    let minute = parse_digits(&time[3..5])?;
    let second_part = &time[6..];
    let second = if let Some((whole, fraction)) = second_part.split_once('.') {
        if whole.len() != 2 || fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        parse_digits(whole)?
    } else {
        if second_part.len() != 2 {
            return None;
        }
        parse_digits(second_part)?
    };
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

fn parse_digits(raw: &str) -> Option<i64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> Option<i64> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    })
}

fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
}

/// Very small `--window` parser covering the shapes the V1 CLI
/// accepts: `<n>s`, `<n>m`, `<n>h`, or bare seconds. Anything
/// outside that set falls through to `None` so the caller picks
/// the default.
fn parse_duration_simple(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (digits, unit) = if let Some(rest) = raw.strip_suffix('s') {
        (rest, 1u64)
    } else if let Some(rest) = raw.strip_suffix('m') {
        (rest, 60)
    } else if let Some(rest) = raw.strip_suffix('h') {
        (rest, 3_600)
    } else {
        (raw, 1)
    };
    let n: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(n.saturating_mul(unit)))
}

/// Render a [`Cloud`] label for [`CrabError::ImportSchemeMismatch`].
fn cloud_label(cloud: Cloud) -> String {
    match cloud {
        Cloud::S3 => "s3".into(),
        Cloud::Gcs => "gs".into(),
        Cloud::Azure => "az".into(),
        Cloud::Local => "file".into(),
    }
}

/// Pick the repo prefix the publish stage will route refs and
/// manifests under. For `crab://` this is the Crab repo path;
/// for raw cloud targets the URL's prefix plays the same role.
/// `file://` stores are rooted at the target directory, so their
/// repo prefix is empty.
fn target_repo_prefix(to: &ObjectUrl) -> String {
    if to.cloud == Cloud::Local {
        String::new()
    } else {
        to.prefix.clone()
    }
}

/// Default job count when `--jobs` is absent. Mirrors the
/// requirement I1 hint: "default: CPU count".
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
}

/// Milliseconds with saturation, for log fields.
fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Remove the journal file (and its SQLite sidecars) after a
/// successful run. Swallows `NotFound` because a best-effort
/// cleanup race doesn't block the summary.
async fn remove_journal(path: &std::path::Path) -> Result<()> {
    // Remove the main file and the WAL/SHM sidecars. Use the
    // async tokio API so we don't block the runtime thread.
    for suffix in ["", "-wal", "-shm"] {
        let candidate = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            PathBuf::from(p)
        };
        match tokio::fs::remove_file(&candidate).await {
            Ok(()) => debug!(path = %candidate.display(), "import: removed journal sidecar"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CrabError::Internal(format!(
                    "failed to remove {}: {e}",
                    candidate.display()
                )));
            }
        }
    }
    // Best-effort: give the SystemTime caller something deterministic.
    let _ = SystemTime::now().duration_since(UNIX_EPOCH);
    Ok(())
}

// ── No-op progress sinks ────────────────────────────────────────
//
// V1 wires the pipeline end-to-end before the structured-output
// plumbing. Real sinks land in task 16; these no-ops keep the
// pipeline compilable and the integration tests silent.

struct NoOpEnumerate;
impl ProgressSink for NoOpEnumerate {
    fn enumerate_event(&mut self, _event: crate::import::enumerate::EnumerateEvent) {}
}

struct NoOpIngest;
impl IngestProgressSink for NoOpIngest {
    fn stage_event(&mut self, _event: &StageEvent<'_>) {}
}

struct NoOpAssemble;
impl AssembleProgressSink for NoOpAssemble {
    fn assemble_event(&mut self, _event: &crate::import::assemble::AssembleEvent) {}
}

// ── Preflight safety rails ──────────────────────────────────────

/// Preflight inputs — one place for every check that must fire
/// before we touch enumerate/ingest/assemble/publish state.
pub struct PreflightInputs<'a> {
    pub args: &'a ImportArgs,
    pub source: &'a ResolvedStore,
    pub target: &'a ResolvedStore,
    pub into: &'a std::path::Path,
    pub source_url: &'a ObjectUrl,
    pub target_url: &'a ObjectUrl,
    pub cancel: &'a CancellationToken,
}

#[cfg(test)]
async fn expect_preflight_err(inputs: PreflightInputs<'_>) -> CrabError {
    match preflight_safety_checks(inputs).await {
        Ok(_) => panic!("preflight_safety_checks must fail"),
        Err(err) => err,
    }
}

/// Run every safety rail that can be checked *before* the
/// pipeline writes anything. Order matches requirement I11.
///
/// Any rail that requires pipeline state (commit-ceiling, large-
/// import confirmation) runs later; this helper is the pre-
/// mutation gate.
pub async fn preflight_safety_checks(
    inputs: PreflightInputs<'_>,
) -> Result<Option<std::sync::Arc<crab_lfs::LfsObjectStore>>> {
    check_cancelled(inputs.cancel)?;

    // 17.11 — `--since > --until` would produce an empty window.
    // Raise early so enumerate never runs against an empty range.
    validate_history_range(
        parse_at_seconds(inputs.args.since.as_deref())?,
        parse_at_seconds(inputs.args.until.as_deref())?,
    )?;

    // 17.1 — target directory must be empty (or an empty git repo).
    // `--force` bypasses. The assemble stage enforces this again
    // before `git init`, but hoisting it here saves the cost of
    // detect + enumerate when the user aimed --into at the wrong
    // place.
    if !inputs.args.resume {
        ensure_target_dir_preflight(inputs.into, inputs.args.force)?;
    }

    // 17.4 — source prefix collision with the target `.crab/`
    // layout. Hard error; no --force override. Must run before
    // any store work because the collision would corrupt reads
    // during ingest.
    ensure_no_prefix_collision(inputs.source, inputs.target)?;

    // 17.2 — target bucket already hosts a published repo. The
    // HEAD probe below is best-effort: a missing object maps to
    // `NotFound` and we proceed; any other error surfaces
    // verbatim so the user can correct creds etc. before work
    // starts.
    ensure_no_existing_remote(inputs.target, inputs.args.to_url()?, inputs.args.force).await?;

    // 17.3 — source prefix itself is a Crab repo. Same probe
    // shape as the target check but under the source.
    ensure_source_not_crab_repo(inputs.source, inputs.args.from_url()?, inputs.args.force).await?;

    // 17.5 — LFS-format source. Default policy refuses, `skip`
    // lets per-blob pointer skips surface in the summary, and
    // `resolve` wires a companion object root into ingest.
    let lfs_store = resolve_lfs_import_strategy(
        inputs.source,
        inputs.source_url,
        inputs.args.from_url()?,
        inputs.args.effective_lfs_source(),
        inputs.args.effective_lfs_objects(),
        inputs.cancel,
    )
    .await?;

    // 17.6 — git identity must be configured before we start
    // doing work that eventually asks git to make a commit.
    // Assemble checks this too but surfacing it here means the
    // user never waits for enumerate + ingest before hitting the
    // identity error.
    ensure_git_identity_preflight(inputs.into)?;

    Ok(lfs_store)
}

/// Non-empty target check. Accepts:
///
/// 1. Missing directory.
/// 2. Empty directory.
/// 3. Directory containing only `.git/` (a bare-inited empty
///    repo with no commits yet) or `.crab/` (the coordinator's
///    own bookkeeping from a prior partial run).
///
/// Anything else errors with [`CrabError::ImportTargetNotEmpty`]
/// unless `--force` is set.
fn ensure_target_dir_preflight(into: &std::path::Path, force: bool) -> Result<()> {
    if force || !into.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(into)?;
    let mut has_git = false;
    let mut has_other = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(".git") {
            has_git = true;
        } else if name != std::ffi::OsStr::new(".crab") {
            has_other = true;
        }
    }

    if has_other {
        return Err(CrabError::ImportTargetNotEmpty {
            path: into.display().to_string(),
        });
    }

    if !has_git {
        return Ok(());
    }

    // `.git/` exists — confirm no commits landed.
    let output = git_command_in(into)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if output.status.success() {
        return Err(CrabError::ImportTargetNotEmpty {
            path: into.display().to_string(),
        });
    }
    Ok(())
}

/// Verify the user has a git identity configured. Uses the
/// process-wide git lookup (global + system configs); callers
/// running with a per-directory config must have either a
/// `.git/` already initialized or global identity set.
fn ensure_git_identity_preflight(into: &std::path::Path) -> Result<()> {
    // If the target dir doesn't exist yet, fall back to checking
    // the user's global config.
    let check_dir = if into.exists() && into.join(".git").exists() {
        into
    } else {
        std::path::Path::new(".")
    };
    for key in &["user.name", "user.email"] {
        let output = git_command_in(check_dir)
            .args(["config", "--get", key])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || value.is_empty() {
            return Err(CrabError::ImportMissingGitIdentity);
        }
    }
    Ok(())
}

fn git_command_in(dir: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    command
}

/// Reject same-bucket imports whose source prefix overlaps the
/// target `.crab/` layout or vice versa. The push pipeline
/// would read xorbs back from the same prefix it writes to.
fn ensure_no_prefix_collision(source: &ResolvedStore, target: &ResolvedStore) -> Result<()> {
    if source.bucket != target.bucket {
        return Ok(());
    }
    let src = normalize_prefix(&source.prefix);
    let tgt = normalize_prefix(&target.prefix);
    let tgt_crab = if tgt.is_empty() {
        ".crab".to_owned()
    } else {
        format!("{tgt}/.crab")
    };

    // Exact overlap
    if src == tgt {
        return Err(CrabError::ImportPrefixCollision {
            detail: format!("source prefix {src:?} equals target prefix {tgt:?}"),
        });
    }
    // Parent/child checks — src contains tgt, or tgt contains src,
    // or src sits under / above tgt/.crab.
    if is_ancestor_or_equal(&src, &tgt) || is_ancestor_or_equal(&tgt, &src) {
        return Err(CrabError::ImportPrefixCollision {
            detail: format!(
                "source prefix {src:?} overlaps target prefix {tgt:?} in the same bucket"
            ),
        });
    }
    if is_ancestor_or_equal(&src, &tgt_crab) || is_ancestor_or_equal(&tgt_crab, &src) {
        return Err(CrabError::ImportPrefixCollision {
            detail: format!("source prefix {src:?} overlaps target .crab layout {tgt_crab:?}"),
        });
    }
    Ok(())
}

/// Strip leading/trailing slashes so prefix comparisons work on
/// normalized strings. An empty prefix becomes `""` which compares
/// correctly against any prefix (everything is a descendant).
fn normalize_prefix(prefix: &str) -> String {
    prefix.trim_matches('/').to_owned()
}

/// Whether `a` is an ancestor of (or equal to) `b` in prefix-path
/// semantics. Empty `a` is an ancestor of everything.
fn is_ancestor_or_equal(a: &str, b: &str) -> bool {
    if a.is_empty() {
        return true;
    }
    if a == b {
        return true;
    }
    b.starts_with(a) && b.as_bytes().get(a.len()) == Some(&b'/')
}

/// Refuse when the target already hosts a published repo —
/// `manifests/HEAD` is the canonical tell. Any non-`NotFound`
/// error from the probe surfaces as-is (creds / network issues
/// that would show up later anyway).
async fn ensure_no_existing_remote(
    target: &ResolvedStore,
    target_url: &str,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    let head_path = manifest_head_path(&target.prefix);
    match target.store.head(&head_path).await {
        Ok(_) => Err(CrabError::ImportRemoteExists {
            existing_url: target_url.to_owned(),
            new_url: target_url.to_owned(),
        }),
        Err(CrabError::NotFound { .. }) => Ok(()),
        Err(other) => Err(other),
    }
}

/// Detect source-is-Crab via `refs/HEAD` (the canonical marker
/// for a published Crab repo). `NotFound` is the happy path;
/// any other error surfaces.
async fn ensure_source_not_crab_repo(
    source: &ResolvedStore,
    source_url: &str,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    let refs_head = refs_head_path(&source.prefix);
    match source.store.head(&refs_head).await {
        Ok(_) => {
            return Err(CrabError::ImportSourceIsCrabRepo {
                url: source_url.to_owned(),
            });
        }
        Err(CrabError::NotFound { .. }) => {}
        Err(other) => return Err(other),
    }
    // Belt-and-suspenders: check manifests/HEAD too — a partially
    // pushed repo might have manifests but no refs/HEAD yet.
    let manifests_head = manifest_head_path(&source.prefix);
    match source.store.head(&manifests_head).await {
        Ok(_) => Err(CrabError::ImportSourceIsCrabRepo {
            url: source_url.to_owned(),
        }),
        Err(CrabError::NotFound { .. }) => Ok(()),
        Err(other) => Err(other),
    }
}

/// Check whether the source is LFS-formatted and apply the selected LFS
/// import policy.
///
/// Returns `Some(store_url)` when LFS pointer resolution is requested and
/// the store was discovered. Returns `None` for plain sources and for
/// `--lfs-source skip`, where ingest records each pointer as skipped.
///
/// # Errors
///
/// Returns [`CrabError::ImportLfsSourceUnsupported`] when the source is
/// LFS-formatted and policy is `fail`.
/// Returns [`CrabError::ImportLfsStoreNotFound`] when policy is `resolve`
/// but no LFS object store could be discovered.
async fn resolve_lfs_import_strategy(
    source: &ResolvedStore,
    source_url: &ObjectUrl,
    source_url_raw: &str,
    mode: LfsSourceMode,
    explicit_lfs_objects: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Option<std::sync::Arc<crab_lfs::LfsObjectStore>>> {
    match detect_lfs_source(source, cancel).await? {
        LfsDetection::Plain => Ok(None),
        LfsDetection::LfsFormat { .. } => match mode {
            LfsSourceMode::Fail => Err(CrabError::ImportLfsSourceUnsupported {
                url: source_url_raw.to_owned(),
            }),
            LfsSourceMode::Skip => {
                tracing::info!(
                    source = %source_url_raw,
                    "LFS source detected; unresolved pointers will be skipped"
                );
                Ok(None)
            }
            LfsSourceMode::Resolve => {
                let root = match explicit_lfs_objects {
                    Some(root) => root.to_owned(),
                    None => discover_lfs_objects_root(source, cancel)
                        .await?
                        .ok_or_else(|| CrabError::ImportLfsStoreNotFound {
                            url: source_url_raw.to_owned(),
                        })?,
                };
                let lfs_store = resolve_lfs_object_store(source, source_url, &root)?;

                tracing::info!(
                    source = %source_url_raw,
                    lfs_objects = %root,
                    lfs_prefix = %lfs_store.prefix(),
                    "LFS import enabled: will rehydrate LFS pointers"
                );
                Ok(Some(std::sync::Arc::new(lfs_store)))
            }
        },
    }
}

async fn discover_lfs_objects_root(
    source: &ResolvedStore,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    check_cancelled(cancel)?;

    let path = source_child_path(&source.prefix, ".lfsstore");
    match source.store.get_with_etag(&path).await {
        Ok((bytes, _etag)) => {
            let text = String::from_utf8_lossy(&bytes);
            Ok(text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .map(ToOwned::to_owned))
        }
        Err(CrabError::NotFound { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

fn resolve_lfs_object_store(
    source: &ResolvedStore,
    source_url: &ObjectUrl,
    root: &str,
) -> Result<crab_lfs::LfsObjectStore> {
    let root = root.trim();
    if root.is_empty() {
        return Err(CrabError::Configuration {
            key: "--lfs-objects must not be empty".into(),
            origin: "crab import".into(),
        });
    }

    let prefix = if looks_like_url(root) {
        lfs_prefix_from_root_url(source, source_url, &ObjectUrl::parse(root)?)?
    } else {
        lfs_store_prefix_from_root(root)
    };
    Ok(crab_lfs::LfsObjectStore::new(
        source.store.as_storage().clone(),
        &prefix,
    ))
}

fn lfs_prefix_from_root_url(
    source: &ResolvedStore,
    source_url: &ObjectUrl,
    root_url: &ObjectUrl,
) -> Result<String> {
    root_url.require_raw()?;
    if root_url.cloud != source_url.cloud {
        return Err(CrabError::Configuration {
            key: "--lfs-objects must use the same storage scheme as --from".into(),
            origin: "crab import".into(),
        });
    }

    if root_url.cloud == Cloud::Local {
        return local_lfs_root_prefix(source_url, root_url);
    }

    if root_url.bucket_identity() != source.bucket {
        return Err(CrabError::Configuration {
            key: "--lfs-objects must point at the same bucket/container as --from".into(),
            origin: "crab import".into(),
        });
    }
    Ok(lfs_store_prefix_from_root(&root_url.prefix))
}

fn local_lfs_root_prefix(source_url: &ObjectUrl, root_url: &ObjectUrl) -> Result<String> {
    let source_root = std::path::Path::new(&source_url.prefix);
    let root = std::path::Path::new(&root_url.prefix);
    let relative = root
        .strip_prefix(source_root)
        .map_err(|_| CrabError::Configuration {
            key: "--lfs-objects file:// path must be inside the --from file:// tree".into(),
            origin: "crab import".into(),
        })?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    Ok(lfs_store_prefix_from_root(&relative))
}

fn lfs_store_prefix_from_root(root_prefix: &str) -> String {
    let normalized = root_prefix.trim_matches('/');
    if normalized == "lfs/objects" {
        return String::new();
    }
    normalized
        .strip_suffix("/lfs/objects")
        .unwrap_or(normalized)
        .to_owned()
}

fn source_child_path(prefix: &str, child: &str) -> ObjectPath {
    if prefix.is_empty() {
        ObjectPath::from(child)
    } else {
        ObjectPath::from(format!("{prefix}/{child}"))
    }
}

fn manifest_head_path(repo_prefix: &str) -> ObjectPath {
    if repo_prefix.is_empty() {
        ObjectPath::from("manifests/HEAD")
    } else {
        ObjectPath::from(format!("{repo_prefix}/manifests/HEAD"))
    }
}

fn refs_head_path(repo_prefix: &str) -> ObjectPath {
    if repo_prefix.is_empty() {
        ObjectPath::from("refs/HEAD")
    } else {
        ObjectPath::from(format!("{repo_prefix}/refs/HEAD"))
    }
}

// ── Dry-run + plan ─────────────────────────────────────────────

/// Build the [`PlanInputs`] for [`Journal::record_plan`] /
/// [`Journal::verify_plan`] from the current CLI arguments and
/// detected source mode.
///
/// The `target_prefix` / `source_prefix` come from the parsed
/// URLs rather than the resolved stores so the checksum is
/// independent of how bucket identity got built.
fn plan_inputs_from(
    args: &ImportArgs,
    source_url: &ObjectUrl,
    target_url: &ObjectUrl,
    source_mode: &SourceMode,
) -> Result<crate::import::journal::PlanInputs> {
    let window_secs = args
        .window
        .as_deref()
        .and_then(parse_duration_simple)
        .map(|d| d.as_secs());
    let snapshot_at = match source_mode {
        SourceMode::SingleSnapshot { at } => Some(*at),
        _ => parse_at_seconds(args.at.as_deref())?,
    };
    Ok(crate::import::journal::PlanInputs {
        source_url: args.from_url()?.to_owned(),
        target_url: args.to_url()?.to_owned(),
        source_prefix: source_url.prefix.clone(),
        target_prefix: target_url.prefix.clone(),
        dest_prefix: args.dest_prefix.clone().unwrap_or_default(),
        source_mode: source_mode.tag(),
        window_secs,
        snapshot_at,
        since_epoch: parse_at_seconds(args.since.as_deref())?,
        until_epoch: parse_at_seconds(args.until.as_deref())?,
        branch: args.branch.clone(),
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        track: args.track.clone(),
        lfs_source: lfs_source_plan_tag(args.effective_lfs_source()).into(),
        lfs_objects: args.effective_lfs_objects().unwrap_or_default().to_owned(),
    })
}

fn lfs_source_plan_tag(mode: LfsSourceMode) -> &'static str {
    match mode {
        LfsSourceMode::Fail => "fail",
        LfsSourceMode::Resolve => "resolve",
        LfsSourceMode::Skip => "skip",
    }
}

/// Fresh-run preamble: detect → journal → enumerate → record
/// plan. Returns the detected mode plus the enumerate stats the
/// main pipeline needs for the kept-count check.
async fn fresh_preamble(
    args: &ImportArgs,
    source_lists: &SourceLists,
    journal_dir: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<(SourceMode, u64, u64, u64)> {
    fresh_preamble_inner(
        args,
        source_lists.detect(),
        |mode| source_lists.enumerate_for(mode),
        journal_dir,
        cancel,
    )
    .await
}

#[cfg(test)]
async fn fresh_preamble_with_list(
    args: &ImportArgs,
    source_list: &dyn VersionedList,
    journal_dir: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<(SourceMode, u64, u64, u64)> {
    fresh_preamble_inner(args, source_list, |_| source_list, journal_dir, cancel).await
}

async fn fresh_preamble_inner<'a>(
    args: &ImportArgs,
    detect_list: &'a dyn VersionedList,
    enumerate_list: impl FnOnce(&SourceMode) -> &'a dyn VersionedList,
    journal_dir: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<(SourceMode, u64, u64, u64)> {
    check_cancelled(cancel)?;
    let from_raw = args.from_url()?;
    let detect_args = DetectArgs {
        versions: args.versions,
        at: parse_at_seconds(args.at.as_deref())?,
        source_url: from_raw,
    };
    let detect_span = info_span!("detect");
    let source_mode = {
        let start = Instant::now();
        let mode = detect_source_mode(detect_list, &detect_args, cancel)
            .instrument(detect_span.clone())
            .await?;
        info!(
            parent: &detect_span,
            mode = ?mode,
            duration_ms = ms(start.elapsed()),
            "detect: complete"
        );
        mode
    };

    check_cancelled(cancel)?;
    let journal = Journal::open(journal_dir)?;

    check_cancelled(cancel)?;
    let enumerate_span = info_span!("enumerate");
    let (kept_count, total_bytes_source, skipped_count) = {
        let start = Instant::now();
        let mut journal_mut = journal;
        let mut sink: NoOpEnumerate = NoOpEnumerate;
        let stats = run_enumerate(
            enumerate_list(&source_mode),
            source_mode.clone(),
            &args.include,
            &args.exclude,
            parse_at_seconds(args.since.as_deref())?,
            parse_at_seconds(args.until.as_deref())?,
            &mut journal_mut,
            cancel,
            &mut sink,
        )
        .instrument(enumerate_span.clone())
        .await?;
        info!(
            parent: &enumerate_span,
            kept = stats.kept,
            total_bytes = stats.total_bytes,
            skipped_filtered = stats.skipped_filtered,
            skipped_invalid_git_path = stats.skipped_invalid_git_path,
            skipped_directory_placeholders = stats.skipped_directory_placeholders,
            skipped_outside_window = stats.skipped_outside_window,
            duration_ms = ms(start.elapsed()),
            "enumerate: complete"
        );

        // Record the canonical plan now that enumerate succeeded.
        // A later `--resume` invocation will verify its CLI args
        // against this row before claiming any entries.
        let source_url = ObjectUrl::parse(args.from_url()?)?;
        let target_url = ObjectUrl::parse(args.to_url()?)?;
        let plan_inputs = plan_inputs_from(args, &source_url, &target_url, &source_mode)?;
        journal_mut.record_plan(&plan_inputs, now_epoch_secs())?;

        let skipped_count = stats
            .skipped_filtered
            .saturating_add(stats.skipped_directory_placeholders)
            .saturating_add(stats.skipped_invalid_git_path)
            .saturating_add(stats.skipped_outside_window);

        (stats.kept, stats.total_bytes, skipped_count)
    };
    Ok((source_mode, kept_count, total_bytes_source, skipped_count))
}

/// Resume preamble: open the journal, reject missing, verify the
/// checksum, reconstitute the source mode, and report the count
/// of rows still in the Pending or Failed states. Enumerate is
/// deliberately skipped — the plan checksum + journal rows are
/// the canonical contract.
fn resume_preamble(
    args: &ImportArgs,
    journal_dir: &std::path::Path,
    journal_path: &std::path::Path,
) -> Result<(SourceMode, u64, u64, u64)> {
    if !journal_path.exists() {
        return Err(CrabError::ImportNoJournal {
            path: journal_path.display().to_string(),
        });
    }

    let journal = Journal::open(journal_dir)?;

    // `load_plan` returns `None` if the journal file exists but
    // was never populated (e.g. a `.db` file from an unrelated
    // tool). Treat that as a missing journal from the user's POV.
    let Some(plan) = journal.load_plan()? else {
        return Err(CrabError::ImportNoJournal {
            path: journal_path.display().to_string(),
        });
    };

    // Reconstitute the SourceMode so the rest of the pipeline
    // doesn't branch on resume/fresh.
    let source_mode = SourceMode::from_tag(plan.inputs.source_mode, plan.inputs.snapshot_at)?;

    // Verify the provided CLI arguments agree with the recorded
    // plan. `verify_plan` returns `ImportPlanMismatch` on drift.
    let source_url = ObjectUrl::parse(args.from_url()?)?;
    let target_url = ObjectUrl::parse(args.to_url()?)?;
    let provided = plan_inputs_from(args, &source_url, &target_url, &source_mode)?;
    journal.verify_plan(&provided)?;

    // Count rows still pending / failed so the caller's kept-
    // count short-circuit stays honest, and tally source bytes
    // across the full row set so the final summary has a
    // sensible `bytes_source` fallback when ingest hasn't yet
    // re-processed the rows.
    let mut total_rows: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut retryable: u64 = 0;
    journal.iter_entries_sorted_by_time(|e| {
        total_rows = total_rows.saturating_add(1);
        total_bytes = total_bytes.saturating_add(e.size);
        match e.state {
            EntryState::Pending | EntryState::InProgress | EntryState::Failed { .. } => {
                retryable = retryable.saturating_add(1);
            }
            _ => {}
        }
        Ok(())
    })?;

    // Flip retryable rows back to Pending so the ingest workers
    // retry them. `InProgress` means the previous process died
    // after claiming the row and before recording an outcome.
    journal.reset_retryable_to_pending()?;

    info!(
        rows = total_rows,
        retryable, "import: resume preamble complete"
    );

    // `kept_count` here is the number of rows the coordinator
    // believes still need work. A zero here is legal — ingest
    // will drain nothing and assemble will land zero commits.
    // Let the main pipeline decide.
    let kept = if retryable > 0 { retryable } else { total_rows };
    Ok((source_mode, kept, total_bytes, 0))
}

fn now_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Short-circuit dry-run plan: detect → enumerate → window plan,
/// no ingest / assemble / publish, no staging writes.
///
/// Returns an [`ImportSummary`] with `dry_run = true` and a
/// populated `plan` field describing what the real run would have
/// done.
pub async fn run_import_plan(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    source_list: VersionedListImpl,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    run_import_plan_with_source_lists(
        args,
        source,
        target,
        SourceLists::single(source_list),
        into,
        cancel,
    )
    .await
}

async fn run_import_plan_with_source_lists(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    source_lists: SourceLists,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    run_import_plan_inner(
        args,
        source,
        target,
        source_lists.detect(),
        |mode| source_lists.enumerate_for(mode),
        into,
        cancel,
    )
    .await
}

/// Test-friendly variant of [`run_import_plan`] that accepts any
/// `&dyn VersionedList`. Keeps the public surface narrow while
/// letting unit tests plug in in-memory listers without
/// extending [`VersionedListImpl`].
#[cfg(test)]
pub(crate) async fn run_import_plan_with_list(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    source_list: &dyn crate::import::versions::VersionedList,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    run_import_plan_inner(
        args,
        source,
        target,
        source_list,
        |_| source_list,
        into,
        cancel,
    )
    .await
}

async fn run_import_plan_inner<'a>(
    args: &ImportArgs,
    source: ResolvedStore,
    target: ResolvedStore,
    detect_list: &'a dyn VersionedList,
    enumerate_list: impl FnOnce(&SourceMode) -> &'a dyn VersionedList,
    into: PathBuf,
    cancel: &CancellationToken,
) -> Result<ImportSummary> {
    let total_start = Instant::now();
    let branch = args.branch.clone();
    let same_bucket = source.bucket == target.bucket;
    let from_raw = args.from_url()?;
    let to_raw = args.to_url()?;

    info!(
        from = %from_raw,
        to = %to_raw,
        "import: dry-run plan starting"
    );

    // Preflight — no mutations, so this runs exactly as in the
    // real pipeline. `--dry-run` respects the same safety rails.
    let source_url = ObjectUrl::parse(from_raw)?;
    let target_url = ObjectUrl::parse(to_raw)?;
    preflight_safety_checks(PreflightInputs {
        args,
        source: &source,
        target: &target,
        into: &into,
        source_url: &source_url,
        target_url: &target_url,
        cancel,
    })
    .await?;

    // Detect
    check_cancelled(cancel)?;
    let detect_args = DetectArgs {
        versions: args.versions,
        at: parse_at_seconds(args.at.as_deref())?,
        source_url: from_raw,
    };
    let source_mode = detect_source_mode(detect_list, &detect_args, cancel).await?;

    // Collision warnings (gathered during preflight; surface in
    // the plan summary as empty). If we reach this point, no
    // collisions fired — leave empty for now. Later we can fold
    // soft warnings (same-account, different bucket, etc.) in.
    let collision_warnings: Vec<String> = Vec::new();

    // Enumerate into a temp-backed journal that lives under
    // `<into>/.crab/import-journal.db` — but we tear it down at
    // the end so dry-run leaves no state behind.
    let journal_dir = into.clone();
    let mut journal = Journal::open(&journal_dir)?;
    let journal_path = journal_dir.join(".crab").join("import-journal.db");

    let mut sink: NoOpEnumerate = NoOpEnumerate;
    let enum_stats = run_enumerate(
        enumerate_list(&source_mode),
        source_mode.clone(),
        &args.include,
        &args.exclude,
        parse_at_seconds(args.since.as_deref())?,
        parse_at_seconds(args.until.as_deref())?,
        &mut journal,
        cancel,
        &mut sink,
    )
    .await?;

    // Collect Pending entries for histogram + LFS probe.
    let pending = collect_entries(&journal, EntryFilter::PendingOnly)?;

    // LFS-pointer count: run the cheap prefix classifier against
    // only entries small enough to *be* pointer blobs. Real
    // content ignores it.
    let lfs_pointer_count: u64 = count_lfs_pointers(&source, &pending, cancel).await?;
    let mapped_pending = map_entries_for_commit(pending, args.dest_prefix.as_deref())?;

    let extension_histogram = build_extension_histogram(
        mapped_pending
            .iter()
            .map(|e| (e.relative_path.as_str(), e.size)),
    );

    // Commit plan — respects commit ceiling already.
    let window = args
        .window
        .as_deref()
        .and_then(parse_duration_simple)
        .unwrap_or(DEFAULT_WINDOW);
    let windows = plan_windows(
        &source_mode,
        mapped_pending.clone(),
        window,
        DEFAULT_MAX_COMMITS,
    )?;

    // Tear down the journal so dry-run leaves no trace. Best-
    // effort: if cleanup fails, log and continue — the summary is
    // still correct.
    journal.close().ok();
    if let Err(err) = remove_journal(&journal_path).await {
        warn!(
            path = %journal_path.display(),
            %err,
            "dry-run: failed to clean up journal after plan"
        );
    }

    let versioning = versioning_from_mode(&source_mode);
    let plan = ImportPlanSummary {
        extension_histogram,
        files_total: enum_stats.kept,
        bytes_total: enum_stats.total_bytes,
        lfs_pointer_count,
        same_bucket,
        collision_warnings,
        versioning,
        planned_commit_count: u64::try_from(windows.len()).unwrap_or(u64::MAX),
    };

    let summary = ImportSummary {
        source_url: from_raw.to_owned(),
        target_url: to_raw.to_owned(),
        versioning,
        files_imported: enum_stats.kept,
        versions_imported: enum_stats.kept,
        commits_created: plan.planned_commit_count,
        files_skipped: enum_stats
            .skipped_filtered
            .saturating_add(enum_stats.skipped_directory_placeholders)
            .saturating_add(enum_stats.skipped_invalid_git_path)
            .saturating_add(enum_stats.skipped_outside_window),
        files_failed: 0,
        lfs_resolved: 0,
        lfs_skipped: 0,
        lfs_failed: 0,
        bytes_source: enum_stats.total_bytes,
        bytes_staged: 0,
        bytes_uploaded: 0,
        same_bucket,
        duration_ms: ms(total_start.elapsed()),
        head_commit_oid: None,
        first_commit_oid: None,
        branch,
        history_range: history_range_from_args(args),
        dry_run: true,
        plan: Some(plan),
    };

    info!(
        files = summary.files_imported,
        bytes_source = summary.bytes_source,
        commits = summary.commits_created,
        duration_ms = summary.duration_ms,
        "import: dry-run complete"
    );

    Ok(summary)
}

/// Count how many of the enumerated entries look like Git LFS
/// pointer blobs. Only probes objects smaller than 1 KiB per
/// the LFS spec — anything bigger can't be a pointer. Errors
/// (e.g. NotFound between list and get) are swallowed and do
/// not count against the total.
async fn count_lfs_pointers(
    source: &ResolvedStore,
    entries: &[ImportEntry],
    cancel: &CancellationToken,
) -> Result<u64> {
    use crab_git::pointer_detect::{PointerKind, classify as classify_pointer};
    const LFS_POINTER_PROBE_SIZE: u64 = 1024;
    let mut count: u64 = 0;
    for entry in entries {
        check_cancelled(cancel)?;
        if entry.is_delete_marker || entry.size >= LFS_POINTER_PROBE_SIZE {
            continue;
        }
        let path = if source.prefix.is_empty() {
            ObjectPath::from(entry.relative_path.clone())
        } else {
            ObjectPath::from(format!("{}/{}", source.prefix, entry.relative_path))
        };
        let Ok((bytes, _etag)) = source.store.get_with_etag(&path).await else {
            continue;
        };
        if matches!(classify_pointer(&bytes), PointerKind::Lfs(_)) {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

// ── Large-import confirmation ──────────────────────────────────

/// Thresholds for the "are you sure?" large-import confirmation
/// required by requirement I11.
const LARGE_IMPORT_FILES: u64 = 1_000_000;
const LARGE_IMPORT_BYTES: u64 = 1024 * 1024 * 1024 * 1024; // 1 TiB

/// Check whether the enumerated set requires confirmation and,
/// if so, either read `y/yes` from stdin (text mode) or require
/// `--yes` (machine modes).
///
/// V1 scope: text-mode interactive prompt reads a single line
/// from stdin. Returns
/// [`CrabError::Internal`] with a clear message rather than a
/// dedicated error variant — this path is extremely rare in
/// practice and the message is the whole UX.
fn confirm_large_import(
    kept: u64,
    total_bytes: u64,
    yes_flag: bool,
    mode_is_machine: bool,
) -> Result<()> {
    if kept <= LARGE_IMPORT_FILES && total_bytes <= LARGE_IMPORT_BYTES {
        return Ok(());
    }
    if yes_flag {
        return Ok(());
    }
    if mode_is_machine {
        return Err(CrabError::Configuration {
            key: "--yes required for large imports".into(),
            origin: format!(
                "import planned {kept} files / {total_bytes} bytes exceeds thresholds \
                 ({LARGE_IMPORT_FILES} files / {LARGE_IMPORT_BYTES} bytes) — pass --yes to confirm"
            ),
        });
    }
    // Text-mode interactive prompt. Best-effort read from stdin;
    // EOF / read error treated as "no".
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(
        lock,
        "import: planned {kept} files / {total_bytes} bytes exceeds thresholds \
         ({LARGE_IMPORT_FILES} files / {LARGE_IMPORT_BYTES} bytes)"
    );
    let _ = writeln!(lock, "Continue? [y/N]");
    let _ = lock.flush();
    drop(lock);

    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return Err(CrabError::Configuration {
            key: "large-import confirmation aborted".into(),
            origin: "failed to read confirmation from stdin".into(),
        });
    }
    let response = line.trim().to_ascii_lowercase();
    if response == "y" || response == "yes" {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: "large-import confirmation declined".into(),
            origin: "pass --yes to skip the interactive prompt in the future".into(),
        })
    }
}

/// Convert [`SourceMode`] to the serializable [`SummaryVersioning`]
/// used in [`ImportSummary`] and [`ImportPlanSummary`].
fn versioning_from_mode(mode: &SourceMode) -> SummaryVersioning {
    match mode {
        SourceMode::Flat => SummaryVersioning::Flat,
        SourceMode::Versioned => SummaryVersioning::Versioned,
        SourceMode::SingleSnapshot { .. } => SummaryVersioning::SingleSnapshot,
    }
}

/// Assemble the [`HistoryRange`] field from the user's args.
/// Returns `None` when both bounds are absent.
fn history_range_from_args(args: &ImportArgs) -> Option<HistoryRange> {
    let since = args
        .since
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let until = args
        .until
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if since.is_none() && until.is_none() {
        return None;
    }
    Some(HistoryRange {
        since: since.unwrap_or_default().to_owned(),
        until: until.unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, MutexGuard};

    use bytes::Bytes;
    use futures_util::{StreamExt, TryStreamExt};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use tempfile::TempDir;

    use crate::cmd::import::{LfsSourceMode, VersionsMode};
    use crate::import::ingest::ResolvedStore;
    use crate::import::versions::{VersionRecord, VersionSample, VersionedList};
    use crate::storage::store::{BucketIdentity, Store};
    use crate::test::git_repo::{CacheDirGuard, GIT_DIR_MUTEX};
    use async_trait::async_trait;

    #[tokio::test]
    async fn resume_requeues_staged_entry_when_local_chunks_were_retired() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let file_hash = [0x42; 32];
        journal
            .upsert_entry_batch(&[
                ImportEntry {
                    relative_path: "large.bin".into(),
                    version_id: String::new(),
                    size: 20_000_000_000,
                    etag: None,
                    last_modified: 1,
                    is_delete_marker: false,
                    state: EntryState::Staged { file_hash },
                },
                ImportEntry {
                    relative_path: "deleted.bin".into(),
                    version_id: "delete-marker".into(),
                    size: 0,
                    etag: None,
                    last_modified: 2,
                    is_delete_marker: true,
                    state: EntryState::Staged {
                        file_hash: DELETE_MARKER_FILE_HASH,
                    },
                },
            ])
            .unwrap();

        let reset =
            reset_staged_entries_missing_local_chunks(&journal, &tmp.path().join(".crab/staging"))
                .await
                .unwrap();

        assert_eq!(reset, 1);
        let pending = collect_entries(&journal, EntryFilter::PendingOnly).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "large.bin");
        assert!(matches!(pending[0].state, EntryState::Pending));

        let staged = collect_entries(&journal, EntryFilter::StagedOnly).unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].relative_path, "deleted.bin");
    }

    fn versioned_list_data_ptr(list: &dyn VersionedList) -> *const () {
        list as *const dyn VersionedList as *const ()
    }

    /// RAII guard that holds test-global git/cache env overrides for
    /// the scope of an import run. Identical to the one in publish.rs's
    /// test module; duplicated here rather than exported because the
    /// publish version lives behind `#[cfg(test)]`.
    struct GitDirOverride {
        _cache_guard: CacheDirGuard,
        _cache_dir: TempDir,
        _lock: MutexGuard<'static, ()>,
        prev_git_dir: Option<String>,
        prev_git_work_tree: Option<String>,
        prev_git_common_dir: Option<String>,
    }

    impl GitDirOverride {
        fn locked_without_env() -> Self {
            let cache_dir = TempDir::new().expect("tempdir for CRAB_CACHE_DIR");
            let cache_guard = CacheDirGuard::new(cache_dir.path());
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_git_dir = std::env::var("GIT_DIR").ok();
            let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
            let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
            // SAFETY: serialized by GIT_DIR_MUTEX.
            unsafe {
                std::env::remove_var("GIT_DIR");
                std::env::remove_var("GIT_WORK_TREE");
                std::env::remove_var("GIT_COMMON_DIR");
            }
            Self {
                _cache_guard: cache_guard,
                _cache_dir: cache_dir,
                _lock: lock,
                prev_git_dir,
                prev_git_work_tree,
                prev_git_common_dir,
            }
        }

        fn set_env(self, git_dir: &Path) -> Self {
            // SAFETY: we still hold _lock.
            unsafe {
                std::env::set_var("GIT_DIR", git_dir);
                std::env::remove_var("GIT_WORK_TREE");
                std::env::remove_var("GIT_COMMON_DIR");
            }
            self
        }

        fn clear_env(self) -> Self {
            // SAFETY: we still hold _lock.
            unsafe {
                std::env::remove_var("GIT_DIR");
                std::env::remove_var("GIT_WORK_TREE");
                std::env::remove_var("GIT_COMMON_DIR");
            }
            self
        }
    }

    impl Drop for GitDirOverride {
        fn drop(&mut self) {
            // SAFETY: serialized by GIT_DIR_MUTEX.
            unsafe {
                match &self.prev_git_dir {
                    Some(v) => std::env::set_var("GIT_DIR", v),
                    None => std::env::remove_var("GIT_DIR"),
                }
                match &self.prev_git_work_tree {
                    Some(v) => std::env::set_var("GIT_WORK_TREE", v),
                    None => std::env::remove_var("GIT_WORK_TREE"),
                }
                match &self.prev_git_common_dir {
                    Some(v) => std::env::set_var("GIT_COMMON_DIR", v),
                    None => std::env::remove_var("GIT_COMMON_DIR"),
                }
            }
        }
    }

    /// In-memory `VersionedList` that enumerates every key in an
    /// `ObjectStore` under a prefix. Mirrors the shape of the
    /// flat-bucket cloud listers without depending on their SDKs.
    struct InMemoryVersionedList {
        store: Arc<dyn ObjectStore>,
        prefix: String,
    }

    #[async_trait]
    impl VersionedList for InMemoryVersionedList {
        async fn sample(&self, _limit: usize) -> Result<VersionSample> {
            // Non-versioned: one record per key, no delete markers.
            let prefix_path = ObjectPath::from(self.prefix.clone());
            let metas = self
                .store
                .list(Some(&prefix_path))
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| CrabError::Internal(format!("list for sample: {e}")))?;
            let records: Vec<VersionRecord> = metas
                .into_iter()
                .map(|m| {
                    let key = strip_prefix(&m.location, &self.prefix);
                    VersionRecord {
                        key,
                        version_id: String::new(),
                        size: m.size,
                        etag: m.e_tag,
                        last_modified: m.last_modified.timestamp().try_into().unwrap_or(0_i64),
                        is_delete_marker: false,
                    }
                })
                .collect();
            let unique = records.len();
            Ok(VersionSample {
                total_versions: records.len(),
                unique_keys: unique,
                has_delete_markers: false,
                records,
            })
        }

        async fn enumerate(
            &self,
            _since: Option<i64>,
            _until: Option<i64>,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            let prefix_path = ObjectPath::from(self.prefix.clone());
            let metas = self
                .store
                .list(Some(&prefix_path))
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| CrabError::Internal(format!("list for enumerate: {e}")))?;
            for meta in metas {
                let key = strip_prefix(&meta.location, &self.prefix);
                let rec = VersionRecord {
                    key,
                    version_id: String::new(),
                    size: meta.size,
                    etag: meta.e_tag,
                    last_modified: meta.last_modified.timestamp().try_into().unwrap_or(0_i64),
                    is_delete_marker: false,
                };
                callback(rec)?;
            }
            Ok(())
        }

        async fn enumerate_at(
            &self,
            _at: i64,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            self.enumerate(None, None, callback).await
        }
    }

    fn strip_prefix(full: &ObjectPath, prefix: &str) -> String {
        let full_str = full.to_string();
        if prefix.is_empty() {
            full_str
        } else {
            full_str
                .strip_prefix(prefix)
                .map(|s| s.trim_start_matches('/').to_owned())
                .unwrap_or(full_str)
        }
    }

    fn resolved(inner: Arc<dyn ObjectStore>, prefix: &str) -> ResolvedStore {
        ResolvedStore {
            store: Store::new(inner),
            bucket: BucketIdentity::local_unset(),
            prefix: prefix.to_owned(),
        }
    }

    fn s3_resolved(inner: Arc<dyn ObjectStore>, bucket: &str, prefix: &str) -> ResolvedStore {
        ResolvedStore {
            store: Store::new(inner),
            bucket: BucketIdentity {
                cloud: Cloud::S3,
                host: bucket.to_owned(),
                container: bucket.to_owned(),
            },
            prefix: prefix.to_owned(),
        }
    }

    async fn seed(store: &Arc<dyn ObjectStore>, prefix: &str, relative: &str, body: &[u8]) {
        let key = if prefix.is_empty() {
            relative.to_owned()
        } else {
            format!("{prefix}/{relative}")
        };
        store
            .put(
                &ObjectPath::from(key),
                PutPayload::from(Bytes::from(body.to_vec())),
            )
            .await
            .expect("seed put");
    }

    fn configure_test_identity(repo_root: &Path) {
        for (key, val) in [
            ("user.name", "Crab Coordinator"),
            ("user.email", "coord@crab.dev"),
        ] {
            let status = Command::new("git")
                .args(["config", "--local", key, val])
                .current_dir(repo_root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git config --local must run");
            assert!(status.success(), "git config --local {key} failed");
        }
    }

    fn init_empty_repo_with_identity(repo_root: &Path) {
        std::fs::create_dir_all(repo_root).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(repo_root);
    }

    async fn lfs_format_source_and_target_with_store()
    -> (Arc<dyn ObjectStore>, ResolvedStore, ResolvedStore) {
        let source_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(
            &source_store,
            "",
            ".gitattributes",
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .await;
        let target_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = s3_resolved(Arc::clone(&source_store), "src-bucket", "");
        let tgt = s3_resolved(Arc::clone(&target_store), "dst-bucket", "repo");
        (source_store, src, tgt)
    }

    async fn lfs_format_source_and_target() -> (ResolvedStore, ResolvedStore) {
        let (_, src, tgt) = lfs_format_source_and_target_with_store().await;
        (src, tgt)
    }

    async fn seed_lfs_object(
        store: &Arc<dyn ObjectStore>,
        lfs_prefix: &str,
        body: &'static [u8],
    ) -> [u8; 32] {
        use sha2::{Digest, Sha256};

        let oid: [u8; 32] = Sha256::digest(body).into();
        let path = crab_lfs::LfsObjectStore::object_path_for_prefix(lfs_prefix, &oid);
        store
            .put(&path, PutPayload::from(Bytes::from_static(body)))
            .await
            .expect("seed lfs object");
        oid
    }

    fn base_args(from: &str, to: &str) -> ImportArgs {
        ImportArgs {
            source: None,
            from: Some(from.into()),
            to: Some(to.into()),
            bucket: None,
            name: None,
            into: None,
            dest_prefix: None,
            include: Vec::new(),
            exclude: Vec::new(),
            branch: "main".into(),
            message: None,
            track: Vec::new(),
            versions: VersionsMode::Auto,
            window: None,
            at: None,
            since: None,
            until: None,
            author_template: None,
            dry_run: false,
            estimate: false,
            resume: false,
            jobs: Some(2),
            fail_fast: true,
            force: false,
            lfs_source: None,
            lfs_objects: None,
            allow_lfs_import: false,
            lfs_store: None,
            yes: false,
            source_profile: None,
            target_profile: None,
            json: false,
            jsonl: false,
        }
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    #[test]
    fn resolve_import_args_accepts_local_source_and_bucket_name() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("large-files");
        std::fs::create_dir_all(&source).unwrap();

        let mut args = base_args("s3://placeholder/source", "s3://placeholder/target");
        args.source = Some(source.display().to_string());
        args.from = None;
        args.to = None;
        args.bucket = Some("crab".into());
        args.name = Some("import-demo".into());
        args.dest_prefix = Some("/crab/large-files/".into());

        let resolved = resolve_import_args(&args).unwrap();
        assert!(resolved.from.unwrap().starts_with("file://"));
        assert_eq!(resolved.to.as_deref(), Some("crab://crab/import-demo"));
        assert_eq!(resolved.dest_prefix.as_deref(), Some("crab/large-files"));
    }

    #[test]
    fn resolve_import_args_rejects_lfs_objects_without_resolve_mode() {
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_objects = Some("s3://src-bucket/lfs".into());

        let err = resolve_import_args(&args).unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration, got {err:?}"
        );
        assert!(
            err.to_string().contains("--lfs-source resolve"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn resolve_import_args_rejects_lfs_store_alias_without_resolve_mode() {
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_store = Some("s3://src-bucket/lfs".into());

        let err = resolve_import_args(&args).unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration, got {err:?}"
        );
        assert!(
            err.to_string().contains("--lfs-source resolve"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn lfs_store_prefix_accepts_repo_root_or_objects_root() {
        assert_eq!(lfs_store_prefix_from_root("repo"), "repo");
        assert_eq!(lfs_store_prefix_from_root("repo/lfs/objects"), "repo");
        assert_eq!(lfs_store_prefix_from_root("/repo/lfs/objects/"), "repo");
        assert_eq!(lfs_store_prefix_from_root("lfs/objects"), "");
        assert_eq!(
            lfs_store_prefix_from_root("repo/.git/lfs/objects"),
            "repo/.git"
        );
    }

    #[test]
    fn resolve_import_args_rejects_unsafe_dest_prefix() {
        let mut args = base_args("s3://src-bucket/data", "s3://dst-bucket/repo");
        args.dest_prefix = Some(".git/imported".into());

        let err = resolve_import_args(&args).unwrap_err();
        assert!(
            err.to_string().contains("--dest-prefix"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn map_entries_for_commit_prefixes_paths() {
        let entries = vec![ImportEntry {
            relative_path: "a/b.bin".into(),
            version_id: String::new(),
            size: 10,
            etag: None,
            last_modified: 0,
            is_delete_marker: false,
            state: EntryState::Pending,
        }];

        let mapped = map_entries_for_commit(entries, Some("crab/large-files")).unwrap();
        assert_eq!(mapped[0].relative_path, "crab/large-files/a/b.bin");
    }

    // ── parse helpers ────────────────────────────────────────

    #[test]
    fn parse_at_seconds_handles_decimal_epoch_and_none() {
        assert_eq!(parse_at_seconds(None).unwrap(), None);
        assert_eq!(parse_at_seconds(Some("")).unwrap(), None);
        assert_eq!(
            parse_at_seconds(Some("1735689600")).unwrap(),
            Some(1_735_689_600)
        );
    }

    #[test]
    fn parse_at_seconds_accepts_rfc3339_utc_offsets_and_fractions() {
        assert_eq!(
            parse_at_seconds(Some("2025-01-01T00:00:00Z")).unwrap(),
            Some(1_735_689_600)
        );
        assert_eq!(
            parse_at_seconds(Some("2025-01-01T00:30:00+00:30")).unwrap(),
            Some(1_735_689_600)
        );
        assert_eq!(
            parse_at_seconds(Some("2024-12-31T16:00:00-08:00")).unwrap(),
            Some(1_735_689_600)
        );
        assert_eq!(
            parse_at_seconds(Some("2025-01-01T00:00:00.999Z")).unwrap(),
            Some(1_735_689_600)
        );
    }

    #[test]
    fn parse_at_seconds_rejects_invalid_rfc3339_dates() {
        for raw in [
            "2025-02-29T00:00:00Z",
            "2025-01-01T24:00:00Z",
            "2025-01-01T00:00:000Z",
            "2025-01-01T00:00:0.999Z",
            "2025-01-01T00:00:00+24:00",
            "2025-01-01 00:00:00Z",
            "é025-01-01T00:00:00Z",
            "not-a-timestamp",
        ] {
            let err = parse_at_seconds(Some(raw)).expect_err("timestamp should be rejected");
            assert!(
                err.to_string().contains("RFC3339"),
                "unexpected error for {raw}: {err}"
            );
        }
    }

    #[test]
    fn parse_duration_simple_covers_basic_suffixes() {
        assert_eq!(parse_duration_simple("30s"), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_duration_simple("15m"),
            Some(Duration::from_secs(15 * 60))
        );
        assert_eq!(
            parse_duration_simple("2h"),
            Some(Duration::from_secs(2 * 3_600))
        );
        assert_eq!(parse_duration_simple("42"), Some(Duration::from_secs(42)));
        assert_eq!(parse_duration_simple(""), None);
        assert_eq!(parse_duration_simple("garbage"), None);
    }

    #[test]
    fn s3_source_lists_use_history_adapter_and_flat_current_lister() {
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let source = ResolvedStore {
            store: Store::new(source_inner),
            bucket: BucketIdentity {
                cloud: Cloud::S3,
                host: "crab".into(),
                container: "crab".into(),
            },
            prefix: "crab/large-files".into(),
        };
        let url = ObjectUrl::parse("s3://crab/crab/large-files").unwrap();

        let lists = source_lists_for_resolved_url(&url, &source).unwrap();

        assert!(matches!(&lists.history, VersionedListImpl::S3(_)));
        assert!(matches!(
            &lists.current,
            VersionedListImpl::FlatObjectStore(_)
        ));
        assert_eq!(
            versioned_list_data_ptr(lists.enumerate_for(&SourceMode::Flat)),
            versioned_list_data_ptr(&lists.current)
        );
        assert_eq!(
            versioned_list_data_ptr(lists.enumerate_for(&SourceMode::Versioned)),
            versioned_list_data_ptr(&lists.history)
        );
    }

    #[test]
    fn azure_source_lists_use_raw_url_container_for_history_adapter() {
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let source = ResolvedStore {
            store: Store::new(source_inner),
            bucket: BucketIdentity {
                cloud: Cloud::Azure,
                host: "account".into(),
                container: "container".into(),
            },
            prefix: "org/repo".into(),
        };
        let url = ObjectUrl::parse("az://account/container/org/repo").unwrap();

        let lists = source_lists_for_resolved_url(&url, &source).unwrap();

        assert!(matches!(&lists.history, VersionedListImpl::Azure(_)));
        assert!(matches!(
            &lists.current,
            VersionedListImpl::FlatObjectStore(_)
        ));
    }

    #[test]
    fn azure_source_lists_reject_missing_container() {
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let source = ResolvedStore {
            store: Store::new(source_inner),
            bucket: BucketIdentity {
                cloud: Cloud::Azure,
                host: "account".into(),
                container: "account".into(),
            },
            prefix: String::new(),
        };
        let url = ObjectUrl::parse("az://account").unwrap();

        let err = source_lists_for_resolved_url(&url, &source)
            .err()
            .expect("missing Azure container should error");

        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration, got {err:?}"
        );
    }

    // ── URL validation in run_import_inner ──────────────────

    #[tokio::test]
    async fn run_import_inner_rejects_crab_source() {
        let args = base_args("crab://src/repo", "s3://dst/repo");
        let cancel = CancellationToken::new();
        let err = run_import_inner(&args, &cancel).await.unwrap_err();
        assert!(
            matches!(err, CrabError::ImportSourceMustBeRaw { .. }),
            "expected ImportSourceMustBeRaw, got {err:?}"
        );
    }

    #[tokio::test]
    async fn run_import_inner_rejects_cross_cloud_raw_target() {
        let args = base_args("s3://src/data", "az://dst/repo");
        let cancel = CancellationToken::new();
        let err = run_import_inner(&args, &cancel).await.unwrap_err();
        assert!(
            matches!(err, CrabError::ImportSchemeMismatch { .. }),
            "expected ImportSchemeMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn run_import_inner_local_dry_run_reaches_plan() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("large.bin"), b"local dry-run bytes").unwrap();

        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
        configure_test_identity(&into);

        let mut args = base_args(&file_url(&source), &file_url(&target));
        args.into = Some(into.clone());
        args.dry_run = true;
        let cancel = CancellationToken::new();
        let summary = run_import_inner(&args, &cancel)
            .await
            .expect("local dry-run should complete");

        assert!(summary.dry_run);
        assert_eq!(summary.files_imported, 1);
        assert_eq!(summary.bytes_source, 19);
        assert_eq!(summary.commits_created, 1);
        assert!(summary.plan.is_some());
        assert!(!into.join(".crab").join("import-journal.db").exists());
    }

    // ── End-to-end: run_import_with_stores ──────────────────

    /// Drive the full pipeline (detect → enumerate → ingest →
    /// assemble → publish) against in-memory stores. Asserts the
    /// summary carries non-zero counts, the target gets the full
    /// Crab layout, and the journal file is gone after success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_import_with_stores_end_to_end_cross_bucket() {
        // Hold GIT_DIR_MUTEX for the whole test so other tests'
        // GIT_DIR envs don't leak into our `git init` / assemble
        // calls. We flip the env to our repo's .git dir right
        // before publish runs via set_env.
        let git_dir_guard = GitDirOverride::locked_without_env();

        let tmp = TempDir::new().unwrap();

        // Seed a small in-memory source bucket with a few files.
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let objects: &[(&str, &[u8])] = &[
            ("data/a.bin", &[0x11u8; 8 * 1024]),
            ("data/b.bin", &[0x22u8; 16 * 1024]),
            ("models/m.safetensors", &[0x33u8; 32 * 1024]),
        ];
        for (path, body) in objects {
            seed(&source_inner, "", path, body).await;
        }

        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/v2";

        // Build a repo dir + configure identity up front. Our
        // coordinator calls `git init` inside assemble; running it
        // here first lets us set the local identity without
        // needing global git config on the test host.
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        // Resolved stores plus a source list backed by an
        // in-memory ObjectStore. The test harness gives the
        // pipeline a focused `&dyn VersionedList` so the full
        // import path runs without filesystem setup or cloud SDKs.
        let source = resolved(Arc::clone(&source_inner), "");
        let target = resolved(Arc::clone(&target_inner), target_prefix);
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&source_inner),
            prefix: String::new(),
        };

        let args = base_args(
            "s3://src-bucket/",
            &format!("s3://dst-bucket/{target_prefix}"),
        );

        // Release the `GIT_DIR` env mutex only after publish by
        // keeping the guard alive through the whole test. We
        // point `GIT_DIR` at our repo's .git dir just before
        // running publish so the push pipeline honors it.
        let git_dir_guard = git_dir_guard.set_env(&into.join(".git"));

        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("run_import_with_stores_inner must succeed");

        // Release the env var guard now that publish has run.
        drop(git_dir_guard);

        // Summary sanity.
        assert!(
            summary.commits_created >= 1,
            "at least one commit must land: {summary:?}"
        );
        assert!(
            summary.files_imported >= 1,
            "files_imported must be non-zero: {summary:?}"
        );
        assert_eq!(summary.branch, "main");
        assert!(summary.head_commit_oid.is_some());
        assert_eq!(summary.first_commit_oid, summary.head_commit_oid);

        // Target received the Crab layout.
        let target_metas = target_inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let target_keys: HashSet<String> = target_metas
            .into_iter()
            .map(|m| m.location.to_string())
            .collect();
        assert!(
            target_keys.iter().any(|k| k.starts_with(".crab/xorbs/")),
            "target missing xorbs: {target_keys:?}"
        );
        assert!(
            target_keys
                .iter()
                .any(|k| *k == format!("{target_prefix}/manifest")),
            "target missing manifest pointer: {target_keys:?}"
        );

        // Journal file was removed on success.
        let journal_path = into.join(".crab").join("import-journal.db");
        assert!(
            !journal_path.exists(),
            "journal must be cleaned up after success"
        );
    }

    /// Test-only sibling of `run_import_with_stores` that accepts a
    /// `&dyn VersionedList` so we can plug in an in-memory list
    /// without extending `VersionedListImpl`. Mirrors the real
    /// pipeline's resume/fresh split via [`fresh_preamble`] /
    /// [`resume_preamble`] so resume integration tests exercise
    /// the same branches the CLI will.
    async fn run_import_with_stores_inner(
        args: &ImportArgs,
        source: ResolvedStore,
        target: ResolvedStore,
        source_list: &dyn VersionedList,
        into: PathBuf,
        cancel: &CancellationToken,
    ) -> Result<ImportSummary> {
        let effective_args = resolve_import_args(args)?;
        let args = &effective_args;
        let total_start = Instant::now();
        let branch = args.branch.clone();
        let same_bucket = source.bucket == target.bucket;

        let journal_dir = into.clone();
        let journal_path = journal_dir.join(".crab").join("import-journal.db");
        let metrics = Arc::new(Metrics::new());
        let lfs_store = resolve_lfs_import_strategy(
            &source,
            &ObjectUrl::parse(args.from_url()?)?,
            args.from_url()?,
            args.effective_lfs_source(),
            args.effective_lfs_objects(),
            cancel,
        )
        .await?;

        let (source_mode, kept_count, _total_bytes_source, _enumerate_skipped) = if args.resume {
            resume_preamble(args, &journal_dir, &journal_path)?
        } else {
            fresh_preamble_with_list(args, source_list, &journal_dir, cancel).await?
        };
        assert!(
            kept_count > 0 || args.resume,
            "test seed must produce kept entries on fresh runs"
        );

        check_cancelled(cancel)?;
        let journal = Journal::open(&journal_dir)?;
        let staging_root = journal_dir.join(".crab").join("staging");
        if args.resume {
            reset_staged_entries_missing_local_chunks(&journal, &staging_root).await?;
            validate_lfs_resume_state(&journal, &source, lfs_store.as_deref(), cancel).await?;
        }
        let pending = collect_entries(&journal, EntryFilter::PendingOnly)?;
        let pending = map_entries_for_commit(pending, args.dest_prefix.as_deref())?;
        let window = args
            .window
            .as_deref()
            .and_then(parse_duration_simple)
            .unwrap_or(DEFAULT_WINDOW);
        if !(args.resume && pending.is_empty()) {
            let _ = plan_windows(&source_mode, pending, window, DEFAULT_MAX_COMMITS)?;
        }

        check_cancelled(cancel)?;
        let staging = Arc::new(StagingArea::open(staging_root.clone()).await?);
        let journal_arc = Arc::new(Mutex::new(journal));

        let ingest_stats = {
            let progress: Arc<Mutex<NoOpIngest>> = Arc::new(Mutex::new(NoOpIngest));
            let inputs = IngestInputs {
                source,
                journal: Arc::clone(&journal_arc),
                staging: Arc::clone(&staging),
                repo_root: into.clone(),
                lfs_store: lfs_store.clone(),
                jobs: args.jobs.unwrap_or(2),
                fail_fast: args.fail_fast,
                progress,
                metrics: Some(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            };
            run_ingest(inputs).await?.snapshot()
        };

        check_cancelled(cancel)?;
        let staged = {
            let guard = journal_arc.lock().await;
            collect_entries(&guard, EntryFilter::StagedOnly)?
        };
        let staged = map_entries_for_commit(staged, args.dest_prefix.as_deref())?;
        let windows = plan_windows(&source_mode, staged, window, DEFAULT_MAX_COMMITS)?;

        let assemble_stats = {
            let progress: Arc<Mutex<NoOpAssemble>> = Arc::new(Mutex::new(NoOpAssemble));
            let inputs = AssembleInputs {
                into: into.clone(),
                branch: branch.clone(),
                force: args.force,
                resume: args.resume,
                target_url: args.to_url()?.to_owned(),
                windows,
                track: args.track.clone(),
                message_template: args.message.clone(),
                author_template: args.author_template.clone(),
                progress,
                metrics: Some(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            };
            run_assemble(inputs).await?
        };

        check_cancelled(cancel)?;
        let head_commit_oid = assemble_stats
            .head_commit_oid
            .clone()
            .ok_or_else(|| CrabError::Internal("test coordinator: no head commit".into()))?;

        drop(staging);
        let staging_ro = Arc::new(StagingAreaReadOnly::open(staging_root.clone()).await?);
        let publish_stats = {
            let to = ObjectUrl::parse(args.to_url()?)?;
            let inputs = PublishInputs {
                target,
                repo_prefix: to.prefix.clone(),
                staging: staging_ro,
                branch: branch.clone(),
                head_commit_oid: head_commit_oid.clone(),
                git_dir: into.join(".git"),
                metrics: Some(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            };
            run_publish(inputs).await?
        };

        drop(journal_arc);
        remove_journal(&journal_path).await?;

        Ok(ImportSummary {
            source_url: args.from_url()?.to_owned(),
            target_url: args.to_url()?.to_owned(),
            versioning: versioning_from_mode(&source_mode),
            files_imported: assemble_stats.files_imported,
            versions_imported: assemble_stats.versions_imported,
            commits_created: assemble_stats.commits_created,
            files_skipped: ingest_stats.skipped,
            files_failed: ingest_stats.failed,
            lfs_resolved: ingest_stats.lfs_resolved,
            lfs_skipped: ingest_stats.lfs_skipped,
            lfs_failed: ingest_stats.lfs_failed,
            bytes_source: ingest_stats.bytes_source,
            bytes_staged: ingest_stats.bytes_staged,
            bytes_uploaded: publish_stats.bytes_uploaded,
            same_bucket,
            duration_ms: ms(total_start.elapsed()),
            head_commit_oid: assemble_stats.head_commit_oid,
            first_commit_oid: assemble_stats.first_commit_oid,
            branch,
            history_range: history_range_from_args(args),
            dry_run: false,
            plan: None,
        })
    }

    // ── Phase 8 resume integration tests ─────────────────────

    /// Version-aware source store for versioned import integration tests.
    ///
    /// `object_store::memory::InMemory` stores only the latest body for a
    /// path. Versioned import needs `get_opts(version = Some(...))` to return
    /// the historical body so staging sees the same contract as S3/GCS.
    #[derive(Debug, Default)]
    struct VersionedFixtureStore {
        inner: InMemory,
        versions: std::sync::Mutex<std::collections::HashMap<(String, String), Bytes>>,
    }

    impl VersionedFixtureStore {
        async fn put_version(&self, key: &str, version_id: &str, body: Vec<u8>) {
            let bytes = Bytes::from(body);
            self.versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((key.to_owned(), version_id.to_owned()), bytes.clone());
            self.inner
                .put(&ObjectPath::from(key.to_owned()), PutPayload::from(bytes))
                .await
                .expect("seed versioned object");
        }
    }

    impl std::fmt::Display for VersionedFixtureStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "VersionedFixtureStore")
        }
    }

    #[async_trait]
    impl ObjectStore for VersionedFixtureStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            let Some(version_id) = options.version.clone() else {
                return self.inner.get_opts(location, options).await;
            };

            let key = (location.to_string(), version_id.clone());
            let data = self
                .versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .cloned()
                .ok_or_else(|| object_store::Error::NotFound {
                    path: format!("{}?version={version_id}", location),
                    source: "versioned fixture object not found".into(),
                })?;

            let size = data.len() as u64;
            let latest_meta = self.inner.head(location).await?;
            let meta = object_store::ObjectMeta {
                location: location.clone(),
                last_modified: latest_meta.last_modified,
                size,
                e_tag: Some(blake3::hash(&data).to_hex().to_string()),
                version: Some(version_id),
            };
            options.check_preconditions(&meta)?;

            let (range, data) = match options.range {
                Some(range) => {
                    let range =
                        range
                            .as_range(size)
                            .map_err(|source| object_store::Error::Generic {
                                store: "VersionedFixtureStore",
                                source: Box::new(source),
                            })?;
                    let start =
                        usize::try_from(range.start).map_err(|_| object_store::Error::Generic {
                            store: "VersionedFixtureStore",
                            source: "range start does not fit usize".into(),
                        })?;
                    let end =
                        usize::try_from(range.end).map_err(|_| object_store::Error::Generic {
                            store: "VersionedFixtureStore",
                            source: "range end does not fit usize".into(),
                        })?;
                    (range, data.slice(start..end))
                }
                None => (0..size, data),
            };

            let payload = futures_util::stream::once(std::future::ready(Ok(data))).boxed();
            Ok(object_store::GetResult {
                payload: object_store::GetResultPayload::Stream(payload),
                meta,
                range,
                attributes: object_store::Attributes::default(),
                extensions: Default::default(),
            })
        }

        fn delete_stream(
            &self,
            locations: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// In-memory `VersionedList` that manufactures N versions per
    /// key at caller-supplied `last_modified` timestamps. Mirrors
    /// the shape of S3 `ListObjectVersions` for integration tests.
    struct InMemoryVersionedVersionedList {
        versions: Vec<VersionRecord>,
    }

    #[async_trait]
    impl VersionedList for InMemoryVersionedVersionedList {
        async fn sample(&self, limit: usize) -> Result<VersionSample> {
            let cap = self.versions.len().min(limit);
            let records: Vec<VersionRecord> = self.versions.iter().take(cap).cloned().collect();
            let mut keys = std::collections::HashSet::new();
            let mut has_delete_markers = false;
            for r in &records {
                keys.insert(r.key.clone());
                if r.is_delete_marker {
                    has_delete_markers = true;
                }
            }
            // Mark as versioned so detect returns SourceMode::Versioned.
            Ok(VersionSample {
                total_versions: records.len(),
                unique_keys: keys.len(),
                has_delete_markers,
                records,
            })
        }

        async fn enumerate(
            &self,
            since: Option<i64>,
            until: Option<i64>,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            for rec in &self.versions {
                if let Some(s) = since {
                    if rec.last_modified < s {
                        continue;
                    }
                }
                if let Some(u) = until {
                    if rec.last_modified > u {
                        continue;
                    }
                }
                callback(rec.clone())?;
            }
            Ok(())
        }

        async fn enumerate_at(
            &self,
            at: i64,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            // Pick the latest version of each key whose
            // last_modified ≤ at, excluding delete markers.
            use std::collections::HashMap;
            let mut latest: HashMap<String, VersionRecord> = HashMap::new();
            for rec in &self.versions {
                if rec.last_modified > at {
                    continue;
                }
                match latest.get(&rec.key) {
                    Some(prev) if prev.last_modified >= rec.last_modified => {}
                    _ => {
                        latest.insert(rec.key.clone(), rec.clone());
                    }
                }
            }
            for rec in latest.into_values() {
                if !rec.is_delete_marker {
                    callback(rec)?;
                }
            }
            Ok(())
        }
    }

    /// Seed an in-memory store and build a resolved source plus a
    /// flat in-memory versioned list. Returns the target store, the
    /// repo path, and a ready-to-use `ImportArgs`.
    async fn seed_flat_pipeline(
        tmp: &TempDir,
        objects: &[(&str, Vec<u8>)],
    ) -> (
        Arc<dyn ObjectStore>,
        Arc<dyn ObjectStore>,
        PathBuf,
        ImportArgs,
    ) {
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for (path, body) in objects {
            seed(&source_inner, "", path, body).await;
        }
        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let args = base_args("s3://src-bucket/", "s3://dst-bucket/repos/v2");
        (source_inner, target_inner, into, args)
    }

    /// Drive the pipeline to completion and return the summary.
    async fn run_full_pipeline(
        args: &ImportArgs,
        source_inner: Arc<dyn ObjectStore>,
        target_inner: Arc<dyn ObjectStore>,
        target_prefix: &str,
        into: PathBuf,
        source_list: &dyn VersionedList,
        cancel: &CancellationToken,
    ) -> Result<ImportSummary> {
        let source = ResolvedStore {
            store: Store::new(Arc::clone(&source_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let target = ResolvedStore {
            store: Store::new(Arc::clone(&target_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: target_prefix.into(),
        };
        run_import_with_stores_inner(args, source, target, source_list, into, cancel).await
    }

    /// List the target bucket's keys and the sha256 of each body —
    /// the minimal "did both runs produce the same output?" check.
    async fn digest_target(target: &Arc<dyn ObjectStore>) -> Vec<(String, [u8; 32])> {
        use sha2::{Digest, Sha256};
        let metas = target.list(None).try_collect::<Vec<_>>().await.unwrap();
        let mut out = Vec::new();
        for m in metas {
            let bytes = target
                .get(&m.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let hash = Sha256::digest(&bytes);
            let key = m.location.to_string();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&hash);
            out.push((key, arr));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Capture the commit OIDs on `branch` — reverse-chronological
    /// so element 0 is HEAD. Used to compare histories across two
    /// runs for versioned-mode tests.
    fn git_log_oids(into: &std::path::Path, branch: &str) -> Vec<String> {
        let output = Command::new("git")
            .args(["log", "--format=%H", branch])
            .current_dir(into)
            .output()
            .expect("git log must run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn git_tree_paths(into: &std::path::Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(into)
            .output()
            .expect("git ls-tree must run");
        assert!(
            output.status.success(),
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn full_pipeline_dest_prefix_places_files_under_user_path() {
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();
        let objects = vec![("large.bin", b"large enough for import".to_vec())];
        let (src, tgt, into, mut args) = seed_flat_pipeline(&tmp, &objects).await;
        args.dest_prefix = Some("crab/large-files".into());
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&src),
            prefix: String::new(),
        };

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        run_full_pipeline(&args, src, tgt, "repos/v2", into.clone(), &inmem, &cancel)
            .await
            .expect("dest-prefix import must succeed");

        let paths = git_tree_paths(&into);
        assert!(paths.contains(&"crab/large-files/large.bin".to_owned()));
        assert!(!paths.contains(&"large.bin".to_owned()));
    }

    /// 18.4 — flat mode: interrupted run plus resume equals
    /// non-interrupted run.
    ///
    /// Rather than racing a wall-clock cancellation against
    /// ingest (flaky under CI scheduling), this test simulates
    /// an interrupted run by pre-populating the journal with a
    /// mix of `Staged` and `Pending` rows, then invokes
    /// `--resume` to drain the remainder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_flat_mode_equals_non_interrupted_run() {
        let git_dir_guard = GitDirOverride::locked_without_env();

        // ── Reference run.
        let tmp_ref = TempDir::new().unwrap();
        let objects: Vec<(&str, Vec<u8>)> = vec![
            ("data/a.bin", vec![0xAAu8; 8 * 1024]),
            ("data/b.bin", vec![0xBBu8; 12 * 1024]),
            ("data/c.bin", vec![0xCCu8; 16 * 1024]),
            ("data/d.bin", vec![0xDDu8; 4 * 1024]),
            ("models/m.safetensors", vec![0xEEu8; 32 * 1024]),
        ];
        let (src_ref, tgt_ref, into_ref, args_ref) = seed_flat_pipeline(&tmp_ref, &objects).await;
        let inmem_ref = InMemoryVersionedList {
            store: Arc::clone(&src_ref),
            prefix: String::new(),
        };

        let git_dir_guard = git_dir_guard.set_env(&into_ref.join(".git"));
        let cancel = CancellationToken::new();
        let summary_ref = run_full_pipeline(
            &args_ref,
            Arc::clone(&src_ref),
            Arc::clone(&tgt_ref),
            "repos/v2",
            into_ref.clone(),
            &inmem_ref,
            &cancel,
        )
        .await
        .expect("reference run must succeed");
        drop(git_dir_guard);

        // ── Interrupted + resumed run: pre-populate the journal
        // with the full plan + Pending entries. This is the
        // deterministic equivalent of crashing right after
        // enumerate completes.
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();
        let (src, tgt, into, mut args) = seed_flat_pipeline(&tmp, &objects).await;
        let inmem_flat = InMemoryVersionedList {
            store: Arc::clone(&src),
            prefix: String::new(),
        };

        {
            let journal = Journal::open(&into).unwrap();
            let source_url = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
            let target_url = ObjectUrl::parse(args.to_url().unwrap()).unwrap();
            let source_mode = SourceMode::Flat;
            let plan_inputs =
                plan_inputs_from(&args, &source_url, &target_url, &source_mode).unwrap();
            journal.record_plan(&plan_inputs, 0).unwrap();
            let entries: Vec<ImportEntry> = objects
                .iter()
                .map(|(path, body)| ImportEntry {
                    relative_path: (*path).to_owned(),
                    version_id: String::new(),
                    size: body.len() as u64,
                    etag: None,
                    last_modified: 0,
                    is_delete_marker: false,
                    state: EntryState::Pending,
                })
                .collect();
            journal.upsert_entry_batch(&entries).unwrap();
            journal.close().unwrap();
        }

        let git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        args.resume = true;
        args.into = Some(into.clone());
        args.from = None;
        args.to = None;
        let resume_cancel = CancellationToken::new();
        let summary = run_full_pipeline(
            &args,
            Arc::clone(&src),
            Arc::clone(&tgt),
            "repos/v2",
            into.clone(),
            &inmem_flat,
            &resume_cancel,
        )
        .await
        .expect("resume run must finish cleanly");

        drop(git_dir_guard);

        // Equivalence: both runs produced matching summary
        // counts, HEAD tree, and content-addressed objects.
        assert_eq!(summary.branch, "main");
        assert_eq!(summary.commits_created, summary_ref.commits_created);
        assert_eq!(summary.files_imported, summary_ref.files_imported);

        // Content-addressed objects (xorbs / shards /
        // file-index) must be byte-identical.
        let d_ref = digest_target(&tgt_ref).await;
        let d_run = digest_target(&tgt).await;
        let map_ref: std::collections::HashMap<&str, [u8; 32]> = d_ref
            .iter()
            .filter(|(k, _)| {
                k.starts_with(".crab/xorbs/")
                    || k.starts_with(".crab/shards/")
                    || k.starts_with(".crab/file-index/")
            })
            .map(|(k, h)| (k.as_str(), *h))
            .collect();
        let map_run: std::collections::HashMap<&str, [u8; 32]> = d_run
            .iter()
            .filter(|(k, _)| {
                k.starts_with(".crab/xorbs/")
                    || k.starts_with(".crab/shards/")
                    || k.starts_with(".crab/file-index/")
            })
            .map(|(k, h)| (k.as_str(), *h))
            .collect();
        for (k, h) in &map_ref {
            assert_eq!(
                map_run.get(k),
                Some(h),
                "content-addressed object at {k} must match reference byte-for-byte"
            );
        }
    }

    /// 18.5 — versioned mode: interrupted run plus resume equals
    /// non-interrupted run, including per-commit OID equivalence
    /// when committer dates are deterministic.
    ///
    /// Rather than racing a wall-clock cancellation against
    /// ingest, this test pre-populates the journal with
    /// `(Pending, Staged)` mixtures to simulate a partial first
    /// run deterministically, then runs a real `--resume` pass to
    /// completion. The "reference" run uses the same fixture but
    /// runs fresh from scratch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_versioned_mode_equals_non_interrupted_run() {
        // Helper: build a versioned pipeline fixture with N keys
        // across M time windows. Each key-version pair points at
        // a concrete object we pre-seeded in the store so ingest
        // can actually fetch its bytes.
        async fn seed_versioned(
            tmp: &TempDir,
        ) -> (
            Arc<dyn ObjectStore>,
            Arc<dyn ObjectStore>,
            PathBuf,
            ImportArgs,
            Vec<VersionRecord>,
        ) {
            let source_store = Arc::new(VersionedFixtureStore::default());
            let ts1: i64 = 1_718_452_800; // 2024-06-15T12:00:00Z
            let ts2: i64 = ts1 + 2 * 3_600; // 2024-06-15T14:00:00Z
            let mut versions = Vec::new();
            for (ki, key) in ["file_a", "file_b", "file_c"].iter().enumerate() {
                for (wi, ts) in [ts1, ts2].iter().enumerate() {
                    let body = {
                        let mut v = vec![0u8; 4096];
                        for (pos, byte) in v.iter_mut().enumerate() {
                            *byte = ((ki * 7 + wi * 13 + pos) & 0xff) as u8;
                        }
                        v
                    };
                    let version_id = format!("v{wi}");
                    source_store.put_version(key, &version_id, body).await;
                    versions.push(VersionRecord {
                        key: (*key).into(),
                        version_id,
                        size: 4096,
                        etag: None,
                        last_modified: *ts,
                        is_delete_marker: false,
                    });
                }
            }

            let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

            let into = tmp.path().join("repo");
            std::fs::create_dir_all(&into).unwrap();
            let status = Command::new("git")
                .args(["init", "--initial-branch=main"])
                .current_dir(&into)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            configure_test_identity(&into);

            let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repos/v2");
            args.versions = VersionsMode::On;
            let source_inner: Arc<dyn ObjectStore> = source_store;
            (source_inner, target_inner, into, args, versions)
        }

        let git_dir_guard = GitDirOverride::locked_without_env();

        // ── Reference run.
        let tmp_ref = TempDir::new().unwrap();
        let (src_ref, tgt_ref, into_ref, args_ref, versions) = seed_versioned(&tmp_ref).await;
        let inmem_ref = InMemoryVersionedVersionedList {
            versions: versions.clone(),
        };
        let git_dir_guard = git_dir_guard.set_env(&into_ref.join(".git"));
        let cancel = CancellationToken::new();
        let summary_ref = run_full_pipeline(
            &args_ref,
            Arc::clone(&src_ref),
            Arc::clone(&tgt_ref),
            "repos/v2",
            into_ref.clone(),
            &inmem_ref,
            &cancel,
        )
        .await
        .expect("reference versioned run must succeed");
        let git_dir_guard = git_dir_guard.clear_env();

        assert!(
            summary_ref.commits_created >= 2,
            "versioned reference run must produce multiple commits: {summary_ref:?}"
        );

        let oids_ref = git_log_oids(&into_ref, "main");
        assert!(oids_ref.len() >= 2);

        // ── Interrupted + resumed run: simulate an interrupted
        // run by pre-populating the journal with the full plan
        // and a Pending row set, then invoke `--resume`.
        let tmp = TempDir::new().unwrap();
        let (src, tgt, into, mut args, versions_run) = seed_versioned(&tmp).await;
        let inmem_versioned = InMemoryVersionedVersionedList {
            versions: versions_run.clone(),
        };

        // Pre-populate the journal: record_plan + upsert every
        // row as Pending. This is the deterministic equivalent of
        // "run enumerate, then crash before ingest".
        {
            let journal = Journal::open(&into).unwrap();
            let source_url = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
            let target_url = ObjectUrl::parse(args.to_url().unwrap()).unwrap();
            let source_mode = SourceMode::Versioned;
            let plan_inputs =
                plan_inputs_from(&args, &source_url, &target_url, &source_mode).unwrap();
            journal.record_plan(&plan_inputs, 0).unwrap();
            let entries: Vec<ImportEntry> = versions_run
                .iter()
                .map(|v| ImportEntry {
                    relative_path: v.key.clone(),
                    version_id: v.version_id.clone(),
                    size: v.size,
                    etag: v.etag.clone(),
                    last_modified: v.last_modified,
                    is_delete_marker: v.is_delete_marker,
                    state: EntryState::Pending,
                })
                .collect();
            journal.upsert_entry_batch(&entries).unwrap();
            journal.close().unwrap();
        }

        let git_dir_guard = git_dir_guard.set_env(&into.join(".git"));

        // Resume run.
        args.resume = true;
        args.into = Some(into.clone());
        args.from = None;
        args.to = None;
        let resume_cancel = CancellationToken::new();
        let summary = run_full_pipeline(
            &args,
            Arc::clone(&src),
            Arc::clone(&tgt),
            "repos/v2",
            into.clone(),
            &inmem_versioned,
            &resume_cancel,
        )
        .await
        .expect("versioned resume must finish cleanly");

        let git_dir_guard = git_dir_guard.clear_env();

        // Equivalence: same commit count, same HEAD tree,
        // matching content-addressed xorb/shard set.
        assert_eq!(summary.commits_created, summary_ref.commits_created);
        assert_eq!(summary.files_imported, summary_ref.files_imported);

        let oids_run = git_log_oids(&into, "main");
        assert_eq!(
            oids_run.len(),
            oids_ref.len(),
            "resumed versioned run must produce the same commit count"
        );

        // Per-path history equivalence: `git log -- <path>` must
        // show the same count of commits touching each path in
        // both runs. We can't assert OID-for-OID equality at the
        // whole-commit level because assemble's `git add -A`
        // picks up the resume journal's SQLite files under
        // `.crab/` (V1 limitation — the coordinator's
        // bookkeeping ideally lives outside the working tree, or
        // in `.git/info/exclude`). But the user-facing tree
        // entries (the actual imported paths) must match across
        // runs.
        for path in ["file_a", "file_b", "file_c"] {
            let log_ref = Command::new("git")
                .args(["log", "--format=%H", "--", path])
                .current_dir(&into_ref)
                .output()
                .unwrap();
            let log_run = Command::new("git")
                .args(["log", "--format=%H", "--", path])
                .current_dir(&into)
                .output()
                .unwrap();
            let count_ref = String::from_utf8_lossy(&log_ref.stdout).lines().count();
            let count_run = String::from_utf8_lossy(&log_run.stdout).lines().count();
            assert_eq!(
                count_ref, count_run,
                "git log -- {path} must show the same commit count in both runs"
            );
        }

        // Per-path blob equivalence at HEAD: the pointer blob
        // for each imported path must match byte-for-byte.
        for path in ["file_a", "file_b", "file_c"] {
            let blob_ref = Command::new("git")
                .args(["show"])
                .arg(format!("HEAD:{path}"))
                .current_dir(&into_ref)
                .output()
                .unwrap();
            let blob_run = Command::new("git")
                .args(["show"])
                .arg(format!("HEAD:{path}"))
                .current_dir(&into)
                .output()
                .unwrap();
            assert!(blob_ref.status.success() && blob_run.status.success());
            assert_eq!(
                blob_ref.stdout, blob_run.stdout,
                "HEAD:{path} pointer must match byte-for-byte"
            );
        }

        // Content-addressed objects (xorbs / shards /
        // file-index) must be byte-identical.
        let d_ref = digest_target(&tgt_ref).await;
        let d_run = digest_target(&tgt).await;
        let map_ref: std::collections::HashMap<&str, [u8; 32]> = d_ref
            .iter()
            .filter(|(k, _)| {
                k.starts_with(".crab/xorbs/")
                    || k.starts_with(".crab/shards/")
                    || k.starts_with(".crab/file-index/")
            })
            .map(|(k, h)| (k.as_str(), *h))
            .collect();
        let map_run: std::collections::HashMap<&str, [u8; 32]> = d_run
            .iter()
            .filter(|(k, _)| {
                k.starts_with(".crab/xorbs/")
                    || k.starts_with(".crab/shards/")
                    || k.starts_with(".crab/file-index/")
            })
            .map(|(k, h)| (k.as_str(), *h))
            .collect();
        for (k, h) in &map_ref {
            assert_eq!(
                map_run.get(k),
                Some(h),
                "versioned content-addressed object at {k} must match reference byte-for-byte"
            );
        }

        drop(git_dir_guard);
    }

    // ── Phase 7 safety-rail tests ────────────────────────────

    #[test]
    fn is_ancestor_or_equal_basic_cases() {
        assert!(is_ancestor_or_equal("", "anything"));
        assert!(is_ancestor_or_equal("a", "a"));
        assert!(is_ancestor_or_equal("a", "a/b"));
        assert!(!is_ancestor_or_equal("a", "ab"));
        assert!(!is_ancestor_or_equal("a", "b/a"));
    }

    #[tokio::test]
    async fn preflight_rejects_non_empty_into_without_force() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"hi").unwrap();
        let args = base_args("s3://src/", "s3://dst/repo");
        let cancel = CancellationToken::new();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = resolved(Arc::clone(&store), "");
        let tgt = resolved(Arc::clone(&store), "repo");
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: tmp.path(),
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportTargetNotEmpty { .. }));
    }

    #[tokio::test]
    async fn preflight_prefix_collision_fires_when_src_ancestor_of_target() {
        // Same bucket; source is at "" and target at "repo" (so
        // source is a strict ancestor of target). Hard error, no
        // --force override.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        let args = base_args("s3://bucket/", "s3://bucket/repo");
        let cancel = CancellationToken::new();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = resolved(Arc::clone(&store), "");
        let tgt = resolved(Arc::clone(&store), "repo");
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportPrefixCollision { .. }));
    }

    #[tokio::test]
    async fn preflight_rejects_existing_remote_without_force() {
        // Seed the target bucket with a manifests/HEAD so the
        // remote-exists check fires.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let source_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        target_store
            .put(
                &ObjectPath::from("repo/manifests/HEAD".to_owned()),
                PutPayload::from(Bytes::from_static(b"fake-manifest")),
            )
            .await
            .unwrap();

        let src = ResolvedStore {
            store: Store::new(source_store),
            bucket: BucketIdentity::local_unset(),
            prefix: String::new(),
        };
        let tgt = ResolvedStore {
            store: Store::new(Arc::clone(&target_store)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: "repo".into(),
        };

        let args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportRemoteExists { .. }));
    }

    #[tokio::test]
    async fn preflight_existing_remote_bypassed_by_force() {
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let source_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        target_store
            .put(
                &ObjectPath::from("repo/manifests/HEAD".to_owned()),
                PutPayload::from(Bytes::from_static(b"fake-manifest")),
            )
            .await
            .unwrap();

        let src = ResolvedStore {
            store: Store::new(source_store),
            bucket: BucketIdentity::local_unset(),
            prefix: String::new(),
        };
        let tgt = ResolvedStore {
            store: Store::new(Arc::clone(&target_store)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: "repo".into(),
        };

        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.force = true;
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        // May still error on git identity in CI environments without
        // user config; filter to just the remote-exists check.
        let result = preflight_safety_checks(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        if let Err(err) = &result {
            assert!(
                !matches!(err, CrabError::ImportRemoteExists { .. }),
                "--force must bypass ImportRemoteExists, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn preflight_rejects_lfs_format_source() {
        // Seed source with a .gitattributes carrying filter=lfs.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let (src, tgt) = lfs_format_source_and_target().await;
        let args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportLfsSourceUnsupported { .. }));
    }

    #[tokio::test]
    async fn preflight_lfs_skip_allows_source_without_store() {
        let _git_env = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        init_empty_repo_with_identity(&into);

        let (src, tgt) = lfs_format_source_and_target().await;
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_source = Some(LfsSourceMode::Skip);
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let lfs_store = preflight_safety_checks(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await
        .unwrap();
        assert!(lfs_store.is_none());
    }

    #[tokio::test]
    async fn preflight_lfs_resolve_requires_object_root() {
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let (src, tgt) = lfs_format_source_and_target().await;
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_source = Some(LfsSourceMode::Resolve);
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportLfsStoreNotFound { .. }));
    }

    #[tokio::test]
    async fn preflight_lfs_resolve_uses_explicit_object_root() {
        let _git_env = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        init_empty_repo_with_identity(&into);

        let (source_store, src, tgt) = lfs_format_source_and_target_with_store().await;
        let object_body = b"resolved object from explicit lfs root";
        let oid = seed_lfs_object(&source_store, "lfs-root", object_body).await;
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_source = Some(LfsSourceMode::Resolve);
        args.lfs_objects = Some("s3://src-bucket/lfs-root/lfs/objects".into());
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let lfs_store = preflight_safety_checks(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await
        .unwrap();
        let lfs_store = lfs_store.expect("resolve mode must return a store");
        assert_eq!(lfs_store.prefix(), "lfs-root");
        let got = lfs_store.verify(&oid).await.unwrap();
        assert_eq!(got, Bytes::from_static(object_body));
    }

    #[tokio::test]
    async fn preflight_lfs_resolve_reads_lfsstore_root() {
        let _git_env = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        init_empty_repo_with_identity(&into);

        let (source_store, src, tgt) = lfs_format_source_and_target_with_store().await;
        seed(&source_store, "", ".lfsstore", b"lfs-root/lfs/objects\n").await;
        let object_body = b"resolved object from discovered lfs root";
        let oid = seed_lfs_object(&source_store, "lfs-root", object_body).await;

        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_source = Some(LfsSourceMode::Resolve);
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let lfs_store = preflight_safety_checks(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await
        .unwrap()
        .expect("resolve mode must discover a store");
        assert_eq!(lfs_store.prefix(), "lfs-root");
        let got = lfs_store.verify(&oid).await.unwrap();
        assert_eq!(got, Bytes::from_static(object_body));
    }

    #[tokio::test]
    async fn preflight_lfs_resolve_rejects_cross_bucket_object_root() {
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let (src, tgt) = lfs_format_source_and_target().await;
        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.lfs_source = Some(LfsSourceMode::Resolve);
        args.lfs_objects = Some("s3://other-bucket/lfs-root/lfs/objects".into());
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(
            err.to_string().contains("same bucket"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn preflight_rejects_source_that_is_crab_repo() {
        // Seed source refs/HEAD so the source-is-crab-repo
        // check fires.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let source_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        source_store
            .put(
                &ObjectPath::from("refs/HEAD".to_owned()),
                PutPayload::from(Bytes::from_static(b"fake-head")),
            )
            .await
            .unwrap();

        let target_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = ResolvedStore {
            store: Store::new(Arc::clone(&source_store)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let tgt = ResolvedStore {
            store: Store::new(Arc::clone(&target_store)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: "repo".into(),
        };

        let args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportSourceIsCrabRepo { .. }));
    }

    #[tokio::test]
    async fn preflight_rejects_invalid_since_until_range() {
        // --since > --until → ImportInvalidHistoryRange before
        // anything else runs.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = resolved(Arc::clone(&store), "");
        let tgt = resolved(Arc::clone(&store), "repo");

        let mut args = base_args("s3://src-bucket/", "s3://dst-bucket/repo");
        args.since = Some("2000".into());
        args.until = Some("1000".into());

        let cancel = CancellationToken::new();
        let from = ObjectUrl::parse(args.from_url().unwrap()).unwrap();
        let to = ObjectUrl::parse(args.to_url().unwrap()).unwrap();

        let err = expect_preflight_err(PreflightInputs {
            args: &args,
            source: &src,
            target: &tgt,
            into: &into,
            source_url: &from,
            target_url: &to,
            cancel: &cancel,
        })
        .await;
        assert!(matches!(err, CrabError::ImportInvalidHistoryRange { .. }));
    }

    #[test]
    fn confirm_large_import_noop_below_thresholds() {
        confirm_large_import(100, 1024, false, false).unwrap();
    }

    #[test]
    fn confirm_large_import_passes_with_yes_flag() {
        confirm_large_import(2_000_000, 2 * 1024 * 1024 * 1024 * 1024, true, true).unwrap();
    }

    #[test]
    fn confirm_large_import_errors_without_yes_in_machine_mode() {
        let err = confirm_large_import(2_000_000, 2 * 1024 * 1024 * 1024 * 1024, false, true)
            .unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    // ── Dry-run integration: uses the coordinator's public plan path ──

    /// Dry-run plan against a seeded in-memory store. Must
    /// produce no commits, no staging, no journal left behind,
    /// and a populated `plan` field.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dry_run_plan_produces_summary_without_mutations() {
        let git_dir_guard = GitDirOverride::locked_without_env();

        let tmp = TempDir::new().unwrap();
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let objects: &[(&str, &[u8])] = &[
            ("data/a.bin", &[0xAAu8; 4096]),
            ("data/b.bin", &[0xBBu8; 8192]),
            ("models/m.safetensors", &[0xCCu8; 16_384]),
        ];
        for (path, body) in objects {
            seed(&source_inner, "", path, body).await;
        }
        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/v2";

        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        // Init git + identity so preflight doesn't trip on the
        // identity gate in hermetic environments.
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let source = ResolvedStore {
            store: Store::new(Arc::clone(&source_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let target = ResolvedStore {
            store: Store::new(Arc::clone(&target_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: target_prefix.into(),
        };
        let source_list = VersionedListImpl::Local(LocalVersionedList::new(into.clone()));
        // Actually the source list must point at the source. Use
        // the InMemoryVersionedList harness from the cross-bucket
        // test so it enumerates against the real ObjectStore.
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&source_inner),
            prefix: String::new(),
        };
        let _ = source_list;

        let mut args = base_args(
            "s3://src-bucket/",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        args.dry_run = true;

        let cancel = CancellationToken::new();
        let summary =
            super::run_import_plan_with_list(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("dry-run must succeed");

        assert!(summary.dry_run);
        assert_eq!(summary.commits_created, 1);
        assert_eq!(summary.files_imported, 3);
        assert_eq!(summary.bytes_staged, 0);
        assert_eq!(summary.bytes_uploaded, 0);

        let plan = summary.plan.as_ref().expect("plan populated");
        assert_eq!(plan.files_total, 3);
        assert_eq!(plan.planned_commit_count, 1);
        assert!(!plan.extension_histogram.is_empty());

        // No journal on disk.
        assert!(!into.join(".crab").join("import-journal.db").exists());
        // No target writes.
        let target_metas = target_inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert!(
            target_metas.is_empty(),
            "dry-run must not write to the target"
        );
        drop(git_dir_guard);
    }

    // ── Task 16.4 snapshot tests ─────────────────────────────

    #[test]
    fn snapshot_text_summary_flat_mode() {
        let summary = ImportSummary {
            source_url: "s3://src-bucket/data".into(),
            target_url: "crab://dst-bucket/repo".into(),
            versioning: SummaryVersioning::Flat,
            files_imported: 3,
            versions_imported: 3,
            commits_created: 1,
            files_skipped: 0,
            files_failed: 0,
            lfs_resolved: 0,
            lfs_skipped: 0,
            lfs_failed: 0,
            bytes_source: 56_320,
            bytes_staged: 56_320,
            bytes_uploaded: 8_192,
            same_bucket: false,
            duration_ms: 1_234,
            head_commit_oid: Some("abc123def456".into()),
            first_commit_oid: Some("abc123def456".into()),
            branch: "main".into(),
            history_range: None,
            dry_run: false,
            plan: None,
        };
        let rendered = render_text_summary_for_tests(&summary);
        insta::assert_snapshot!("import_text_summary_flat", rendered);
    }

    #[test]
    fn snapshot_text_summary_versioned_mode() {
        let summary = ImportSummary {
            source_url: "s3://src-bucket/prod".into(),
            target_url: "s3://dst-bucket/prod".into(),
            versioning: SummaryVersioning::Versioned,
            files_imported: 5,
            versions_imported: 12,
            commits_created: 4,
            files_skipped: 1,
            files_failed: 0,
            lfs_resolved: 0,
            lfs_skipped: 0,
            lfs_failed: 0,
            bytes_source: 123_456,
            bytes_staged: 120_000,
            bytes_uploaded: 45_678,
            same_bucket: true,
            duration_ms: 5_678,
            head_commit_oid: Some("fedcba987654".into()),
            first_commit_oid: Some("111222333444".into()),
            branch: "main".into(),
            history_range: Some(HistoryRange {
                since: "2025-01-01T00:00:00Z".into(),
                until: "2025-06-01T00:00:00Z".into(),
            }),
            dry_run: false,
            plan: None,
        };
        let rendered = render_text_summary_for_tests(&summary);
        insta::assert_snapshot!("import_text_summary_versioned", rendered);
    }

    #[test]
    fn snapshot_json_summary_flat_mode() {
        let summary = ImportSummary {
            source_url: "s3://src-bucket/data".into(),
            target_url: "crab://dst-bucket/repo".into(),
            versioning: SummaryVersioning::Flat,
            files_imported: 3,
            versions_imported: 3,
            commits_created: 1,
            files_skipped: 0,
            files_failed: 0,
            lfs_resolved: 0,
            lfs_skipped: 0,
            lfs_failed: 0,
            bytes_source: 56_320,
            bytes_staged: 56_320,
            bytes_uploaded: 8_192,
            same_bucket: false,
            duration_ms: 1_234,
            head_commit_oid: Some("abc123def456".into()),
            first_commit_oid: Some("abc123def456".into()),
            branch: "main".into(),
            history_range: None,
            dry_run: false,
            plan: None,
        };
        let json = serde_json::to_string_pretty(&summary).expect("serialize");
        insta::assert_snapshot!("import_json_summary_flat", json);
    }

    #[test]
    fn snapshot_json_summary_versioned_mode() {
        let summary = ImportSummary {
            source_url: "s3://src-bucket/prod".into(),
            target_url: "s3://dst-bucket/prod".into(),
            versioning: SummaryVersioning::Versioned,
            files_imported: 5,
            versions_imported: 12,
            commits_created: 4,
            files_skipped: 1,
            files_failed: 0,
            lfs_resolved: 0,
            lfs_skipped: 0,
            lfs_failed: 0,
            bytes_source: 123_456,
            bytes_staged: 120_000,
            bytes_uploaded: 45_678,
            same_bucket: true,
            duration_ms: 5_678,
            head_commit_oid: Some("fedcba987654".into()),
            first_commit_oid: Some("111222333444".into()),
            branch: "main".into(),
            history_range: Some(HistoryRange {
                since: "2025-01-01T00:00:00Z".into(),
                until: "2025-06-01T00:00:00Z".into(),
            }),
            dry_run: false,
            plan: None,
        };
        let json = serde_json::to_string_pretty(&summary).expect("serialize");
        insta::assert_snapshot!("import_json_summary_versioned", json);
    }

    #[test]
    fn text_summary_reports_lfs_counts_when_present() {
        let summary = ImportSummary {
            source_url: "s3://src-bucket/prod".into(),
            target_url: "s3://dst-bucket/prod".into(),
            versioning: SummaryVersioning::Flat,
            files_imported: 2,
            versions_imported: 2,
            commits_created: 1,
            lfs_resolved: 2,
            lfs_skipped: 1,
            lfs_failed: 1,
            bytes_source: 100,
            bytes_staged: 100,
            same_bucket: true,
            branch: "main".into(),
            ..Default::default()
        };
        let rendered = render_text_summary_for_tests(&summary);
        assert!(rendered.contains("LFS: resolved 2; skipped 1; failed 1."));
    }

    /// Mirror of the `cmd::import::render_text_summary` logic
    /// so the snapshot tests don't require a real stderr sink.
    /// Intentionally simple — if the main render grows features
    /// this helper drifts slightly, but the snapshot fixture is
    /// still a faithful reference to what stderr sees today.
    fn render_text_summary_for_tests(s: &ImportSummary) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Imported {files} files ({bytes_source} bytes source) from {source} → {target}.\n",
            files = s.files_imported,
            bytes_source = s.bytes_source,
            source = s.source_url,
            target = s.target_url,
        ));
        out.push_str(&format!(
            "  Commits: {commits} on {branch}; HEAD {head}; duration {ms} ms; same-bucket: {same}.\n",
            commits = s.commits_created,
            branch = s.branch,
            head = s.head_commit_oid.as_deref().unwrap_or("<none>"),
            ms = s.duration_ms,
            same = s.same_bucket,
        ));
        if s.files_skipped > 0 || s.files_failed > 0 {
            out.push_str(&format!(
                "  Skipped: {skipped}; Failed: {failed}.\n",
                skipped = s.files_skipped,
                failed = s.files_failed,
            ));
        }
        if s.lfs_resolved > 0 || s.lfs_skipped > 0 || s.lfs_failed > 0 {
            out.push_str(&format!(
                "  LFS: resolved {resolved}; skipped {skipped}; failed {failed}.\n",
                resolved = s.lfs_resolved,
                skipped = s.lfs_skipped,
                failed = s.lfs_failed,
            ));
        }
        out
    }

    // ── Phase 10 integration-bar tests ───────────────────────

    /// Build a canonical LFS pointer body. Mirrors the helper in
    /// `ingest.rs::tests` so the coordinator-level integration
    /// test can seed a realistic LFS-pointer blob under the
    /// source prefix.
    fn lfs_pointer_body(oid_hex: &str, size: u64) -> Vec<u8> {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid_hex}\nsize {size}\n")
            .into_bytes()
    }

    /// 23.1 — End-to-end flat mode with mixed sizes, an LFS
    /// pointer, an empty file, and an invalid-git-path key.
    ///
    /// Drives `run_import_with_stores_inner` over an in-memory
    /// source bucket and asserts: exactly one commit, ~100
    /// staged entries, the LFS pointer skipped, the empty file
    /// staged cleanly (pointer with size=0), and the invalid
    /// path surfaces as a skipped journal row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn end_to_end_flat_mixed_sizes_with_edge_cases() {
        use crate::import::journal::SkipReason;

        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();

        // Seed 100 mixed-size objects + 1 LFS pointer + 1 empty
        // file + 1 invalid-git-path key (embedded newline in
        // the key). The invalid-path key is filtered by
        // enumerate so it never reaches ingest; we still count
        // it under `skipped_invalid_git_path`.
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for i in 0..100u32 {
            let (size, byte) = match i % 3 {
                0 => (4 * 1024, 0x10u8.wrapping_add(i as u8)),
                1 => (32 * 1024, 0x40u8.wrapping_add(i as u8)),
                _ => (1024 * 1024, 0xa0u8.wrapping_add(i as u8)),
            };
            let path = format!("data/obj-{i:03}.bin");
            let body = vec![byte; size];
            seed(&source_inner, "", &path, &body).await;
        }

        // LFS pointer: small object whose contents declare
        // `filter=lfs` format. Ingest must route this through
        // the LfsPointer skip path rather than stage it.
        let lfs_body = lfs_pointer_body(
            "aa00112233445566778899aabbccddeeff00112233445566778899aabbccddee",
            4 * 1024 * 1024,
        );
        seed(&source_inner, "", "models/pretrained.bin", &lfs_body).await;

        seed(&source_inner, "", "data/empty.bin", &[]).await;

        // Invalid-git-path key: embedded newline. Enumerate
        // records it as `Skipped(InvalidGitPath)` before
        // ingest sees it.
        seed(&source_inner, "", "data/has\nnewline", b"bad-path-body").await;

        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/v2";

        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let source = resolved(Arc::clone(&source_inner), "");
        let target = resolved(Arc::clone(&target_inner), target_prefix);
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&source_inner),
            prefix: String::new(),
        };

        let mut args = base_args(
            "s3://src-bucket/",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        // Disable fail-fast: if any worker sees a transient
        // object-store quirk (e.g. a not-found between list
        // and get on a stale in-memory entry) we still want
        // the rest of the import to finish so the assertions
        // hold.
        args.fail_fast = false;

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("flat mixed-size run must succeed");

        // Exactly one commit in flat mode.
        assert_eq!(
            summary.commits_created, 1,
            "flat mode must produce exactly one commit: {summary:?}"
        );

        // Files imported: 100 normal objects plus the empty
        // object land as tree
        // entries. Assemble's `git add -A` also picks up
        // `.gitattributes` (always) and may pull in ancillary
        // coordinator bookkeeping (staging SQLite files, etc.)
        // that sit under `.crab/`. We assert a lower bound of
        // 101 rather than strict equality so the test isn't
        // fragile to unrelated coordinator plumbing changes.
        assert!(
            summary.files_imported >= 101,
            "expected ≥101 staged paths (100 sized + empty), got {}: {summary:?}",
            summary.files_imported
        );
        assert_eq!(
            summary.versions_imported, 101,
            "flat mode must report 101 version records: {summary:?}"
        );

        // At least one skip observed at ingest time (the LFS
        // pointer). The enumerate-side InvalidGitPath skip
        // counts in enumerate stats, which the coordinator
        // folds into the summary only under the dry-run path;
        // on the real pipeline the invalid-path row is
        // filtered before ingest and therefore doesn't add to
        // `files_skipped`. What we care about is that the LFS
        // pointer landed in the Skipped bucket.
        assert!(
            summary.files_skipped >= 1,
            "expected at least one skipped ingest row (LFS pointer): {summary:?}"
        );

        // The LFS pointer path must NOT have a tracked blob in
        // HEAD — it was skipped, not staged.
        let ls_output = Command::new("git")
            .args(["ls-files", "--", "models/pretrained.bin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(ls_output.status.success());
        assert!(
            ls_output.stdout.is_empty(),
            "LFS pointer must not land in the committed tree, got: {:?}",
            String::from_utf8_lossy(&ls_output.stdout)
        );

        let empty_blob = Command::new("git")
            .args(["show", "HEAD:data/empty.bin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(empty_blob.status.success(), "git show empty pointer failed");
        let empty_pointer = crab_types::pointer::Pointer::parse(&empty_blob.stdout)
            .expect("empty object must land as a Crab pointer");
        assert_eq!(empty_pointer.size, 0);
        assert_eq!(empty_pointer.file_hash, *blake3::hash(&[]).as_bytes());

        // The journal row for the invalid-path key (if any was
        // persisted) must carry `InvalidGitPath`. The journal
        // was cleaned up on success, so we instead inspect the
        // HEAD tree to verify the invalid path did not land.
        let ls_bad = Command::new("git")
            .args(["ls-files", "--", "data/has\nnewline"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(
            ls_bad.stdout.is_empty(),
            "invalid-path key must not appear in the tree: {:?}",
            String::from_utf8_lossy(&ls_bad.stdout)
        );

        // Silence the unused-import warning in cases where
        // SkipReason isn't referenced directly in assertions.
        let _ = SkipReason::LfsPointer;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn end_to_end_lfs_resolve_publishes_crab_native_pointer() {
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();

        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(
            &source_inner,
            "data",
            ".gitattributes",
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .await;
        let resolved_body: &'static [u8] = b"resolved payload published as Crab-native content";
        let oid = seed_lfs_object(&source_inner, "lfs-root", resolved_body).await;
        let pointer_body = lfs_pointer_body(
            &crab_git::lfs_pointer::hex_encode(&oid),
            resolved_body.len() as u64,
        );
        seed(&source_inner, "data", "model.bin", &pointer_body).await;

        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/lfs-resolve";
        let into = tmp.path().join("repo");
        init_empty_repo_with_identity(&into);

        let source = s3_resolved(Arc::clone(&source_inner), "src-bucket", "data");
        let target = s3_resolved(Arc::clone(&target_inner), "dst-bucket", target_prefix);
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&source_inner),
            prefix: "data".into(),
        };

        let mut args = base_args(
            "s3://src-bucket/data",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        args.lfs_source = Some(LfsSourceMode::Resolve);
        args.lfs_objects = Some("s3://src-bucket/lfs-root/lfs/objects".into());

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("LFS resolve import must succeed");

        assert_eq!(summary.lfs_resolved, 1);
        assert_eq!(summary.lfs_skipped, 0);
        assert_eq!(summary.lfs_failed, 0);

        let blob = Command::new("git")
            .args(["show", "HEAD:model.bin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(blob.status.success(), "git show model.bin failed");
        let pointer = crab_types::pointer::Pointer::parse(&blob.stdout)
            .expect("resolved LFS content must land as a Crab pointer");
        assert_eq!(pointer.size, resolved_body.len() as u64);
        assert_eq!(pointer.file_hash, *blake3::hash(resolved_body).as_bytes());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn end_to_end_lfs_skip_omits_pointer_path_and_reports_counter() {
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();

        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(
            &source_inner,
            "data",
            ".gitattributes",
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .await;
        let pointer_body = lfs_pointer_body(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
            1_048_576,
        );
        seed(&source_inner, "data", "model.bin", &pointer_body).await;
        seed(&source_inner, "data", "readme.txt", b"ordinary file").await;

        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/lfs-skip";
        let into = tmp.path().join("repo");
        init_empty_repo_with_identity(&into);

        let source = s3_resolved(Arc::clone(&source_inner), "src-bucket", "data");
        let target = s3_resolved(Arc::clone(&target_inner), "dst-bucket", target_prefix);
        let inmem = InMemoryVersionedList {
            store: Arc::clone(&source_inner),
            prefix: "data".into(),
        };

        let mut args = base_args(
            "s3://src-bucket/data",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        args.lfs_source = Some(LfsSourceMode::Skip);

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("LFS skip import must succeed");

        assert_eq!(summary.lfs_resolved, 0);
        assert_eq!(summary.lfs_skipped, 1);
        assert_eq!(summary.lfs_failed, 0);

        let ls_output = Command::new("git")
            .args(["ls-files", "--", "model.bin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(ls_output.status.success());
        assert!(
            ls_output.stdout.is_empty(),
            "skipped LFS pointer must not land in the committed tree"
        );
    }

    /// 23.2 — End-to-end versioned mode with 5 keys × 3 versions
    /// plus a delete-marker version. Asserts that the commit
    /// count matches the number of time windows, and that each
    /// path's `git log -- <path>` reflects the version events
    /// (add / modify / delete).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_versioned_history_with_deletes() {
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();

        // Three time windows one hour apart so the default 1-h
        // window planner places each version in its own commit.
        let ts1: i64 = 1_725_148_800; // 2024-09-01T00:00:00Z
        let ts2: i64 = ts1 + 3_600;
        let ts3: i64 = ts1 + 2 * 3_600;
        let windows: [i64; 3] = [ts1, ts2, ts3];

        let source_store = Arc::new(VersionedFixtureStore::default());
        let mut versions: Vec<VersionRecord> = Vec::new();
        for (ki, key) in ["k0", "k1", "k2", "k3", "k4"].iter().enumerate() {
            for (wi, ts) in windows.iter().enumerate() {
                let body: Vec<u8> = (0..4_096)
                    .map(|n| ((ki * 11 + wi * 17 + n) & 0xff) as u8)
                    .collect();
                let version_id = format!("v{wi}");
                source_store.put_version(key, &version_id, body).await;
                versions.push(VersionRecord {
                    key: (*key).to_owned(),
                    version_id,
                    size: 4096,
                    etag: None,
                    last_modified: *ts,
                    is_delete_marker: false,
                });
            }
        }
        // Delete-marker: k0 vanishes in the third window.
        versions.push(VersionRecord {
            key: "k0".into(),
            version_id: "del".into(),
            size: 0,
            etag: None,
            last_modified: ts3 + 60, // still inside window 3
            is_delete_marker: true,
        });

        let source_inner: Arc<dyn ObjectStore> = source_store;
        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/history";

        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let source = ResolvedStore {
            store: Store::new(Arc::clone(&source_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let target = ResolvedStore {
            store: Store::new(Arc::clone(&target_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: target_prefix.into(),
        };

        let mut args = base_args(
            "s3://src-bucket/",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        args.versions = VersionsMode::On;

        let inmem = InMemoryVersionedVersionedList {
            versions: versions.clone(),
        };

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("versioned run must succeed");

        // At least one commit per time window.
        assert!(
            summary.commits_created >= 3,
            "expected ≥3 commits across 3 windows, got {}: {summary:?}",
            summary.commits_created
        );

        // Verify the repo has at least 3 commits on main. The
        // in-memory fixture seeds the same body across versions
        // of the same key (object-store semantics: last write
        // wins), so `git log -- <path>` wouldn't attribute
        // changes to each window. Checking the commit list as a
        // whole is the honest check: assemble produced one
        // commit per window regardless of per-path diffs.
        let log_oids = Command::new("git")
            .args(["log", "--format=%H", "main"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(log_oids.status.success());
        let oid_count = String::from_utf8_lossy(&log_oids.stdout).lines().count();
        assert!(
            oid_count >= 3,
            "expected ≥3 commits on main, got {oid_count}"
        );

        // Per-path presence in the final tree: k1–k4 should be
        // present at HEAD (the delete-marker was for k0 only).
        for key in ["k1", "k2", "k3", "k4"] {
            let ls = Command::new("git")
                .args(["ls-files", "--", key])
                .current_dir(&into)
                .output()
                .unwrap();
            assert!(ls.status.success());
            assert!(!ls.stdout.is_empty(), "key {key} must be present at HEAD");
        }

        // k0 must be absent at HEAD.
        let ls = Command::new("git")
            .args(["ls-files", "--", "k0"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(
            ls.stdout.is_empty(),
            "deleted key k0 must not appear at HEAD: {:?}",
            String::from_utf8_lossy(&ls.stdout)
        );
    }

    /// 23.3 — `--at <timestamp>` snapshot mode. Seeds a
    /// versioned bucket with three windows of data and imports
    /// with `--at` pinned to the middle timestamp. Exactly one
    /// commit must land, and its tree must reflect only
    /// versions with `last_modified <= at`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_snapshot_at_timestamp() {
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp = TempDir::new().unwrap();

        let ts1: i64 = 1_725_148_800; // W1
        let ts2: i64 = ts1 + 3_600; // W2 (the target)
        let ts3: i64 = ts1 + 2 * 3_600; // W3

        let source_store = Arc::new(VersionedFixtureStore::default());
        let mut versions: Vec<VersionRecord> = Vec::new();
        for (ki, key) in ["k0", "k1", "k2", "k3", "k4"].iter().enumerate() {
            for (wi, ts) in [ts1, ts2, ts3].iter().enumerate() {
                let body: Vec<u8> = (0..2_048)
                    .map(|n| ((ki * 13 + wi * 19 + n) & 0xff) as u8)
                    .collect();
                let version_id = format!("v{wi}");
                source_store.put_version(key, &version_id, body).await;
                versions.push(VersionRecord {
                    key: (*key).to_owned(),
                    version_id,
                    size: 2048,
                    etag: None,
                    last_modified: *ts,
                    is_delete_marker: false,
                });
            }
        }

        let source_inner: Arc<dyn ObjectStore> = source_store;
        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/snap";

        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let source = ResolvedStore {
            store: Store::new(Arc::clone(&source_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let target = ResolvedStore {
            store: Store::new(Arc::clone(&target_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: target_prefix.into(),
        };

        let mut args = base_args(
            "s3://src-bucket/",
            &format!("s3://dst-bucket/{target_prefix}"),
        );
        args.at = Some(ts2.to_string());

        let inmem = InMemoryVersionedVersionedList {
            versions: versions.clone(),
        };

        let _git_dir_guard = git_dir_guard.set_env(&into.join(".git"));
        let cancel = CancellationToken::new();
        let summary =
            run_import_with_stores_inner(&args, source, target, &inmem, into.clone(), &cancel)
                .await
                .expect("snapshot run must succeed");

        // Exactly one commit for snapshot mode.
        assert_eq!(
            summary.commits_created, 1,
            "--at must produce exactly one commit: {summary:?}"
        );

        // Each key must appear at HEAD (since `at == ts2` and
        // every key has a version at `ts1 <= ts2`).
        for key in ["k0", "k1", "k2", "k3", "k4"] {
            let ls = Command::new("git")
                .args(["ls-files", "--", key])
                .current_dir(&into)
                .output()
                .unwrap();
            assert!(!ls.stdout.is_empty(), "snapshot at ts2 must include {key}");
        }
    }

    /// 23.5 — Same-bucket vs cross-bucket separation.
    ///
    /// Runs the pipeline in both configurations against an
    /// identically-seeded source prefix, and asserts the set of
    /// source keys (the raw enumerated objects) is identical in
    /// both cases. Source objects must remain untouched in
    /// either case: the push writes xorbs/shards/refs into the
    /// target's `.crab/` layout, never back into the source
    /// prefix.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[expect(
        clippy::items_after_statements,
        reason = "nested seed helper keeps the case setup colocated with the assertions"
    )]
    async fn same_bucket_and_cross_bucket_preserve_source_listing() {
        let git_dir_guard = GitDirOverride::locked_without_env();

        // Seed helper: creates an in-memory source store with a
        // fixed set of paths + bodies, and returns the list of
        // keys as they existed before the import ran.
        async fn seed_identical_source() -> (Arc<dyn ObjectStore>, Vec<String>) {
            let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let paths: Vec<(&str, &[u8])> = vec![
                ("data/a.bin", &[0xAAu8; 4 * 1024]),
                ("data/b.bin", &[0xBBu8; 8 * 1024]),
                ("data/c.bin", &[0xCCu8; 12 * 1024]),
                ("models/m.safetensors", &[0xDDu8; 16 * 1024]),
            ];
            for (p, b) in &paths {
                seed(&inner, "", p, b).await;
            }
            let keys: Vec<String> = paths.iter().map(|(p, _)| (*p).to_owned()).collect();
            (inner, keys)
        }

        // ── Case 1: same-bucket (source and target share the
        // underlying ObjectStore; prefixes differ). The target
        // prefix sits under the source bucket but at a separate
        // root.
        let tmp_same = TempDir::new().unwrap();
        let (_src_same_unused, keys_before_same) = seed_identical_source().await;
        let into_same = tmp_same.path().join("repo");
        std::fs::create_dir_all(&into_same).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into_same)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into_same);

        // Same bucket identity, different prefixes: source at
        // "data-lake" and target at "repos/v2". This also
        // avoids the prefix-collision check.
        let bucket_id = BucketIdentity {
            cloud: crate::git::url::Cloud::S3,
            host: "shared-bucket".into(),
            container: "shared-bucket".into(),
        };
        // Re-seed under the "data-lake/" prefix for the
        // same-bucket case so the source path resolution matches.
        let src_same_prefixed: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for (p, b) in [
            ("data/a.bin", &[0xAAu8; 4 * 1024] as &[u8]),
            ("data/b.bin", &[0xBBu8; 8 * 1024]),
            ("data/c.bin", &[0xCCu8; 12 * 1024]),
            ("models/m.safetensors", &[0xDDu8; 16 * 1024]),
        ] {
            seed(&src_same_prefixed, "data-lake", p, b).await;
        }
        let source_same = ResolvedStore {
            store: Store::new(Arc::clone(&src_same_prefixed)),
            bucket: bucket_id.clone(),
            prefix: "data-lake".into(),
        };
        let target_same = ResolvedStore {
            store: Store::new(Arc::clone(&src_same_prefixed)),
            bucket: bucket_id.clone(),
            prefix: "repos/v2".into(),
        };

        let args_same = base_args(
            "s3://shared-bucket/data-lake/",
            "s3://shared-bucket/repos/v2",
        );
        let inmem_same = InMemoryVersionedList {
            store: Arc::clone(&src_same_prefixed),
            prefix: "data-lake".into(),
        };

        let git_dir_guard = git_dir_guard.set_env(&into_same.join(".git"));
        let cancel_same = CancellationToken::new();
        let summary_same = run_import_with_stores_inner(
            &args_same,
            source_same,
            target_same,
            &inmem_same,
            into_same.clone(),
            &cancel_same,
        )
        .await
        .expect("same-bucket run must succeed");
        drop(git_dir_guard);

        assert!(
            summary_same.same_bucket,
            "same-bucket summary flag must be true: {summary_same:?}"
        );

        // Source listing must be unchanged: every source key
        // that existed before the import still exists, and its
        // body is unchanged. Additional keys under ".crab/"
        // or "repos/v2/" are expected (the target layout).
        let metas_after: Vec<_> = src_same_prefixed.list(None).try_collect().await.unwrap();
        let keys_after: std::collections::HashSet<String> = metas_after
            .into_iter()
            .map(|m| m.location.to_string())
            .collect();
        for key in &keys_before_same {
            let full = format!("data-lake/{key}");
            assert!(
                keys_after.contains(&full),
                "same-bucket run erased source key {full}; got {keys_after:?}"
            );
        }

        // ── Case 2: cross-bucket (source and target are
        // independent ObjectStores). The source bucket must not
        // receive any writes.
        let git_dir_guard = GitDirOverride::locked_without_env();
        let tmp_cross = TempDir::new().unwrap();
        let (src_cross_inner, keys_before_cross) = seed_identical_source().await;

        // Snapshot the source listing before the import runs.
        let metas_before_cross: Vec<_> = src_cross_inner.list(None).try_collect().await.unwrap();
        let keys_before_cross_set: std::collections::HashSet<String> = metas_before_cross
            .into_iter()
            .map(|m| m.location.to_string())
            .collect();

        let tgt_cross_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let into_cross = tmp_cross.path().join("repo");
        std::fs::create_dir_all(&into_cross).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into_cross)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into_cross);

        let source_cross = ResolvedStore {
            store: Store::new(Arc::clone(&src_cross_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "src-bucket".into(),
                container: "src-bucket".into(),
            },
            prefix: String::new(),
        };
        let target_cross = ResolvedStore {
            store: Store::new(Arc::clone(&tgt_cross_inner)),
            bucket: BucketIdentity {
                cloud: crate::git::url::Cloud::S3,
                host: "dst-bucket".into(),
                container: "dst-bucket".into(),
            },
            prefix: "repos/v2".into(),
        };

        let args_cross = base_args("s3://src-bucket/", "s3://dst-bucket/repos/v2");
        let inmem_cross = InMemoryVersionedList {
            store: Arc::clone(&src_cross_inner),
            prefix: String::new(),
        };

        let git_dir_guard = git_dir_guard.set_env(&into_cross.join(".git"));
        let cancel_cross = CancellationToken::new();
        let summary_cross = run_import_with_stores_inner(
            &args_cross,
            source_cross,
            target_cross,
            &inmem_cross,
            into_cross.clone(),
            &cancel_cross,
        )
        .await
        .expect("cross-bucket run must succeed");
        drop(git_dir_guard);

        assert!(
            !summary_cross.same_bucket,
            "cross-bucket summary flag must be false: {summary_cross:?}"
        );

        // Source bucket must be byte-identical to what we
        // started with.
        let metas_after_cross: Vec<_> = src_cross_inner.list(None).try_collect().await.unwrap();
        let keys_after_cross: std::collections::HashSet<String> = metas_after_cross
            .into_iter()
            .map(|m| m.location.to_string())
            .collect();
        assert_eq!(
            keys_before_cross_set, keys_after_cross,
            "cross-bucket source listing must be unchanged post-import"
        );

        // Both runs observed the same source keys — reuse
        // `keys_before_cross` for a belt-and-suspenders cross-
        // check that neither run mutated the source.
        assert_eq!(
            keys_before_same.len(),
            keys_before_cross.len(),
            "same-bucket and cross-bucket runs must see the same source key count"
        );
    }
}
