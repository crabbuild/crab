//! `crab import` — build a Crab-backed git repo from a raw
//! object-storage prefix.
//!
//! Entry point for the import command. The full pipeline
//! (detect → enumerate → ingest → assemble → publish) lives under
//! [`crate::import`]; this file owns CLI argument parsing and the
//! thin renderer that translates the pipeline's [`ImportSummary`]
//! into text / JSON / JSONL.

use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Mutex;

use clap::{Parser, ValueEnum};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::git::url::{Cloud, ObjectUrl, UrlForm};
use crate::import::coordinator::run_import_inner;
use crate::import::summary::ImportSummary;

/// How import should treat source-bucket object versioning.
///
/// `Auto` probes the source bucket and branches to versioned history
/// generation when any key has > 1 version or a delete-marker. `On`
/// requires versioning to be enabled; `Off` imports only each key's
/// latest live object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum VersionsMode {
    /// Probe the bucket and pick flat or versioned mode automatically.
    #[default]
    Auto,
    /// Require versioning; error if the bucket is flat.
    On,
    /// Ignore versions; import only the current live state.
    Off,
}

/// How import should handle Git LFS pointer sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LfsSourceMode {
    /// Refuse sources whose `.gitattributes` declares `filter=lfs`.
    Fail,
    /// Resolve LFS pointers from the companion object root before chunking.
    Resolve,
    /// Import non-LFS files and skip unresolved LFS pointer files.
    Skip,
}

/// Arguments for `crab import`.
///
/// URLs are named flags (`--from` / `--to`) rather than positional to
/// avoid the classic wrong-way-round mistake two URLs at the same
/// position on a command line invite.
#[derive(Debug, Clone, Parser)]
pub struct ImportArgs {
    /// Source URL or local path. Equivalent to --from for quick imports.
    #[arg(value_name = "SOURCE", conflicts_with = "from")]
    pub source: Option<String>,

    /// Source URL: raw cloud prefix (s3://, gs://, az://, file://).
    #[arg(long, value_name = "URL")]
    pub from: Option<String>,

    /// Target URL: crab:// or the same raw scheme as --from.
    #[arg(long, value_name = "URL")]
    pub to: Option<String>,

    /// Target Crab bucket, used with --name to build crab://<bucket>/<name>.
    #[arg(long, value_name = "BUCKET", requires = "name", conflicts_with = "to")]
    pub bucket: Option<String>,

    /// Target Crab repo name/path, used with --bucket.
    #[arg(long, value_name = "REPO", requires = "bucket", conflicts_with = "to")]
    pub name: Option<String>,

    /// Local directory for the new git repo (default: leaf of --to path).
    #[arg(long, value_name = "DIR")]
    pub into: Option<PathBuf>,

    /// Directory prefix to place imported files under inside the Git repo.
    #[arg(long, value_name = "PATH")]
    pub dest_prefix: Option<String>,

    /// Include glob (keys relative to the source prefix); repeatable.
    #[arg(long = "include", value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude glob (keys relative to the source prefix); repeatable.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Initial branch name.
    #[arg(long, default_value = "main")]
    pub branch: String,

    /// Commit message (or template when in versioned mode).
    #[arg(long, value_name = "TEXT")]
    pub message: Option<String>,

    /// Extra globs to mark `filter=crab` in `.gitattributes`; repeatable.
    #[arg(long = "track", value_name = "GLOB")]
    pub track: Vec<String>,

    /// Versioning behavior: auto (default), on, or off.
    #[arg(long, value_enum, default_value_t = VersionsMode::Auto)]
    pub versions: VersionsMode,

    /// Time-bucket width for versioned mode (e.g. `1h`, `30m`, `0s`).
    #[arg(long, value_name = "DURATION")]
    pub window: Option<String>,

    /// Single-snapshot mode: RFC3339 timestamp to import bucket state at.
    #[arg(long, value_name = "RFC3339")]
    pub at: Option<String>,

    /// Lower bound (RFC3339) for version history in versioned mode.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,

    /// Upper bound (RFC3339) for version history in versioned mode.
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,

    /// Author template applied to per-version commits.
    #[arg(long, value_name = "TEMPLATE")]
    pub author_template: Option<String>,

    /// Plan only: enumerate + classify without mutating anything.
    #[arg(long)]
    pub dry_run: bool,

    /// In dry-run, sample files and estimate xorb/shard bytes.
    #[arg(long)]
    pub estimate: bool,

    /// Resume a previous interrupted run from the journal.
    #[arg(long)]
    pub resume: bool,

