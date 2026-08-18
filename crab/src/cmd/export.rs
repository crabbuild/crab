//! `crab export` — materialize a Crab snapshot into raw object storage.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use clap::Parser;
use futures_util::StreamExt as _;
use object_store::MultipartUpload;
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::git::url::{ObjectUrl, UrlForm};
use crate::read::selection::{
    EmptySelection, SnapshotSelection, normalize_repo_path, select_snapshot_entries,
};
use crate::read::{DownloadEntry, RepositoryOpenOptions, RepositoryReader, SnapshotReader};
use crate::storage::{ResolvedObjectStore, Store, resolve_object_url_store};

const EXPORT_SCHEMA: &str = "export.summary";
const EXPORT_EVENT_SCHEMA: &str = "export.file";
const EXPORT_VERSION: &str = "1.0";
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// Arguments for `crab export`.
#[derive(Debug, Clone, Parser)]
pub struct ExportArgs {
    /// Repository followed by optional exact paths or trailing-slash subtrees.
    #[arg(value_name = "REPO_OR_PATH")]
    pub inputs: Vec<String>,

    /// Source repository. When set, all positional values are path selectors.
    #[arg(long = "from", value_name = "REPO")]
    pub from: Option<String>,

    /// Raw target URL: s3://, gs://, az://, azure://, or file://.
    #[arg(long, value_name = "URL")]
    pub to: String,

    /// Branch, tag, ref, or full commit SHA.
    #[arg(long)]
    pub revision: Option<String>,

    /// Include glob patterns.
    #[arg(long = "include", value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude glob patterns.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Override Crab cache root.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<std::path::PathBuf>,

    /// File-level export concurrency.
    #[arg(long, short = 'j', value_name = "N")]
    pub jobs: Option<usize>,

    /// Plan and print without writing target objects.
    #[arg(long)]
    pub dry_run: bool,

    /// Overwrite existing target objects.
    #[arg(long)]
    pub force: bool,

    /// Suppress human progress/summary while still printing paths.
    #[arg(long)]
    pub quiet: bool,

    /// Structured JSON output (single envelope with terminal result).
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,

    /// Streaming JSONL output (one event per line).
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

impl ExportArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, self.jsonl)
    }
}

/// Summary of a completed export invocation.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExportSummary {
    /// Repository input string.
    pub repo: String,
    /// Raw target URL.
    pub target_url: String,
    /// Revision requested by the user.
    pub requested_revision: String,
    /// Resolved commit SHA.
    pub resolved_revision: String,
    /// Number of selected files.
    pub files_planned: u64,
    /// Number of files exported in this run.
    pub files_exported: u64,
    /// Number of files that would conflict with existing target objects.
    pub files_conflicted: u64,
    /// Logical bytes across all selected files.
    pub bytes_planned: u64,
    /// Bytes exported in this run.
    pub bytes_exported: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Per-file outcomes.
    pub files: Vec<ExportFileResult>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Result for one planned export file.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExportFileResult {
    /// Repo-relative forward-slash path.
    pub repo_path: String,
    /// Raw object path under the target bucket/container/root.
    pub object_path: String,
    /// Logical file size.
    pub bytes: u64,
    /// Outcome status.
    pub status: ExportFileStatus,
}

/// Per-file export outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFileStatus {
    /// File was exported.
    Exported,
    /// Dry-run: file would be exported.
    WouldExport,
    /// Dry-run or preflight: target already exists.
    WouldConflict,
}

/// JSONL planning event emitted before an export writes files.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExportPlanEvent {
    /// Repository input string.
    pub repo: String,
    /// Raw target URL.
    pub target_url: String,
    /// Revision requested by the user.
    pub requested_revision: String,
    /// Resolved commit SHA.
    pub resolved_revision: String,
    /// Number of files selected for export.
    pub files: u64,
    /// Logical bytes selected for export.
    pub bytes: u64,
    /// Whether this invocation only planned writes.
    pub dry_run: bool,
    /// Whether existing target objects may be overwritten.
    pub force: bool,
}

