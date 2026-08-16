//! `crab download` / `crab get` selective file materialization.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
#[cfg(test)]
use crate::read::selection::normalize_path_selectors;
use crate::read::selection::{
    EmptySelection, SnapshotSelection, normalize_repo_path, select_snapshot_entries,
};
use crate::read::{
    DownloadEntry, DownloadEntryKind, RepositoryOpenOptions, RepositoryReader, SnapshotReader,
};

const DOWNLOAD_SCHEMA: &str = "download";
const DOWNLOAD_EVENT_SCHEMA: &str = "download.event";
const DOWNLOAD_VERSION: &str = "1.0";
const LOCAL_METADATA_VERSION: u32 = 1;

/// Arguments for `crab download` / `crab get`.
pub struct DownloadArgs {
    /// Remote object URL or local repository path.
    pub repo: String,
    /// Exact file paths or trailing-slash subtree selectors.
    pub paths: Vec<String>,
    /// Branch, tag, ref, or full commit SHA.
    pub revision: Option<String>,
    /// Include glob patterns.
    pub include: Vec<String>,
    /// Exclude glob patterns.
    pub exclude: Vec<String>,
    /// Override Crab cache root.
    pub cache_dir: Option<PathBuf>,
    /// Write materialized files under this directory.
    pub local_dir: Option<PathBuf>,
    /// Download even when a destination appears fresh.
    pub force_download: bool,
    /// Plan and print without destination or metadata writes.
    pub dry_run: bool,
    /// File-level fanout and hydrate download concurrency.
    pub max_workers: Option<usize>,
    /// Suppress human progress/summary while still printing paths.
    pub quiet: bool,
    /// Output mode resolved from CLI flags.
    pub mode: OutputMode,
}

/// Summary of a completed download invocation.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DownloadSummary {
    /// Repository input string.
    pub repo: String,
    /// Revision requested by the user.
    pub requested_revision: String,
    /// Resolved commit SHA.
    pub resolved_revision: String,
    /// Root directory selected for output files.
    pub destination_root: String,
    /// Number of selected files.
    pub files_planned: u64,
    /// Number of files materialized in this run.
    pub files_downloaded: u64,
    /// Number of files skipped because they were fresh.
    pub files_skipped: u64,
    /// Logical bytes across all selected files.
    pub bytes_planned: u64,
    /// Bytes materialized in this run.
    pub bytes_downloaded: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Per-file outcomes.
    pub files: Vec<DownloadFileResult>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Result for one planned file.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DownloadFileResult {
    /// Repo-relative forward-slash path.
    pub repo_path: String,
    /// Local path where bytes are or would be materialized.
    pub local_path: String,
    /// Logical file size.
    pub bytes: u64,
    /// Outcome status.
    pub status: DownloadFileStatus,
}