    /// Concurrency for ingest workers (default: CPU count).
    #[arg(long, short = 'j', value_name = "N")]
    pub jobs: Option<usize>,

    /// Abort on first per-object error.
    #[arg(long)]
    pub fail_fast: bool,

    /// Bypass non-empty target and remote-already-exists safety rails.
    #[arg(long)]
    pub force: bool,

    /// LFS source handling mode: fail, resolve, or skip (default: fail).
    #[arg(long = "lfs-source", value_enum, value_name = "MODE")]
    pub lfs_source: Option<LfsSourceMode>,

    /// Companion LFS object root for --lfs-source resolve.
    #[arg(long = "lfs-objects", value_name = "URL")]
    pub lfs_objects: Option<String>,

    /// Deprecated alias for --lfs-source resolve.
    #[arg(long, hide = true)]
    pub allow_lfs_import: bool,

    /// Deprecated alias for --lfs-objects.
    #[arg(long, value_name = "URL", hide = true)]
    pub lfs_store: Option<String>,

    /// Skip interactive confirmation for large imports.
    #[arg(long)]
    pub yes: bool,

    /// Credential profile hint for the source bucket.
    #[arg(long, value_name = "NAME")]
    pub source_profile: Option<String>,

    /// Credential profile hint for the target bucket.
    #[arg(long, value_name = "NAME")]
    pub target_profile: Option<String>,

    /// Structured JSON output (single envelope with terminal result).
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,

    /// Streaming JSONL output (one event per line).
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

impl ImportArgs {
    /// Resolved output rendering mode for this invocation.
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, self.jsonl)
    }

    /// Source URL after resume normalization.
    pub fn from_url(&self) -> Result<&str> {
        required_url(self.from.as_deref(), "SOURCE or --from")
    }

    /// Target URL after resume normalization.
    pub fn to_url(&self) -> Result<&str> {
        required_url(self.to.as_deref(), "--to or --bucket/--name")
    }

    /// Effective LFS mode after applying the tagged legacy CLI aliases.
    pub fn effective_lfs_source(&self) -> LfsSourceMode {
        if let Some(mode) = self.lfs_source {
            mode
        } else if self.allow_lfs_import {
            LfsSourceMode::Resolve
        } else {
            LfsSourceMode::Fail
        }
    }

    /// Effective LFS object root after applying the tagged legacy CLI alias.
    pub fn effective_lfs_objects(&self) -> Option<&str> {
        self.lfs_objects
            .as_deref()
            .or(self.lfs_store.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

fn required_url<'a>(value: Option<&'a str>, flag: &str) -> Result<&'a str> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Err(CrabError::Configuration {
            key: format!("{flag} is required unless --resume is set"),
            origin: "crab import".into(),
        });
    };
    Ok(value)
}

/// Entry point for `crab import`.
///
/// Validates the CLI URL rules, delegates to the coordinator, and
/// renders the resulting [`ImportSummary`] according to the
/// resolved [`OutputMode`]. The coordinator owns URL→Store
/// resolution so CLI parsing stays separate from storage setup.
///
/// URL validation that lands here rather than later: `--from` must
/// be raw, and a raw `--to` must share the same cloud as `--from`.
/// Both rules catch whole-command misconfigurations (wrong-way-round
/// URLs, silent cross-cloud imports) before any storage or git work
/// kicks off. Crab-form `--to` URLs bypass the cloud-match rule
/// because `crab://` carries its provider via config, not the URL.
pub async fn run_import(args: &ImportArgs, cancel: &CancellationToken) -> Result<()> {
    let effective_args = crate::import::coordinator::resolve_import_args(args)?;
    let args = &effective_args;
    let from_raw = args.from_url()?;
    let to_raw = args.to_url()?;
    let from = ObjectUrl::parse(from_raw)?;
    let to = ObjectUrl::parse(to_raw)?;

    from.require_raw()?;

    if to.form == UrlForm::Raw && from.cloud != to.cloud {
        return Err(CrabError::ImportSchemeMismatch {
            from_scheme: scheme_label(from.cloud),
            to_scheme: scheme_label(to.cloud),
        });
    }

    let mode = args.output_mode();

    info!(
        from = %from_raw,
        to = %to_raw,
        versions = ?args.versions,
        dry_run = args.dry_run,
        mode = ?mode,
        "crab import invoked"
    );

    // JSONL streams open up front so per-stage events can land
    // before the pipeline finishes. Per-stage events are still
    // no-ops in the coordinator today; JSONL emits the terminal
    // `import.summary` event.
    let jsonl: Option<Mutex<JsonlStream<Stdout>>> = match mode {
        OutputMode::Jsonl => Some(Mutex::new(JsonlStream::new(
            "import.event",
            "1.0",
            std::io::stdout(),
        ))),
        _ => None,
    };

    let summary = run_import_inner(args, cancel).await?;

    render_summary(mode, &summary, jsonl.as_ref());

    Ok(())
}