#[derive(Debug, Clone)]
struct PlannedExport {
    entry: DownloadEntry,
    object_path: ObjectPath,
    conflicts: bool,
}

/// Run `crab export`.
pub async fn run_export(args: &ExportArgs, cancel: &CancellationToken) -> Result<ExportSummary> {
    check_cancelled(cancel)?;
    let start = Instant::now();
    let (repo, paths) = resolve_source_and_paths(args)?;
    let target_url = parse_raw_target_url(&args.to)?;

    let mut config = Config::resolve_local()?;
    let jobs = effective_jobs(args, &config)?;
    config.hydrate.download_concurrency = jobs;

    let reader = RepositoryReader::open(
        &repo,
        RepositoryOpenOptions {
            cache_dir: args.cache_dir.clone(),
            config: config.clone(),
            cancel: cancel.clone(),
        },
    )
    .await?;
    let snapshot = reader.snapshot(args.revision.as_deref()).await?;
    let target = resolve_object_url_store(&target_url, &config, "push", cancel).await?;

    let selected = select_snapshot_entries(
        &snapshot,
        SnapshotSelection {
            paths: &paths,
            include: &args.include,
            exclude: &args.exclude,
            empty: EmptySelection::All,
            origin: "export",
        },
    )
    .await?;
    if selected.is_empty() {
        return Err(CrabError::NotFound {
            path: "export selection matched no files".to_owned(),
        });
    }

    let mut plan = plan_exports(&target, selected)?;
    if !args.force {
        preflight_conflicts(&target.store, &mut plan).await?;
    }

    let bytes_planned = plan
        .iter()
        .map(|item| item.entry.size)
        .fold(0u64, u64::saturating_add);
    let mut jsonl = match args.output_mode() {
        OutputMode::Jsonl => Some(JsonlStream::new(
            EXPORT_EVENT_SCHEMA,
            EXPORT_VERSION,
            std::io::stdout(),
        )),
        OutputMode::Text | OutputMode::Json => None,
    };

    emit_plan_event(args, &repo, &snapshot, &plan, bytes_planned, jsonl.as_mut());
    if args.output_mode() == OutputMode::Text && !args.quiet {
        eprintln!(
            "export: {} file(s), {} byte(s) selected from {}@{}",
            plan.len(),
            bytes_planned,
            repo,
            snapshot.resolved_revision()
        );
    }

    let results = if args.dry_run {
        dry_run_results(&plan, jsonl.as_mut())
    } else if let Some(conflict) = plan.iter().find(|item| item.conflicts) {
        let result = file_result(conflict, ExportFileStatus::WouldConflict);
        emit_file_event(&result, jsonl.as_mut());
        return Err(CrabError::CasConflict {
            path: result.object_path,
            expected_etag: None,
        });
    } else {
        export_entries(
            &snapshot,
            &target.store,
            plan,
            jobs,
            args.force,
            cancel,
            jsonl.as_mut(),
        )
        .await?
    };

    let summary = build_summary(args, repo, &snapshot, results, bytes_planned, start);
    emit_summary(args, &summary, jsonl.as_mut());
    Ok(summary)
}

fn resolve_source_and_paths(args: &ExportArgs) -> Result<(String, Vec<String>)> {
    if let Some(from) = args
        .from
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Ok((from.to_owned(), args.inputs.clone()));
    }

    let Some((repo, paths)) = args.inputs.split_first() else {
        return Err(CrabError::Configuration {
            key: "export requires <repo> or --from <repo>".to_owned(),
            origin: "crab export".to_owned(),
        });
    };
    Ok((repo.clone(), paths.to_vec()))
}

fn effective_jobs(args: &ExportArgs, config: &Config) -> Result<usize> {
    match args.jobs {
        Some(0) => Err(CrabError::Configuration {
            key: "--jobs must be greater than zero".to_owned(),
            origin: "crab export".to_owned(),
        }),
        Some(n) => Ok(n),
        None => Ok(config.hydrate.download_concurrency.max(1)),
    }
}