/// Per-file download outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DownloadFileStatus {
    /// File was materialized.
    Downloaded,
    /// File was already fresh.
    Skipped,
    /// Dry-run: file would be materialized.
    WouldDownload,
    /// Dry-run: file would be skipped.
    WouldSkip,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct DownloadPlanEvent {
    repo: String,
    requested_revision: String,
    resolved_revision: String,
    destination_root: String,
    files: u64,
    bytes: u64,
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct PlannedDownload {
    entry: DownloadEntry,
    local_path: PathBuf,
    is_fresh: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LocalDownloadMetadata {
    version: u32,
    entries: BTreeMap<String, LocalMetadataEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LocalMetadataEntry {
    repo: String,
    resolved_revision: String,
    size: u64,
    kind: DownloadEntryKind,
    content_hash: Option<String>,
}

/// Run `crab download` / `crab get`.
pub async fn run_download(
    args: &DownloadArgs,
    cancel: &CancellationToken,
) -> Result<DownloadSummary> {
    validate_selector_args(args)?;
    check_cancelled(cancel)?;

    let start = Instant::now();
    let mut config = Config::resolve_local().unwrap_or_else(|e| {
        tracing::debug!(error = %e, "download: failed to resolve local config, using defaults");
        Config::default()
    });
    let max_workers = effective_max_workers(args, &config)?;
    config.hydrate.download_concurrency = max_workers;

    let reader = RepositoryReader::open(
        &args.repo,
        RepositoryOpenOptions {
            cache_dir: args.cache_dir.clone(),
            config,
            cancel: cancel.clone(),
        },
    )
    .await?;
    let snapshot = reader.snapshot(args.revision.as_deref()).await?;

    let destination_root = destination_root(args, &reader, snapshot.resolved_revision());
    let local_metadata_path = args
        .local_dir
        .as_ref()
        .map(|dir| dir.join(".cache").join("crab").join("downloads-v1.json"));
    let mut metadata = match local_metadata_path.as_deref() {
        Some(path) if !args.dry_run => read_local_metadata(path)?,
        _ => LocalDownloadMetadata::default(),
    };

    let selected = select_entries(&snapshot, args).await?;
    if selected.is_empty() {
        return Err(CrabError::NotFound {
            path: "download selection matched no files".to_owned(),
        });
    }

    let mut plan = Vec::with_capacity(selected.len());
    for entry in selected {
        let local_path = destination_for(&destination_root, &entry.path)?;
        let is_fresh = !args.force_download
            && destination_is_fresh(
                &local_path,
                &entry,
                &args.repo,
                snapshot.resolved_revision(),
                args.local_dir.is_some(),
                &metadata,
            );
        plan.push(PlannedDownload {
            entry,
            local_path,
            is_fresh,
        });
    }
    let entries_by_path: BTreeMap<String, DownloadEntry> = plan
        .iter()
        .map(|item| (item.entry.path.clone(), item.entry.clone()))
        .collect();

    let bytes_planned = plan
        .iter()
        .map(|item| item.entry.size)
        .fold(0u64, u64::saturating_add);
    let mut jsonl = match args.mode {
        OutputMode::Jsonl => Some(JsonlStream::new(
            DOWNLOAD_EVENT_SCHEMA,
            DOWNLOAD_VERSION,
            std::io::stdout(),
        )),
        OutputMode::Text | OutputMode::Json => None,
    };

    emit_plan_event(
        args,
        &snapshot,
        &destination_root,
        &plan,
        bytes_planned,
        jsonl.as_mut(),
    );
    if args.mode == OutputMode::Text && !args.quiet {
        eprintln!(
            "download: {} file(s), {} byte(s) selected from {}@{}",
            plan.len(),
            bytes_planned,
            args.repo,
            snapshot.resolved_revision()
        );
    }

    let mut results = Vec::with_capacity(plan.len());
    let mut to_download = Vec::new();
    for item in plan {
        if args.dry_run {
            let status = if item.is_fresh {
                DownloadFileStatus::WouldSkip
            } else {
                DownloadFileStatus::WouldDownload
            };
            let result = file_result(&item, status);
            emit_file_event(&result, jsonl.as_mut());
            results.push(result);
        } else if item.is_fresh {
            let result = file_result(&item, DownloadFileStatus::Skipped);
            emit_file_event(&result, jsonl.as_mut());
            results.push(result);
        } else {
            to_download.push(item);
        }
    }

    let downloaded =
        download_entries(&snapshot, to_download, max_workers, cancel, jsonl.as_mut()).await?;
    results.extend(downloaded);
    results.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));

    if !args.dry_run && args.local_dir.is_some() {
        update_metadata_from_results(
            &mut metadata,
            &args.repo,
            snapshot.resolved_revision(),
            &results,
            &entries_by_path,
        );
        if let Some(path) = local_metadata_path.as_deref() {
            write_local_metadata(path, &metadata)?;
        }
    }

    let files_downloaded = results
        .iter()
        .filter(|result| result.status == DownloadFileStatus::Downloaded)
        .count() as u64;
    let files_skipped = results
        .iter()
        .filter(|result| {
            matches!(
                result.status,
                DownloadFileStatus::Skipped | DownloadFileStatus::WouldSkip
            )
        })
        .count() as u64;
    let bytes_downloaded = results
        .iter()
        .filter(|result| result.status == DownloadFileStatus::Downloaded)
        .map(|result| result.bytes)
        .fold(0u64, u64::saturating_add);

    let summary = DownloadSummary {
        repo: args.repo.clone(),
        requested_revision: snapshot.requested_revision().to_owned(),
        resolved_revision: snapshot.resolved_revision().to_owned(),
        destination_root: destination_root.display().to_string(),
        files_planned: results.len() as u64,
        files_downloaded,
        files_skipped,
        bytes_planned,
        bytes_downloaded,
        dry_run: args.dry_run,
        files: results,
        duration_ms: start.elapsed().as_millis() as u64,
    };

    emit_summary(args, &summary, jsonl.as_mut());
    Ok(summary)
}

fn validate_selector_args(args: &DownloadArgs) -> Result<()> {
    if args.paths.is_empty() && args.include.is_empty() {
        return Err(CrabError::Configuration {
            key: "download requires at least one path selector or --include pattern".to_owned(),
            origin: "cli".to_owned(),
        });
    }
    Ok(())
}

fn effective_max_workers(args: &DownloadArgs, config: &Config) -> Result<usize> {
    match args.max_workers {
        Some(0) => Err(CrabError::Configuration {
            key: "--max-workers must be greater than zero".to_owned(),
            origin: "cli".to_owned(),
        }),
        Some(n) => Ok(n),
        None => Ok(config.hydrate.download_concurrency.max(1)),
    }
}

async fn select_entries(
    snapshot: &SnapshotReader,
    args: &DownloadArgs,
) -> Result<Vec<DownloadEntry>> {
    select_snapshot_entries(
        snapshot,
        SnapshotSelection {
            paths: &args.paths,
            include: &args.include,
            exclude: &args.exclude,
            empty: EmptySelection::Reject,
            origin: "download",
        },
    )
    .await
}

fn destination_root(
    args: &DownloadArgs,
    reader: &RepositoryReader,
    resolved_revision: &str,
) -> PathBuf {
    args.local_dir
        .clone()
        .unwrap_or_else(|| reader.download_cache_root(resolved_revision))
}

fn destination_for(root: &Path, repo_path: &str) -> Result<PathBuf> {
    let normalized = normalize_repo_path(repo_path)?;
    let mut out = root.to_path_buf();
    for component in normalized.split('/') {
        out.push(component);
    }
    Ok(out)
}

fn destination_is_fresh(
    local_path: &Path,
    entry: &DownloadEntry,
    repo: &str,
    resolved_revision: &str,
    uses_local_metadata: bool,
    metadata: &LocalDownloadMetadata,
) -> bool {
    let Ok(file_metadata) = std::fs::metadata(local_path) else {
        return false;
    };
    if !file_metadata.is_file() || file_metadata.len() != entry.size {
        return false;
    }
    if !uses_local_metadata {
        return true;
    }

    metadata.entries.get(&entry.path).is_some_and(|stored| {
        stored.repo == repo
            && stored.resolved_revision == resolved_revision
            && stored.size == entry.size
            && stored.kind == entry.kind
            && stored.content_hash == entry.content_hash
    })
}

async fn download_entries(
    snapshot: &SnapshotReader,
    entries: Vec<PlannedDownload>,
    max_workers: usize,
    cancel: &CancellationToken,
    mut jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) -> Result<Vec<DownloadFileResult>> {
    let semaphore = Arc::new(Semaphore::new(max_workers));
    let mut futures = futures_util::stream::FuturesUnordered::new();

    for item in entries {
        check_cancelled(cancel)?;
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .map_err(|e| CrabError::Internal(format!("download semaphore closed: {e}")))?;
        let snapshot = snapshot.clone();
        futures.push(tokio::spawn(async move {
            let _permit = permit;
            materialize_one(&snapshot, item).await
        }));
    }

    let mut results = Vec::new();
    while let Some(joined) = futures.next().await {
        check_cancelled(cancel)?;
        let result = joined.map_err(|join_err| {
            CrabError::Internal(format!("download task failed: {join_err}"))
        })??;
        emit_file_event(&result, jsonl.as_deref_mut());
        results.push(result);
    }
    Ok(results)
}

async fn materialize_one(
    snapshot: &SnapshotReader,
    item: PlannedDownload,
) -> Result<DownloadFileResult> {
    if let Some(parent) = item.local_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let parent = item.local_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".crab-download-")
        .tempfile_in(parent)?
        .into_temp_path();
    let tmp_path = tmp.to_path_buf();
    let bytes = snapshot
        .download_to_path(&item.entry.path, &tmp_path)
        .await?;
    if bytes != item.entry.size {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(CrabError::CorruptObject {
            path: item.entry.path,
            reason: format!(
                "downloaded {bytes} byte(s), expected {} byte(s)",
                item.entry.size
            ),
        });
    }
    tmp.persist(&item.local_path)
        .map_err(|e| CrabError::Io(e.error))?;

    Ok(file_result(&item, DownloadFileStatus::Downloaded))
}