/// Render the final summary according to the CLI output mode.
fn render_summary(
    mode: OutputMode,
    summary: &ImportSummary,
    jsonl: Option<&Mutex<JsonlStream<Stdout>>>,
) {
    match mode {
        OutputMode::Text => render_text_summary(summary),
        OutputMode::Json => emit_json("import.summary", "v1", summary),
        OutputMode::Jsonl => {
            if let Some(stream) = jsonl
                && let Ok(mut guard) = stream.lock()
            {
                guard.emit_result(summary);
            }
        }
    }
}

/// Text-mode summary rendering. Written to stderr so piping
/// stdout doesn't get polluted with human-readable output when
/// the user happens to be running without `--json`.
fn render_text_summary(s: &ImportSummary) {
    if s.dry_run {
        eprintln!(
            "import (dry run): would import {files} files ({bytes} bytes) from {source} → {target}",
            files = s.files_imported,
            bytes = s.bytes_source,
            source = s.source_url,
            target = s.target_url,
        );
        if let Some(plan) = &s.plan {
            eprintln!(
                "  planned commits: {}; versioning: {:?}; same-bucket: {}",
                plan.planned_commit_count, plan.versioning, plan.same_bucket
            );
            if plan.lfs_pointer_count > 0 {
                eprintln!("  LFS pointer blobs detected: {}", plan.lfs_pointer_count);
            }
            for warning in &plan.collision_warnings {
                eprintln!("  warning: {warning}");
            }
            if !plan.extension_histogram.is_empty() {
                eprintln!("  extension histogram:");
                for bucket in &plan.extension_histogram {
                    let label = if bucket.extension.is_empty() {
                        "(no extension)"
                    } else {
                        bucket.extension.as_str()
                    };
                    eprintln!(
                        "    {label}: {count} files, {bytes} bytes",
                        count = bucket.count,
                        bytes = bucket.total_bytes,
                    );
                }
            }
        }
        return;
    }

    eprintln!(
        "Imported {files} files ({bytes_source} bytes source) from {source} → {target}.",
        files = s.files_imported,
        bytes_source = s.bytes_source,
        source = s.source_url,
        target = s.target_url,
    );
    eprintln!(
        "  Commits: {commits} on {branch}; HEAD {head}; duration {ms} ms; same-bucket: {same}.",
        commits = s.commits_created,
        branch = s.branch,
        head = s.head_commit_oid.as_deref().unwrap_or("<none>"),
        ms = s.duration_ms,
        same = s.same_bucket,
    );
    if s.files_skipped > 0 || s.files_failed > 0 {
        eprintln!(
            "  Skipped: {skipped}; Failed: {failed}.",
            skipped = s.files_skipped,
            failed = s.files_failed,
        );
    }
    if s.lfs_resolved > 0 || s.lfs_skipped > 0 || s.lfs_failed > 0 {
        eprintln!(
            "  LFS: resolved {resolved}; skipped {skipped}; failed {failed}.",
            resolved = s.lfs_resolved,
            skipped = s.lfs_skipped,
            failed = s.lfs_failed,
        );
    }
}