fn parse_raw_target_url(raw: &str) -> Result<ObjectUrl> {
    let target_url = ObjectUrl::parse(raw)?;
    if target_url.form != UrlForm::Raw {
        return Err(CrabError::Configuration {
            key: "export target must be a raw object-storage URL, not crab://".to_owned(),
            origin: "crab export".to_owned(),
        });
    }
    Ok(target_url)
}

fn plan_exports(
    target: &ResolvedObjectStore,
    entries: Vec<DownloadEntry>,
) -> Result<Vec<PlannedExport>> {
    entries
        .into_iter()
        .map(|entry| {
            let object_path = target_object_path(&target.prefix, &entry.path)?;
            Ok(PlannedExport {
                entry,
                object_path,
                conflicts: false,
            })
        })
        .collect()
}

fn target_object_path(prefix: &str, repo_path: &str) -> Result<ObjectPath> {
    let normalized = normalize_repo_path(repo_path)?;
    let prefix = prefix.trim().trim_matches('/');
    if prefix.is_empty() {
        return Ok(ObjectPath::from(normalized));
    }
    Ok(ObjectPath::from(format!("{prefix}/{normalized}")))
}

async fn preflight_conflicts(store: &Store, plan: &mut [PlannedExport]) -> Result<()> {
    for item in plan {
        item.conflicts = object_exists(store, &item.object_path).await?;
    }
    Ok(())
}

async fn object_exists(store: &Store, path: &ObjectPath) -> Result<bool> {
    match store.head(path).await {
        Ok(_) => Ok(true),
        Err(CrabError::NotFound { .. }) => Ok(false),
        Err(err) => Err(err),
    }
}

fn dry_run_results(
    plan: &[PlannedExport],
    mut jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) -> Vec<ExportFileResult> {
    let mut results = Vec::with_capacity(plan.len());
    for item in plan {
        let status = if item.conflicts {
            ExportFileStatus::WouldConflict
        } else {
            ExportFileStatus::WouldExport
        };
        let result = file_result(item, status);
        emit_file_event(&result, jsonl.as_deref_mut());
        results.push(result);
    }
    results
}

async fn export_entries(
    snapshot: &SnapshotReader,
    store: &Store,
    entries: Vec<PlannedExport>,
    jobs: usize,
    force: bool,
    cancel: &CancellationToken,
    mut jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) -> Result<Vec<ExportFileResult>> {
    let semaphore = Arc::new(Semaphore::new(jobs));
    let mut futures = futures_util::stream::FuturesUnordered::new();

    for item in entries {
        check_cancelled(cancel)?;
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .map_err(|e| CrabError::Internal(format!("export semaphore closed: {e}")))?;
        let snapshot = snapshot.clone();
        let store = store.clone();
        futures.push(tokio::spawn(async move {
            let _permit = permit;
            export_one(&snapshot, &store, item, force).await
        }));
    }

    let mut results = Vec::new();
    while let Some(joined) = futures.next().await {
        check_cancelled(cancel)?;
        let result = joined
            .map_err(|join_err| CrabError::Internal(format!("export task failed: {join_err}")))??;
        emit_file_event(&result, jsonl.as_deref_mut());
        results.push(result);
    }
    results.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
    Ok(results)
}

async fn export_one(
    snapshot: &SnapshotReader,
    store: &Store,
    item: PlannedExport,
    force: bool,
) -> Result<ExportFileResult> {
    if item.entry.size == 0 {
        if force {
            store.put_overwrite(&item.object_path, Bytes::new()).await?;
        } else {
            store.put(&item.object_path, Bytes::new()).await?;
        }
        return Ok(file_result(&item, ExportFileStatus::Exported));
    }

    let temp_path = temp_object_path(&item.object_path);
    let upload = store.create_multipart_upload(&temp_path).await?;
    let controller = MultipartUploadController::new(upload, temp_path.clone());
    let writer = controller.writer();

    let write_result = snapshot.write_to_writer(&item.entry.path, writer).await;
    let bytes = match write_result {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = controller.abort().await;
            cleanup_temp(store, &temp_path).await;
            return Err(err);
        }
    };
    if bytes != item.entry.size {
        let _ = controller.abort().await;
        cleanup_temp(store, &temp_path).await;
        return Err(CrabError::CorruptObject {
            path: item.entry.path,
            reason: format!("exported {bytes} byte(s), expected {}", item.entry.size),
        });
    }

    if let Err(err) = controller.finish().await {
        cleanup_temp(store, &temp_path).await;
        return Err(err);
    }
    let copy_result = if force {
        store.copy(&temp_path, &item.object_path).await
    } else {
        store
            .copy_if_not_exists(&temp_path, &item.object_path)
            .await
    };
    if let Err(err) = copy_result {
        cleanup_temp(store, &temp_path).await;
        return Err(err);
    }
    cleanup_temp(store, &temp_path).await;

    Ok(file_result(&item, ExportFileStatus::Exported))
}