fn file_result(item: &PlannedDownload, status: DownloadFileStatus) -> DownloadFileResult {
    DownloadFileResult {
        repo_path: item.entry.path.clone(),
        local_path: item.local_path.display().to_string(),
        bytes: item.entry.size,
        status,
    }
}

fn emit_plan_event(
    args: &DownloadArgs,
    snapshot: &SnapshotReader,
    destination_root: &Path,
    plan: &[PlannedDownload],
    bytes_planned: u64,
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) {
    if let Some(stream) = jsonl {
        stream.emit_schema_event(
            "download.plan",
            "event",
            DownloadPlanEvent {
                repo: args.repo.clone(),
                requested_revision: snapshot.requested_revision().to_owned(),
                resolved_revision: snapshot.resolved_revision().to_owned(),
                destination_root: destination_root.display().to_string(),
                files: plan.len() as u64,
                bytes: bytes_planned,
                dry_run: args.dry_run,
            },
        );
    }
}

fn emit_file_event(result: &DownloadFileResult, jsonl: Option<&mut JsonlStream<std::io::Stdout>>) {
    if let Some(stream) = jsonl {
        stream.emit_schema_event("download.file", "event", result);
    }
}

fn emit_summary(
    args: &DownloadArgs,
    summary: &DownloadSummary,
    jsonl: Option<&mut JsonlStream<std::io::Stdout>>,
) {
    match args.mode {
        OutputMode::Text => {
            for file in &summary.files {
                println!("{}", file.local_path);
            }
            if !args.quiet {
                eprintln!(
                    "download complete: {} downloaded, {} skipped, {} byte(s) written",
                    summary.files_downloaded, summary.files_skipped, summary.bytes_downloaded
                );
            }
        }
        OutputMode::Json => emit_json(DOWNLOAD_SCHEMA, DOWNLOAD_VERSION, summary),
        OutputMode::Jsonl => {
            if let Some(stream) = jsonl {
                stream.emit_schema_event(DOWNLOAD_SCHEMA, "result", summary);
            }
        }
    }
}