/// Render a [`Cloud`] back to its raw URL scheme label for error
/// messages. Keeps [`CrabError::ImportSchemeMismatch`] self-
/// explanatory without leaking internal enum names.
fn scheme_label(cloud: Cloud) -> String {
    match cloud {
        Cloud::S3 => "s3".into(),
        Cloud::Gcs => "gs".into(),
        Cloud::Azure => "az".into(),
        Cloud::Local => "file".into(),
    }
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

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use tempfile::TempDir;

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
            jobs: None,
            fail_fast: false,
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
    fn positional_source_bucket_name_and_dest_prefix_parse() {
        let args = ImportArgs::parse_from([
            "import",
            "./large-files",
            "--bucket",
            "crab",
            "--name",
            "import-demo",
            "--dest-prefix",
            "crab/large-files",
        ]);
        assert_eq!(args.source.as_deref(), Some("./large-files"));
        assert_eq!(args.bucket.as_deref(), Some("crab"));
        assert_eq!(args.name.as_deref(), Some("import-demo"));
        assert_eq!(args.dest_prefix.as_deref(), Some("crab/large-files"));
    }

    #[test]
    fn lfs_source_mode_defaults_to_fail() {
        let args = ImportArgs::parse_from([
            "import",
            "--from",
            "s3://src-bucket/data",
            "--to",
            "s3://dst-bucket/repo",
        ]);
        assert_eq!(args.effective_lfs_source(), LfsSourceMode::Fail);
        assert_eq!(args.effective_lfs_objects(), None);
    }

    #[test]
    fn lfs_source_skip_parses() {
        let args = ImportArgs::parse_from([
            "import",
            "--from",
            "s3://src-bucket/data",
            "--to",
            "s3://dst-bucket/repo",
            "--lfs-source",
            "skip",
        ]);
        assert_eq!(args.effective_lfs_source(), LfsSourceMode::Skip);
    }

    #[test]
    fn lfs_source_resolve_uses_new_object_root() {
        let args = ImportArgs::parse_from([
            "import",
            "--from",
            "s3://src-bucket/data",
            "--to",
            "s3://dst-bucket/repo",
            "--lfs-source",
            "resolve",
            "--lfs-objects",
            "s3://src-bucket/lfs",
        ]);
        assert_eq!(args.effective_lfs_source(), LfsSourceMode::Resolve);
        assert_eq!(args.effective_lfs_objects(), Some("s3://src-bucket/lfs"));
    }

    #[test]
    fn shipped_lfs_import_aliases_still_map_to_resolve() {
        let args = ImportArgs::parse_from([
            "import",
            "--from",
            "s3://src-bucket/data",
            "--to",
            "s3://dst-bucket/repo",
            "--allow-lfs-import",
            "--lfs-store",
            "s3://src-bucket/lfs",
        ]);
        assert_eq!(args.effective_lfs_source(), LfsSourceMode::Resolve);
        assert_eq!(args.effective_lfs_objects(), Some("s3://src-bucket/lfs"));
    }

    #[test]
    fn resume_cli_accepts_into_without_urls() {
        let args = ImportArgs::parse_from(["import", "--into", "repo", "--resume"]);
        assert_eq!(args.from, None);
        assert_eq!(args.to, None);
        assert_eq!(args.into, Some(PathBuf::from("repo")));
        assert!(args.resume);
    }

    fn init_empty_repo_with_identity(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");

        for (key, val) in [
            ("user.name", "Crab Import"),
            ("user.email", "import@crab.dev"),
        ] {
            let status = Command::new("git")
                .args(["config", "--local", key, val])
                .current_dir(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git config");
            assert!(status.success(), "git config {key} failed");
        }
    }

    #[tokio::test]
    async fn missing_urls_without_resume_is_rejected() {
        let mut args = base_args("s3://src-bucket/data", "s3://dst-bucket/repo");
        args.from = None;
        args.to = None;
        let cancel = CancellationToken::new();
        let err = run_import(&args, &cancel).await.unwrap_err();
        assert!(
            err.to_string().contains("SOURCE or --from"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn raw_s3_from_with_raw_az_to_is_rejected() {
        // Cross-cloud raw target must surface as a scheme mismatch
        // before the pipeline kicks off. Users who genuinely want
        // cross-cloud imports have to write --to as crab://.
        let args = base_args("s3://src-bucket/data", "az://dst-container/repo");
        let cancel = CancellationToken::new();
        let err = run_import(&args, &cancel).await.unwrap_err();
        assert!(
            matches!(err, CrabError::ImportSchemeMismatch { .. }),
            "expected ImportSchemeMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn local_raw_import_dry_run_reaches_real_pipeline() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        let into = tmp.path().join("repo");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("large.bin"), b"not actually large, just real").unwrap();
        init_empty_repo_with_identity(&into);

        let mut args = base_args(&file_url(&source), &file_url(&target));
        args.into = Some(into);
        args.dry_run = true;
        let cancel = CancellationToken::new();
        run_import(&args, &cancel)
            .await
            .expect("local dry-run should reach the import plan");
    }

    #[tokio::test]
    async fn crab_from_is_rejected() {
        // `crab://` on --from is a wrong-command signal; the
        // correct command is `crab clone`.
        let args = base_args("crab://src-bucket/repo", "s3://dst-bucket/repo");
        let cancel = CancellationToken::new();
        let err = run_import(&args, &cancel).await.unwrap_err();
        assert!(
            matches!(err, CrabError::ImportSourceMustBeRaw { .. }),
            "expected ImportSourceMustBeRaw, got {err:?}"
        );
    }
}