async fn cleanup_temp(store: &Store, path: &ObjectPath) {
    match store.delete(path).await {
        Ok(()) | Err(CrabError::NotFound { .. }) => {}
        Err(err) => {
            tracing::warn!(path = %path, error = %err, "failed to delete export temp object");
        }
    }
}

fn temp_object_path(final_path: &ObjectPath) -> ObjectPath {
    let final_key = final_path.as_ref();
    let parent = final_key
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty());
    let temp_dir = match parent {
        Some(parent) => format!("{parent}/.crab-export-tmp"),
        None => ".crab-export-tmp".to_owned(),
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let hash = blake3::hash(final_key.as_bytes()).to_hex().to_string();
    ObjectPath::from(format!("{temp_dir}/{}-{nonce}-{hash}", std::process::id()))
}

fn file_result(item: &PlannedExport, status: ExportFileStatus) -> ExportFileResult {
    ExportFileResult {
        repo_path: item.entry.path.clone(),
        object_path: item.object_path.to_string(),
        bytes: item.entry.size,
        status,
    }
}

fn build_summary(
    args: &ExportArgs,
    repo: String,
    snapshot: &SnapshotReader,
    results: Vec<ExportFileResult>,
    bytes_planned: u64,
    start: Instant,
) -> ExportSummary {
    let files_exported = results
        .iter()
        .filter(|result| result.status == ExportFileStatus::Exported)
        .count() as u64;
    let files_conflicted = results
        .iter()
        .filter(|result| result.status == ExportFileStatus::WouldConflict)
        .count() as u64;
    let bytes_exported = results
        .iter()
        .filter(|result| result.status == ExportFileStatus::Exported)
        .map(|result| result.bytes)
        .fold(0u64, u64::saturating_add);

    ExportSummary {
        repo,
        target_url: args.to.clone(),
        requested_revision: snapshot.requested_revision().to_owned(),
        resolved_revision: snapshot.resolved_revision().to_owned(),
        files_planned: results.len() as u64,
        files_exported,
        files_conflicted,
        bytes_planned,
        bytes_exported,
        dry_run: args.dry_run,
        files: results,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn emit_plan_event(
    args: &ExportArgs,
    repo: &str,
    snapshot: &SnapshotReader,
    plan: &[PlannedExport],
    bytes_planned: u64,
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) {
    if let Some(stream) = jsonl {
        stream.emit_schema_event(
            "export.plan",
            "event",
            ExportPlanEvent {
                repo: repo.to_owned(),
                target_url: args.to.clone(),
                requested_revision: snapshot.requested_revision().to_owned(),
                resolved_revision: snapshot.resolved_revision().to_owned(),
                files: plan.len() as u64,
                bytes: bytes_planned,
                dry_run: args.dry_run,
                force: args.force,
            },
        );
    }
}

fn emit_file_event(result: &ExportFileResult, jsonl: Option<&mut JsonlStream<std::io::Stdout>>) {
    if let Some(stream) = jsonl {
        stream.emit_schema_event(EXPORT_EVENT_SCHEMA, "event", result);
    }
}

fn emit_summary(
    args: &ExportArgs,
    summary: &ExportSummary,
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) {
    match args.output_mode() {
        OutputMode::Text => {
            for file in &summary.files {
                println!("{}", file.object_path);
            }
            if !args.quiet {
                eprintln!(
                    "export complete: {} exported, {} conflict(s), {} byte(s) written",
                    summary.files_exported, summary.files_conflicted, summary.bytes_exported
                );
            }
        }
        OutputMode::Json => emit_json(EXPORT_SCHEMA, EXPORT_VERSION, summary),
        OutputMode::Jsonl => {
            if let Some(stream) = jsonl {
                stream.emit_schema_event(EXPORT_SCHEMA, "result", summary);
            }
        }
    }
}

struct MultipartUploadController {
    state: Arc<Mutex<MultipartUploadState>>,
    path: ObjectPath,
}

struct MultipartUploadState {
    upload: Option<Box<dyn MultipartUpload>>,
    buffer: Vec<u8>,
    failed: bool,
}

impl MultipartUploadController {
    fn new(upload: Box<dyn MultipartUpload>, path: ObjectPath) -> Self {
        Self {
            state: Arc::new(Mutex::new(MultipartUploadState {
                upload: Some(upload),
                buffer: Vec::with_capacity(MULTIPART_PART_SIZE),
                failed: false,
            })),
            path,
        }
    }

    fn writer(&self) -> MultipartUploadWriter {
        MultipartUploadWriter {
            state: Arc::clone(&self.state),
            path: self.path.clone(),
        }
    }

    async fn finish(&self) -> Result<()> {
        let (mut upload, final_part) = {
            let mut state = self.lock_state()?;
            if state.failed {
                return Err(CrabError::Io(io::Error::other(
                    "multipart writer failed before finish",
                )));
            }
            let upload = state.upload.take().ok_or_else(|| {
                CrabError::Io(io::Error::other("multipart upload already finished"))
            })?;
            let final_part = if state.buffer.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut state.buffer))
            };
            (upload, final_part)
        };

        if let Some(part) = final_part
            && let Err(err) = upload.put_part(Bytes::from(part).into()).await
        {
            let _ = upload.abort().await;
            return Err(crate::core::error::CrabError::from(
                crab_storage::map_object_store_error(err, self.path.as_ref()),
            ));
        }

        if let Err(err) = upload.complete().await {
            let mapped = crate::core::error::CrabError::from(crab_storage::map_object_store_error(
                err,
                self.path.as_ref(),
            ));
            let _ = upload.abort().await;
            return Err(mapped);
        }
        Ok(())
    }

    async fn abort(&self) -> Result<()> {
        let upload = {
            let mut state = self.lock_state()?;
            state.failed = true;
            state.buffer.clear();
            state.upload.take()
        };
        if let Some(mut upload) = upload {
            upload.abort().await.map_err(|err| {
                crate::core::error::CrabError::from(crab_storage::map_object_store_error(
                    err,
                    self.path.as_ref(),
                ))
            })?;
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, MultipartUploadState>> {
        self.state
            .lock()
            .map_err(|_| CrabError::Io(io::Error::other("multipart writer mutex poisoned")))
    }
}

struct MultipartUploadWriter {
    state: Arc<Mutex<MultipartUploadState>>,
    path: ObjectPath,
}

impl MultipartUploadWriter {
    fn flush_full_parts(&self) -> io::Result<()> {
        loop {
            let maybe_part = {
                let mut state = self.lock_state()?;
                if state.failed {
                    return Err(io::Error::other("multipart writer failed"));
                }
                if state.buffer.len() < MULTIPART_PART_SIZE {
                    return Ok(());
                }
                let upload = state
                    .upload
                    .take()
                    .ok_or_else(|| io::Error::other("multipart upload already finished"))?;
                let tail = state.buffer.split_off(MULTIPART_PART_SIZE);
                let part = std::mem::replace(&mut state.buffer, tail);
                Some((upload, part))
            };

            if let Some((mut upload, part)) = maybe_part {
                let result = upload_part_blocking(&mut upload, part, &self.path);
                let mut state = self.lock_state()?;
                state.upload = Some(upload);
                if let Err(err) = result {
                    state.failed = true;
                    return Err(err);
                }
            }
        }
    }

    fn lock_state(&self) -> io::Result<std::sync::MutexGuard<'_, MultipartUploadState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("multipart writer mutex poisoned"))
    }
}