fn read_local_metadata(path: &Path) -> Result<LocalDownloadMetadata> {
    if !path.is_file() {
        return Ok(LocalDownloadMetadata {
            version: LOCAL_METADATA_VERSION,
            entries: BTreeMap::new(),
        });
    }
    let bytes = std::fs::read(path)?;
    let mut metadata: LocalDownloadMetadata =
        serde_json::from_slice(&bytes).map_err(|e| CrabError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("invalid download metadata JSON: {e}"),
        })?;
    if metadata.version != LOCAL_METADATA_VERSION {
        metadata.version = LOCAL_METADATA_VERSION;
        metadata.entries.clear();
    }
    Ok(metadata)
}

fn write_local_metadata(path: &Path, metadata: &LocalDownloadMetadata) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tempfile::Builder::new()
        .prefix(".downloads-v1-")
        .tempfile_in(path.parent().unwrap_or_else(|| Path::new(".")))?
        .into_temp_path();
    let tmp_path = tmp.to_path_buf();
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, metadata)
            .map_err(|e| CrabError::Internal(format!("serialize download metadata: {e}")))?;
        file.write_all(b"\n")?;
        file.flush()?;
    }
    tmp.persist(path).map_err(|e| CrabError::Io(e.error))?;
    Ok(())
}

fn update_metadata_from_results(
    metadata: &mut LocalDownloadMetadata,
    repo: &str,
    resolved_revision: &str,
    results: &[DownloadFileResult],
    entries_by_path: &BTreeMap<String, DownloadEntry>,
) {
    for result in results {
        if matches!(
            result.status,
            DownloadFileStatus::Downloaded | DownloadFileStatus::Skipped
        ) && let Some(entry) = entries_by_path.get(&result.repo_path)
        {
            metadata.entries.insert(
                result.repo_path.clone(),
                LocalMetadataEntry {
                    repo: repo.to_owned(),
                    resolved_revision: resolved_revision.to_owned(),
                    size: entry.size,
                    kind: entry.kind,
                    content_hash: entry.content_hash.clone(),
                },
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args() -> DownloadArgs {
        DownloadArgs {
            repo: "repo".to_owned(),
            paths: vec!["models/".to_owned()],
            revision: None,
            include: Vec::new(),
            exclude: Vec::new(),
            cache_dir: None,
            local_dir: None,
            force_download: false,
            dry_run: false,
            max_workers: None,
            quiet: false,
            mode: OutputMode::Text,
        }
    }

    #[test]
    fn selector_validation_requires_path_or_include() {
        let mut args = args();
        args.paths.clear();
        let err = validate_selector_args(&args).unwrap_err();
        assert!(err.to_string().contains("requires at least one"));
    }

    #[test]
    fn normalize_repo_path_rejects_escape() {
        let err = normalize_repo_path("../secret").unwrap_err();
        assert!(err.to_string().contains("cannot contain '..'"));
    }

    #[test]
    fn trailing_slash_selector_becomes_prefix() {
        let selectors = normalize_path_selectors(&[String::from("./models/")]).unwrap();
        assert_eq!(selectors.prefixes, vec!["models/"]);
        assert!(selectors.exact.is_empty());
    }

    #[test]
    fn destination_preserves_repo_path_under_root() {
        let dest = destination_for(Path::new("/tmp/out"), "models/a.bin").unwrap();
        assert!(dest.ends_with("models/a.bin"));
    }

    #[test]
    fn metadata_fresh_requires_matching_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let entry = DownloadEntry {
            path: "a.txt".to_owned(),
            size: 5,
            kind: DownloadEntryKind::Git,
            content_hash: None,
        };
        let mut metadata = LocalDownloadMetadata {
            version: LOCAL_METADATA_VERSION,
            entries: BTreeMap::new(),
        };
        metadata.entries.insert(
            "a.txt".to_owned(),
            LocalMetadataEntry {
                repo: "repo".to_owned(),
                resolved_revision: "rev1".to_owned(),
                size: 5,
                kind: DownloadEntryKind::Git,
                content_hash: None,
            },
        );

        assert!(destination_is_fresh(
            &file, &entry, "repo", "rev1", true, &metadata,
        ));
        assert!(!destination_is_fresh(
            &file, &entry, "repo", "rev2", true, &metadata,
        ));
    }
}