impl io::Write for MultipartUploadWriter {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let total = buf.len();
        while !buf.is_empty() {
            let copied = {
                let mut state = self.lock_state()?;
                if state.failed {
                    return Err(io::Error::other("multipart writer failed"));
                }
                let remaining = MULTIPART_PART_SIZE.saturating_sub(state.buffer.len());
                let take = remaining.min(buf.len());
                state.buffer.extend_from_slice(&buf[..take]);
                take
            };
            buf = &buf[copied..];
            self.flush_full_parts()?;
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        let state = self.lock_state()?;
        if state.failed {
            return Err(io::Error::other("multipart writer failed"));
        }
        Ok(())
    }
}

fn upload_part_blocking(
    upload: &mut Box<dyn MultipartUpload>,
    part: Vec<u8>,
    path: &ObjectPath,
) -> io::Result<()> {
    let fut = upload.put_part(Bytes::from(part).into());
    block_on_object_store(fut).map_err(|err| io::Error::other(format!("{path}: {err}")))
}

fn block_on_object_store<F, T>(future: F) -> object_store::Result<T>
where
    F: std::future::Future<Output = object_store::Result<T>>,
{
    let handle = tokio::runtime::Handle::current();
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        _ => Err(object_store::Error::Generic {
            store: "crab export",
            source: "multipart export writer requires a multi-thread Tokio runtime".into(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn positional_repo_and_paths_parse() {
        let args = ExportArgs::parse_from([
            "export",
            "crab://bucket/repo",
            "--to",
            "s3://bucket/out",
            "models/",
            "README.md",
        ]);
        let (repo, paths) = resolve_source_and_paths(&args).unwrap();
        assert_eq!(repo, "crab://bucket/repo");
        assert_eq!(paths, vec!["models/".to_owned(), "README.md".to_owned()]);
    }

    #[test]
    fn from_flag_treats_positionals_as_paths() {
        let args = ExportArgs::parse_from([
            "export",
            "--from",
            "crab://bucket/repo",
            "--to",
            "s3://bucket/out",
            "models/",
        ]);
        let (repo, paths) = resolve_source_and_paths(&args).unwrap();
        assert_eq!(repo, "crab://bucket/repo");
        assert_eq!(paths, vec!["models/".to_owned()]);
    }

    #[test]
    fn target_object_path_preserves_repo_path_under_prefix() {
        let path = target_object_path("exports/snap", "models/a.bin").unwrap();
        assert_eq!(path.as_ref(), "exports/snap/models/a.bin");
    }

    #[test]
    fn target_object_path_rejects_escape() {
        let err = target_object_path("exports", "../secret").unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn jobs_must_be_positive() {
        let args = ExportArgs {
            inputs: vec![".".to_owned()],
            from: None,
            to: "file:///tmp/out".to_owned(),
            revision: None,
            include: Vec::new(),
            exclude: Vec::new(),
            cache_dir: None,
            jobs: Some(0),
            dry_run: false,
            force: false,
            quiet: false,
            json: false,
            jsonl: false,
        };
        let err = effective_jobs(&args, &Config::default()).unwrap_err();
        assert!(err.to_string().contains("--jobs"));
    }

    #[test]
    fn raw_target_rejects_crab_url() {
        let err = parse_raw_target_url("crab://bucket/repo").unwrap_err();
        assert!(err.to_string().contains("raw object-storage URL"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_writer_uploads_exact_bytes() {
        use std::io::Write as _;
        use std::sync::Arc;

        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let path = ObjectPath::from("exports/large.bin");
        let upload = store.create_multipart_upload(&path).await.unwrap();
        let controller = MultipartUploadController::new(upload, path.clone());
        let mut writer = controller.writer();
        let bytes = vec![42u8; MULTIPART_PART_SIZE + 123];

        writer.write_all(&bytes).unwrap();
        controller.finish().await.unwrap();

        let (stored, _) = store.get_with_etag(&path).await.unwrap();
        assert_eq!(stored, Bytes::from(bytes));
    }
}
