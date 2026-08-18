//! Source-oriented data commands for the documented DVC replacement profile.
//!
//! The namespace intentionally does not alias the existing raw object-store
//! `import` or self-update `update` commands. Each imported source receives a
//! credential-free, versioned descriptor before it can be refreshed.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use clap::{Args, Subcommand};
use futures_util::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crab_workflow::{
    SOURCE_DESCRIPTOR_SCHEMA_VERSION, SourceDescriptor, load_source_descriptor,
    save_source_descriptor, snapshot_payload,
};

/// Structured-output schema for `crab data` commands.
pub const DATA_SCHEMA: &str = "data";
const DATA_EVENT_SCHEMA: &str = "data.event";

/// Source-management data command family.
#[derive(Debug, Subcommand)]
pub enum DataCommand {
    /// List tracked pointers, workflow outputs, and source descriptors.
    List(DataListArgs),
    /// Import a path from another local repository or directory.
    Import(DataImportArgs),
    /// Import a URL into a tracked target and record its validator.
    ImportUrl(DataImportUrlArgs),
    /// Import a database snapshot through a named connector.
    ImportDb(DataImportDbArgs),
    /// Refresh one previously imported source transactionally.
    Update(DataUpdateArgs),
    /// Report local materialization, descriptor, and source freshness state.
    Status(DataStatusArgs),
}

#[derive(Debug, Args)]
pub struct DataListArgs {
    /// Optional path prefix relative to the worktree.
    pub path: Option<PathBuf>,
    /// Git revision selector. Remote Git transport is not inferred locally.
    #[arg(long)]
    pub rev: Option<String>,
    /// Recurse into directories.
    #[arg(long)]
    pub recursive: bool,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct DataImportArgs {
    /// Local source repository or directory.
    pub source: PathBuf,
    /// Source-relative path (defaults to the source root).
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Destination path in the current worktree.
    #[arg(long, short)]
    pub output: PathBuf,
    /// Locked source revision label for provenance.
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct DataImportUrlArgs {
    /// HTTP(S), object-store, or file URL.
    pub url: String,
    /// Destination path in the current worktree.
    #[arg(long, short)]
    pub output: PathBuf,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct DataImportDbArgs {
    /// Connector name. The bundled connector is sqlite.
    pub connector: String,
    /// Read-only SQLite database path.
    #[arg(long)]
    pub database: PathBuf,
    /// Canonical query text or query-file path.
    #[arg(long)]
    pub query: String,
    /// Destination path in the current worktree.
    #[arg(long, short)]
    pub output: PathBuf,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct DataUpdateArgs {
    /// Descriptor id or target path.
    pub target: String,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct DataStatusArgs {
    /// Optional descriptor id or target path filter.
    pub target: Option<String>,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

impl DataCommand {
    /// Resolve structured output flags once at the command boundary.
    pub fn output_mode(&self) -> OutputMode {
        match self {
            Self::List(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Import(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::ImportUrl(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::ImportDb(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Update(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Status(args) => OutputMode::from_flags(args.json, args.jsonl),
        }
    }

    /// Execute a data command after parsing has completed.
    pub fn run(self) -> Result<()> {
        let cwd = std::env::current_dir().map_err(CrabError::Io)?;
        let root = resolve_data_root(&cwd)?;
        match self {
            Self::List(args) => run_list(&root, &args),
            Self::Import(args) => run_import(&root, &args),
            Self::ImportUrl(args) => run_import_url(&root, &args),
            Self::ImportDb(args) => run_import_db(&root, &args),
            Self::Update(args) => run_update(&root, &args),
            Self::Status(args) => run_status(&root, &args),
        }
    }
}

fn resolve_data_root(start: &Path) -> Result<PathBuf> {
    crate::git::worktree::WorktreeContext::resolve_from_path(start)
        .map(|context| context.current_worktree_root)
}

#[derive(Debug, Serialize)]
struct DataEntry {
    path: String,
    kind: String,
    size: Option<u64>,
    source_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DataStatusEntry {
    path: String,
    kind: String,
    size: Option<u64>,
    source_id: Option<String>,
    /// Stable per-dimension states; network-backed dimensions are explicit when not checked.
    dimensions: BTreeMap<&'static str, &'static str>,
}

#[derive(Debug, Serialize)]
struct DataPayload<T> {
    operation: &'static str,
    entries: T,
}

#[derive(Debug, Serialize)]
struct ImportPayload {
    descriptor: SourceDescriptor,
    changed: bool,
    dry_run: bool,
}

fn run_list(root: &Path, args: &DataListArgs) -> Result<()> {
    let prefix = args.path.as_deref().unwrap_or_else(|| Path::new("."));
    if prefix != Path::new(".") {
        ensure_safe_relative(prefix)?;
    }
    let source_ids = load_descriptors(root)?
        .into_iter()
        .map(|descriptor| (descriptor.target, descriptor.id))
        .collect::<BTreeMap<_, _>>();
    let mut entries = if let Some(revision) = args.rev.as_deref() {
        let commit = resolve_git_revision(root, revision)?;
        collect_git_entries(root, &commit, prefix, args.recursive, &source_ids)?
    } else {
        let mut entries = Vec::new();
        collect_entries(root, prefix, args.recursive, &source_ids, &mut entries)?;
        entries
    };
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    emit(
        OutputMode::from_flags(args.json, args.jsonl),
        DataPayload {
            operation: "list",
            entries,
        },
        |payload| {
            for entry in &payload.entries {
                println!("{}\t{}", entry.kind, entry.path);
            }
        },
    );
    Ok(())
}

fn run_import(root: &Path, args: &DataImportArgs) -> Result<()> {
    let target = safe_target(root, &args.output)?;
    reject_existing_target(&target)?;
    let source_root = canonicalize_source(&args.source)?;
    if let Some(path) = &args.path {
        ensure_safe_relative(path)?;
    }
    let source_path = args.path.as_deref().unwrap_or_else(|| Path::new("."));
    let (hash, size, revision, source_relative, temporary_source) =
        if let Some(requested_revision) = args.rev.as_deref() {
            let source_relative = normalize_git_relative_path(source_path)?;
            let commit = resolve_git_revision(&source_root, requested_revision)?;
            let temporary = temporary_target(&target);
            let (hash, size) =
                materialize_git_revision_path(&source_root, &commit, &source_relative, &temporary)?;
            (hash, size, Some(commit), source_relative, Some(temporary))
        } else {
            let source = source_root.join(source_path);
            let source = canonicalize_source(&source)?;
            if !source.starts_with(&source_root) {
                return Err(CrabError::Configuration {
                    key: "data_import_source_unsafe".into(),
                    origin: source_path.display().to_string(),
                });
            }
            if !source.exists() {
                return Err(CrabError::Configuration {
                    key: "data_import_source_missing".into(),
                    origin: source.display().to_string(),
                });
            }
            let (hash, size) = hash_path(&source)?;
            let temporary = temporary_target(&target);
            materialize_from_path(&source, &temporary)?;
            (
                hash,
                size,
                args.rev.clone().or_else(|| git_head(&source_root)),
                source_relative_path(&source, &source_root),
                Some(temporary),
            )
        };
    let source = temporary_source
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "data_import_source_missing".into(),
            origin: source_path.display().to_string(),
        })?;
    if let Err(error) = verify_materialized(source, hash, size) {
        let _ = remove_existing_path(source);
        return Err(error);
    }
    if let Err(error) = install_new_payload(source, &target) {
        let _ = remove_existing_path(source);
        return Err(error);
    }
    let mut descriptor = descriptor_for(
        "repo",
        source_root.to_string_lossy().as_ref(),
        revision,
        None,
        hash,
        size,
        &args.output,
    );
    if source_relative != "." {
        descriptor
            .metadata
            .insert("source_path".to_owned(), source_relative);
    }
    if let Some(requested_revision) = args.rev.as_deref() {
        descriptor.metadata.insert(
            "requested_revision".to_owned(),
            requested_revision.to_owned(),
        );
    }
    if let Err(error) = save_descriptor(root, &descriptor) {
        let _ = remove_existing_path(&target);
        return Err(error);
    }
    emit_import(
        OutputMode::from_flags(args.json, args.jsonl),
        descriptor,
        true,
        false,
    );
    Ok(())
}

fn run_import_url(root: &Path, args: &DataImportUrlArgs) -> Result<()> {
    let target = safe_target(root, &args.output)?;
    reject_existing_target(&target)?;
    let parsed = url::Url::parse(&args.url).map_err(|error| CrabError::Configuration {
        key: "data_import_url_invalid".into(),
        origin: error.to_string(),
    })?;
    if url_has_embedded_secret(&parsed) {
        return Err(CrabError::Configuration {
            key: "data_import_url_credentials_unsupported".into(),
            origin: "use a configured credential provider; credentials in source URLs cannot be persisted".into(),
        });
    }
    let locator = redact_url(&parsed);
    let temporary = temporary_target(&target);
    let mut temporary_cleanup = TemporaryPathCleanup::new(temporary.clone());
    let (hash, size, validator) = match parsed.scheme() {
        "file" => {
            let source = parsed
                .to_file_path()
                .map_err(|()| CrabError::Configuration {
                    key: "data_import_url_invalid".into(),
                    origin: "file URL does not resolve to a local path".into(),
                })?;
            let source = canonicalize_source(&source)?;
            let (hash, size) = hash_path(&source)?;
            materialize_from_path(&source, &temporary)?;
            (hash, size, None)
        }
        "http" | "https" => {
            let response = reqwest::blocking::Client::new()
                .get(parsed.as_str())
                .send()
                .map_err(|error| {
                    CrabError::NetworkTransient(object_store::Error::Generic {
                        store: "crab data import-url",
                        source: Box::new(error),
                    })
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(CrabError::Configuration {
                    key: "data_import_url_http_error".into(),
                    origin: status.to_string(),
                });
            }
            let validator = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.trim_start().starts_with("W/"))
                .map(ToOwned::to_owned);
            let parent = temporary.parent().unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
            let mut file = File::create(&temporary).map_err(CrabError::Io)?;
            let (hash, size) = stream_to_hash(response, &mut file)?;
            file.sync_all().map_err(CrabError::Io)?;
            (hash, size, validator)
        }
        "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl" => {
            let (store, location) = parse_object_store_url(&parsed)?;
            let (_, size, validator) =
                fetch_object_store_object(store, location, temporary.clone())?;
            let (hash, verified_size) = hash_path(&temporary)?;
            if verified_size != size {
                return Err(CrabError::Configuration {
                    key: "data_import_url_size_mismatch".into(),
                    origin: locator.clone(),
                });
            }
            (hash, size, validator)
        }
        scheme => {
            return Err(CrabError::Configuration {
                key: "data_import_provider_unsupported".into(),
                origin: scheme.to_owned(),
            });
        }
    };
    verify_materialized(&temporary, hash, size)?;
    let swap = TargetSwap::apply(&temporary, &target)?;
    let descriptor = descriptor_for("url", &locator, None, validator, hash, size, &args.output);
    if let Err(error) = save_descriptor(root, &descriptor) {
        drop(swap);
        return Err(error);
    }
    swap.finish();
    temporary_cleanup.disarm();
    emit_import(
        OutputMode::from_flags(args.json, args.jsonl),
        descriptor,
        true,
        false,
    );
    Ok(())
}

fn run_import_db(root: &Path, args: &DataImportDbArgs) -> Result<()> {
    if !args.connector.eq_ignore_ascii_case("sqlite") {
        return Err(CrabError::Configuration {
            key: "data_import_db_connector_unsupported".into(),
            origin: args.connector.clone(),
        });
    }
    let target = safe_target(root, &args.output)?;
    reject_existing_target(&target)?;
    let database = canonicalize_source(&args.database)?;
    if !database.is_file() {
        return Err(CrabError::Configuration {
            key: "data_import_db_source_invalid".into(),
            origin: database.display().to_string(),
        });
    }
    let query = load_db_query(&args.query)?;
    let temporary = temporary_target(&target);
    let mut cleanup = TemporaryPathCleanup::new(temporary.clone());
    write_sqlite_jsonl(&database, &query, &temporary)?;
    let (hash, size) = hash_path(&temporary)?;
    let mut descriptor = descriptor_for(
        "sqlite",
        &database.to_string_lossy(),
        Some(format!("b3:{}", blake3::hash(query.as_bytes()).to_hex())),
        None,
        hash,
        size,
        &args.output,
    );
    descriptor
        .metadata
        .insert("query".to_owned(), query.clone());
    if let Err(error) = install_new_payload(&temporary, &target) {
        return Err(error);
    }
    cleanup.disarm();
    if let Err(error) = save_descriptor(root, &descriptor) {
        let _ = remove_existing_path(&target);
        return Err(error);
    }
    emit_import(
        OutputMode::from_flags(args.json, args.jsonl),
        descriptor,
        true,
        false,
    );
    Ok(())
}

fn load_db_query(raw: &str) -> Result<String> {
    let query = if Path::new(raw).is_file() {
        fs::read_to_string(raw).map_err(CrabError::Io)?
    } else {
        raw.to_owned()
    };
    let query = query.trim().to_owned();
    if query.is_empty() || query.len() > 1024 || query.chars().any(char::is_control) {
        return Err(CrabError::Configuration {
            key: "data_import_db_query_invalid".into(),
            origin: "query must be a non-empty UTF-8 string no longer than 1024 bytes".into(),
        });
    }
    Ok(query)
}

fn write_sqlite_jsonl(database: &Path, query: &str, destination: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| CrabError::Configuration {
            key: "data_import_db_open_failed".into(),
            origin: error.to_string(),
        })?;
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .map_err(|error| CrabError::Configuration {
            key: "data_import_db_read_only_failed".into(),
            origin: error.to_string(),
        })?;
    let mut statement = connection
        .prepare(query)
        .map_err(|error| CrabError::Configuration {
            key: "data_import_db_query_failed".into(),
            origin: error.to_string(),
        })?;
    let columns = statement
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if columns
        .iter()
        .any(|column| column.chars().any(char::is_control))
    {
        return Err(CrabError::Configuration {
            key: "data_import_db_column_invalid".into(),
            origin: "query returned a control character in a column name".into(),
        });
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let mut output = File::create(destination).map_err(CrabError::Io)?;
    let mut rows = statement
        .query([])
        .map_err(|error| CrabError::Configuration {
            key: "data_import_db_query_failed".into(),
            origin: error.to_string(),
        })?;
    while let Some(row) = rows.next().map_err(|error| CrabError::Configuration {
        key: "data_import_db_row_failed".into(),
        origin: error.to_string(),
    })? {
        let mut values = BTreeMap::new();
        for (index, column) in columns.iter().enumerate() {
            let value = row
                .get_ref(index)
                .map_err(|error| CrabError::Configuration {
                    key: "data_import_db_row_failed".into(),
                    origin: error.to_string(),
                })?;
            values.insert(column.clone(), sqlite_json_value(value));
        }
        serde_json::to_writer(&mut output, &values).map_err(|error| CrabError::Configuration {
            key: "data_import_db_serialize_failed".into(),
            origin: error.to_string(),
        })?;
        output.write_all(b"\n").map_err(CrabError::Io)?;
    }
    output.sync_all().map_err(CrabError::Io)
}

fn sqlite_json_value(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(value) => serde_json::Value::from(value),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        ValueRef::Text(value) => {
            serde_json::Value::String(String::from_utf8_lossy(value).into_owned())
        }
        ValueRef::Blob(value) => serde_json::json!({
            "$binary_base64": BASE64_STANDARD.encode(value),
        }),
    }
}

fn run_update(root: &Path, args: &DataUpdateArgs) -> Result<()> {
    let descriptor = find_descriptor(root, &args.target)?;
    let target = safe_target(root, Path::new(&descriptor.target))?;
    let mut fetched = fetch_descriptor_source(&descriptor, &target)?;
    let content_hash = format!("b3:{}", hex(&fetched.hash));
    let changed = content_hash != descriptor.content_hash
        || fetched.size != descriptor.size
        || fetched.revision != descriptor.revision
        || fetched.validator != descriptor.validator;
    if changed && !args.dry_run {
        let temporary = fetched
            .temporary
            .take()
            .ok_or_else(|| CrabError::Configuration {
                key: "data_update_source_missing".into(),
                origin: descriptor.target.clone(),
            })?;
        let swap = match TargetSwap::apply(&temporary, &target) {
            Ok(swap) => swap,
            Err(error) => {
                remove_temporary(&temporary);
                return Err(error);
            }
        };
        let mut updated = descriptor.clone();
        updated.content_hash = content_hash;
        updated.size = fetched.size;
        updated.validator = fetched.validator;
        updated.revision = fetched.revision;
        if let Err(error) = save_descriptor(root, &updated) {
            drop(swap);
            return Err(error);
        }
        swap.finish();
    } else if let Some(temporary) = fetched.temporary.take() {
        remove_temporary(&temporary);
    }
    let payload = DataPayload {
        operation: "update",
        entries: ImportPayload {
            descriptor: if changed && !args.dry_run {
                find_descriptor(root, &args.target)?
            } else {
                descriptor
            },
            changed,
            dry_run: args.dry_run,
        },
    };
    emit(
        OutputMode::from_flags(args.json, args.jsonl),
        payload,
        |payload| {
            println!(
                "{}",
                if payload.entries.changed {
                    "changed"
                } else {
                    "up-to-date"
                }
            );
        },
    );
    Ok(())
}

fn run_status(root: &Path, args: &DataStatusArgs) -> Result<()> {
    let descriptors = load_descriptors(root)?;
    let entries = descriptors
        .into_iter()
        .filter(|descriptor| {
            args.target
                .as_deref()
                .is_none_or(|target| descriptor.id == target || descriptor.target == target)
        })
        .map(|descriptor| status_entry(root, descriptor))
        .collect::<Vec<_>>();
    emit(
        OutputMode::from_flags(args.json, args.jsonl),
        DataPayload {
            operation: "status",
            entries,
        },
        |payload| {
            for entry in &payload.entries {
                let dimensions = entry
                    .dimensions
                    .iter()
                    .map(|(name, state)| format!("{name}={state}"))
                    .collect::<Vec<_>>()
                    .join(",");
                println!("{}\t{}\t{}", entry.kind, entry.path, dimensions);
            }
        },
    );
    Ok(())
}

fn status_entry(root: &Path, descriptor: SourceDescriptor) -> DataStatusEntry {
    let workspace = match safe_target(root, Path::new(&descriptor.target)) {
        Err(_) => "unsafe",
        Ok(target) if !target.exists() => "missing",
        Ok(target) => match hash_path(&target) {
            Ok((hash, _)) if format!("b3:{}", hex(&hash)) == descriptor.content_hash => {
                "up-to-date"
            }
            Ok(_) => "changed",
            Err(_) => "unreadable",
        },
    };
    let target = Path::new(&descriptor.target);
    let mut dimensions = BTreeMap::new();
    dimensions.insert("workspace", workspace);
    dimensions.insert("descriptor", "present");
    dimensions.insert("git", git_worktree_state(root, target));
    dimensions.insert("cache", "not-managed");
    dimensions.insert("remote", "not-checked");
    dimensions.insert("source", "not-checked");
    dimensions.insert(
        "lock",
        if descriptor.revision.is_some() || descriptor.validator.is_some() {
            "locked"
        } else {
            "unlocked"
        },
    );
    DataStatusEntry {
        path: descriptor.target,
        kind: format!("source:{workspace}"),
        size: Some(descriptor.size),
        source_id: Some(descriptor.id),
        dimensions,
    }
}

fn git_worktree_state(root: &Path, target: &Path) -> &'static str {
    let output = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain=v1", "--untracked-files=all", "--"])
        .arg(target)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            if output.stdout.is_empty() {
                "clean"
            } else {
                "changed"
            }
        }
        _ => "unavailable",
    }
}

struct FetchedSource {
    hash: [u8; 32],
    size: u64,
    revision: Option<String>,
    validator: Option<String>,
    temporary: Option<PathBuf>,
}

fn fetch_descriptor_source(descriptor: &SourceDescriptor, target: &Path) -> Result<FetchedSource> {
    let temporary = temporary_target(target);
    let mut temporary_cleanup = TemporaryPathCleanup::new(temporary.clone());
    match descriptor.kind.as_str() {
        "repo" => {
            let source_root = PathBuf::from(&descriptor.locator);
            let source_root = canonicalize_source(&source_root)?;
            if let Some(requested_revision) = descriptor.metadata.get("requested_revision") {
                let source_path = descriptor
                    .metadata
                    .get("source_path")
                    .map(|path| PathBuf::from(path.as_str()))
                    .ok_or_else(|| CrabError::Configuration {
                        key: "data_source_path_missing".into(),
                        origin: descriptor.target.clone(),
                    })?;
                let source_relative = normalize_git_relative_path(&source_path)?;
                let revision = resolve_git_revision(&source_root, requested_revision)?;
                let (hash, size) = materialize_git_revision_path(
                    &source_root,
                    &revision,
                    &source_relative,
                    &temporary,
                )?;
                temporary_cleanup.disarm();
                Ok(FetchedSource {
                    hash,
                    size,
                    revision: Some(revision),
                    validator: None,
                    temporary: Some(temporary),
                })
            } else {
                let source = if let Some(source_path) = descriptor.metadata.get("source_path") {
                    let source_relative = PathBuf::from(source_path);
                    ensure_safe_relative(&source_relative)?;
                    source_root.join(source_relative)
                } else {
                    source_root.clone()
                };
                let source = canonicalize_source(&source)?;
                if !source.starts_with(&source_root) {
                    return Err(CrabError::Configuration {
                        key: "data_source_path_unsafe".into(),
                        origin: descriptor.target.clone(),
                    });
                }
                let (hash, size) = hash_path(&source)?;
                materialize_from_path(&source, &temporary)?;
                temporary_cleanup.disarm();
                Ok(FetchedSource {
                    hash,
                    size,
                    // A repository source imported without an explicit ref
                    // follows its current HEAD. Refresh that lock when the
                    // source advances instead of retaining stale provenance.
                    revision: git_head(&source_root).or_else(|| descriptor.revision.clone()),
                    validator: descriptor.validator.clone(),
                    temporary: Some(temporary),
                })
            }
        }
        "url" => {
            let url =
                url::Url::parse(&descriptor.locator).map_err(|error| CrabError::Configuration {
                    key: "data_source_locator_invalid".into(),
                    origin: error.to_string(),
                })?;
            match url.scheme() {
                "file" => {
                    let source = url.to_file_path().map_err(|()| CrabError::Configuration {
                        key: "data_source_locator_invalid".into(),
                        origin: descriptor.locator.clone(),
                    })?;
                    let source = canonicalize_source(&source)?;
                    let (hash, size) = hash_path(&source)?;
                    materialize_from_path(&source, &temporary)?;
                    temporary_cleanup.disarm();
                    Ok(FetchedSource {
                        hash,
                        size,
                        revision: descriptor.revision.clone(),
                        validator: None,
                        temporary: Some(temporary),
                    })
                }
                "http" | "https" => {
                    let client = reqwest::blocking::Client::new();
                    let mut request = client.get(url.as_str());
                    if let Some(validator) = descriptor.validator.as_deref() {
                        request = request.header(reqwest::header::IF_NONE_MATCH, validator);
                    }
                    let response = request.send().map_err(|error| {
                        CrabError::NetworkTransient(object_store::Error::Generic {
                            store: "crab data update",
                            source: Box::new(error),
                        })
                    })?;
                    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                        let (hash, size) = hash_path(target)?;
                        return Ok(FetchedSource {
                            hash,
                            size,
                            revision: descriptor.revision.clone(),
                            validator: descriptor.validator.clone(),
                            temporary: None,
                        });
                    }
                    if !response.status().is_success() {
                        return Err(CrabError::Configuration {
                            key: "data_update_url_http_error".into(),
                            origin: response.status().to_string(),
                        });
                    }
                    let validator = response
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| !value.trim_start().starts_with("W/"))
                        .map(ToOwned::to_owned);
                    let parent = temporary.parent().unwrap_or_else(|| Path::new("."));
                    fs::create_dir_all(parent).map_err(CrabError::Io)?;
                    let mut file = File::create(&temporary).map_err(CrabError::Io)?;
                    let (hash, size) = stream_to_hash(response, &mut file)?;
                    file.sync_all().map_err(CrabError::Io)?;
                    temporary_cleanup.disarm();
                    Ok(FetchedSource {
                        hash,
                        size,
                        revision: descriptor.revision.clone(),
                        validator,
                        temporary: Some(temporary),
                    })
                }
                "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl" => {
                    let (store, location) = parse_object_store_url(&url)?;
                    let metadata = crate::cmd::lfs::block_on_runtime({
                        let store = store.as_ref();
                        let location = location.clone();
                        async move { store.head(&location).await.map_err(CrabError::Storage) }
                    })?;
                    let validator = object_store_validator(&metadata);
                    if target.exists()
                        && descriptor.size == metadata.size
                        && descriptor.validator.as_deref() == validator.as_deref()
                        && validator.is_some()
                    {
                        let (hash, size) = hash_path(target)?;
                        return Ok(FetchedSource {
                            hash,
                            size,
                            revision: descriptor.revision.clone(),
                            validator,
                            temporary: None,
                        });
                    }
                    let (_, size, validator) =
                        fetch_object_store_object(store, location, temporary.clone())?;
                    let (hash, verified_size) = hash_path(&temporary)?;
                    if size != verified_size {
                        return Err(CrabError::Configuration {
                            key: "data_update_url_size_mismatch".into(),
                            origin: descriptor.locator.clone(),
                        });
                    }
                    temporary_cleanup.disarm();
                    Ok(FetchedSource {
                        hash,
                        size,
                        revision: descriptor.revision.clone(),
                        validator,
                        temporary: Some(temporary),
                    })
                }
                scheme => Err(CrabError::Configuration {
                    key: "data_update_provider_unsupported".into(),
                    origin: scheme.to_owned(),
                }),
            }
        }
        "sqlite" => {
            let database = canonicalize_source(Path::new(&descriptor.locator))?;
            let query =
                descriptor
                    .metadata
                    .get("query")
                    .ok_or_else(|| CrabError::Configuration {
                        key: "data_source_query_missing".into(),
                        origin: descriptor.target.clone(),
                    })?;
            write_sqlite_jsonl(&database, query, &temporary)?;
            let (hash, size) = hash_path(&temporary)?;
            temporary_cleanup.disarm();
            Ok(FetchedSource {
                hash,
                size,
                revision: Some(format!("b3:{}", blake3::hash(query.as_bytes()).to_hex())),
                validator: None,
                temporary: Some(temporary),
            })
        }
        _ => Err(CrabError::Configuration {
            key: "data_update_source_unsupported".into(),
            origin: descriptor.kind.clone(),
        }),
    }
}

fn parse_object_store_url(url: &url::Url) -> Result<(Box<dyn ObjectStore>, ObjectPath)> {
    let options = std::env::vars()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect::<Vec<_>>();
    object_store::parse_url_opts(
        url,
        options
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(CrabError::Storage)
}

fn fetch_object_store_object(
    store: Box<dyn ObjectStore>,
    location: ObjectPath,
    temporary: PathBuf,
) -> Result<([u8; 32], u64, Option<String>)> {
    crate::cmd::lfs::block_on_runtime(async move {
        let metadata = store.head(&location).await.map_err(CrabError::Storage)?;
        let result = store.get(&location).await.map_err(CrabError::Storage)?;
        let parent = temporary.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(CrabError::Io)?;
        let mut file = tokio::fs::File::create(&temporary)
            .await
            .map_err(CrabError::Io)?;
        let mut stream = result.into_stream();
        let mut hasher = blake3::Hasher::new();
        let mut size = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(CrabError::Storage)?;
            file.write_all(&chunk).await.map_err(CrabError::Io)?;
            hasher.update(&chunk);
            size = size.saturating_add(chunk.len() as u64);
        }
        file.sync_all().await.map_err(CrabError::Io)?;
        let validator = object_store_validator(&metadata);
        if size != metadata.size {
            return Err(CrabError::Configuration {
                key: "data_import_url_size_mismatch".into(),
                origin: location.to_string(),
            });
        }
        Ok((*hasher.finalize().as_bytes(), size, validator))
    })
}

fn object_store_validator(metadata: &object_store::ObjectMeta) -> Option<String> {
    metadata.version.clone().or_else(|| {
        metadata
            .e_tag
            .clone()
            .filter(|value| !value.trim_start().starts_with("W/"))
    })
}

fn descriptor_for(
    kind: &str,
    locator: &str,
    revision: Option<String>,
    validator: Option<String>,
    hash: [u8; 32],
    size: u64,
    target: &Path,
) -> SourceDescriptor {
    let id = blake3::hash(format!("{kind}\0{locator}\0{}", target.display()).as_bytes())
        .to_hex()
        .to_string();
    SourceDescriptor {
        schema_version: SOURCE_DESCRIPTOR_SCHEMA_VERSION,
        id,
        kind: kind.to_owned(),
        locator: locator.to_owned(),
        revision,
        validator,
        content_hash: format!("b3:{}", hex(&hash)),
        size,
        target: target.to_string_lossy().replace('\\', "/"),
        metadata: BTreeMap::new(),
    }
}

fn load_descriptors(root: &Path) -> Result<Vec<SourceDescriptor>> {
    let directory = root.join(".crab/workflow/sources");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut descriptors = Vec::new();
    for entry in fs::read_dir(directory).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        if entry.file_type().map_err(CrabError::Io)?.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            descriptors.push(load_source_descriptor(&entry.path())?);
        }
    }
    descriptors.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(descriptors)
}

fn find_descriptor(root: &Path, target: &str) -> Result<SourceDescriptor> {
    load_descriptors(root)?
        .into_iter()
        .find(|descriptor| descriptor.id == target || descriptor.target == target)
        .ok_or_else(|| CrabError::Configuration {
            key: "data_source_not_found".into(),
            origin: target.to_owned(),
        })
}

fn save_descriptor(root: &Path, descriptor: &SourceDescriptor) -> Result<()> {
    save_source_descriptor(
        &root
            .join(".crab/workflow/sources")
            .join(format!("{}.json", descriptor.id)),
        descriptor,
    )
    .map_err(|error| CrabError::Configuration {
        key: "data_source_descriptor_write".into(),
        origin: error.to_string(),
    })
}

fn collect_entries(
    root: &Path,
    relative: &Path,
    recursive: bool,
    source_ids: &BTreeMap<String, String>,
    entries: &mut Vec<DataEntry>,
) -> Result<()> {
    let full = root.join(relative);
    let metadata = fs::symlink_metadata(&full).map_err(CrabError::Io)?;
    if metadata.is_file() {
        let path = normalize_relative_path(relative);
        entries.push(DataEntry {
            source_id: source_ids.get(&path).cloned(),
            path,
            kind: "file".to_owned(),
            size: Some(metadata.len()),
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut children = fs::read_dir(&full)
        .map_err(CrabError::Io)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            name != ".git" && name != ".crab" && name != "target"
        })
        .collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let child = relative.join(entry.file_name());
        let child_metadata = fs::symlink_metadata(entry.path()).map_err(CrabError::Io)?;
        if child_metadata.is_file() {
            let path = normalize_relative_path(&child);
            entries.push(DataEntry {
                source_id: source_ids.get(&path).cloned(),
                path,
                kind: "file".to_owned(),
                size: Some(child_metadata.len()),
            });
        } else if child_metadata.is_dir() {
            if recursive {
                collect_entries(root, &child, true, source_ids, entries)?;
            } else {
                entries.push(DataEntry {
                    path: normalize_relative_path(&child),
                    kind: "directory".to_owned(),
                    size: None,
                    source_id: None,
                });
            }
        }
    }
    Ok(())
}

fn collect_git_entries(
    root: &Path,
    commit: &str,
    prefix: &Path,
    recursive: bool,
    source_ids: &BTreeMap<String, String>,
) -> Result<Vec<DataEntry>> {
    let prefix = normalize_git_relative_path(prefix)?;
    let prefix_kind = if prefix != "." {
        let object = format!("{commit}:{prefix}");
        let kind = git_output(root, &["cat-file", "-t", &object])?;
        if kind.trim() != "tree" && kind.trim() != "blob" {
            return Err(CrabError::Configuration {
                key: "data_list_revision_path_missing".into(),
                origin: prefix,
            });
        }
        Some(kind.trim().to_owned())
    } else {
        None
    };
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["ls-tree", "-z", "-r", "--long", commit]);
    if prefix != "." {
        command.arg("--").arg(&prefix);
    }
    let output = command.output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "data_list_revision_failed".into(),
            origin: prefix,
        });
    }
    let mut files = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let (header, path) = split_tree_record(record)?;
        let fields = split_ascii_fields(header);
        if fields.len() != 4 {
            return Err(CrabError::Configuration {
                key: "data_list_revision_invalid".into(),
                origin: "Git tree entry is malformed".into(),
            });
        }
        let kind = String::from_utf8(fields[1].to_vec()).map_err(|_| CrabError::Configuration {
            key: "data_list_revision_invalid".into(),
            origin: "Git tree entry has an invalid type".into(),
        })?;
        let path = String::from_utf8(path.to_vec()).map_err(|_| CrabError::Configuration {
            key: "data_list_revision_invalid".into(),
            origin: "Git tree entry has an invalid path".into(),
        })?;
        let size = if fields[3] == b"-" {
            None
        } else {
            Some(
                std::str::from_utf8(fields[3])
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| CrabError::Configuration {
                        key: "data_list_revision_invalid".into(),
                        origin: "Git tree entry has an invalid size".into(),
                    })?,
            )
        };
        files.push((path, kind, size));
    }
    if prefix_kind.as_deref() == Some("blob") {
        return Ok(files
            .into_iter()
            .map(|(path, kind, size)| DataEntry {
                source_id: source_ids.get(&path).cloned(),
                path,
                kind: if kind == "blob" {
                    "file".to_owned()
                } else {
                    kind
                },
                size,
            })
            .collect());
    }
    if recursive {
        return Ok(files
            .into_iter()
            .map(|(path, kind, size)| DataEntry {
                source_id: source_ids.get(&path).cloned(),
                path,
                kind: if kind == "blob" {
                    "file".to_owned()
                } else {
                    kind
                },
                size,
            })
            .collect());
    }

    let prefix = (prefix != ".").then_some(format!("{prefix}/"));
    let mut entries = BTreeMap::<String, DataEntry>::new();
    for (path, kind, size) in files {
        let relative = match prefix.as_deref() {
            Some(prefix) if path == prefix.trim_end_matches('/') => continue,
            Some(prefix) => path.strip_prefix(prefix).unwrap_or_default(),
            None => path.as_str(),
        };
        if relative.is_empty() {
            continue;
        }
        let child = relative.split('/').next().unwrap_or(relative);
        let child_path = match prefix.as_deref() {
            Some(prefix) => format!("{}{}", prefix, child),
            None => child.to_owned(),
        };
        let is_direct_file = relative == child;
        if is_direct_file {
            entries.insert(
                child_path.clone(),
                DataEntry {
                    path: child_path.clone(),
                    kind: if kind == "blob" {
                        "file".to_owned()
                    } else {
                        kind
                    },
                    size,
                    source_id: source_ids.get(&child_path).cloned(),
                },
            );
        } else {
            entries
                .entry(child_path.clone())
                .or_insert_with(|| DataEntry {
                    path: child_path,
                    kind: "directory".to_owned(),
                    size: None,
                    source_id: None,
                });
        }
    }
    Ok(entries.into_values().collect())
}

fn resolve_git_revision(root: &Path, revision: &str) -> Result<String> {
    if revision.trim().is_empty() {
        return Err(CrabError::Configuration {
            key: "data_git_revision_invalid".into(),
            origin: "revision is empty".into(),
        });
    }
    let selector = format!("{revision}^{{commit}}");
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(selector)
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "data_git_revision_invalid".into(),
            origin: revision.to_owned(),
        });
    }
    let commit = String::from_utf8(output.stdout).map_err(|_| CrabError::Configuration {
        key: "data_git_revision_invalid".into(),
        origin: revision.to_owned(),
    })?;
    let commit = commit.trim();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CrabError::Configuration {
            key: "data_git_revision_invalid".into(),
            origin: revision.to_owned(),
        });
    }
    Ok(commit.to_owned())
}

fn git_head(root: &Path) -> Option<String> {
    resolve_git_revision(root, "HEAD").ok()
}

fn normalize_git_relative_path(path: &Path) -> Result<String> {
    if path == Path::new(".") {
        return Ok(".".to_owned());
    }
    ensure_safe_relative(path)?;
    let normalized = normalize_relative_path(path);
    if normalized.is_empty() {
        return Err(CrabError::Configuration {
            key: "data_path_unsafe".into(),
            origin: path.display().to_string(),
        });
    }
    Ok(normalized)
}

fn source_relative_path(source: &Path, root: &Path) -> String {
    source
        .strip_prefix(root)
        .ok()
        .map(normalize_relative_path)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| ".".to_owned())
}

fn materialize_git_revision_path(
    root: &Path,
    commit: &str,
    relative: &str,
    target: &Path,
) -> Result<([u8; 32], u64)> {
    reject_existing_target(target)?;
    let mut cleanup = TemporaryPathCleanup::new(target.to_owned());
    if relative == "." {
        let recursive =
            git_output_bytes(root, &["ls-tree", "-z", "-r", "--long", commit, "--", "."])?;
        fs::create_dir_all(target).map_err(CrabError::Io)?;
        materialize_git_tree_records(root, commit, relative, &recursive, target)?;
        let result = hash_path(target)?;
        cleanup.disarm();
        return Ok(result);
    }

    let selected = git_output_bytes(root, &["ls-tree", "-z", "--long", commit, "--", relative])?;
    let records = selected
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let record = records.first().ok_or_else(|| CrabError::Configuration {
        key: "data_import_revision_path_missing".into(),
        origin: relative.to_owned(),
    })?;
    if records.len() != 1 {
        return Err(CrabError::Configuration {
            key: "data_import_revision_path_invalid".into(),
            origin: relative.to_owned(),
        });
    }
    let (header, path) = split_tree_record(record)?;
    let fields = split_ascii_fields(header);
    if fields.len() != 4 {
        return Err(CrabError::Configuration {
            key: "data_import_revision_invalid".into(),
            origin: "Git tree entry is malformed".into(),
        });
    }
    if fields[1] == b"blob" {
        if path != relative.as_bytes() {
            return Err(CrabError::Configuration {
                key: "data_import_revision_path_invalid".into(),
                origin: relative.to_owned(),
            });
        }
        let mode = parse_git_file_mode(fields[0], relative)?;
        let object = format!("{commit}:{relative}");
        let result = materialize_git_blob_object(root, &object, relative, mode, target)?;
        cleanup.disarm();
        return Ok(result);
    }
    if fields[1] != b"tree" || path != relative.as_bytes() {
        return Err(CrabError::Configuration {
            key: "data_import_revision_path_unsupported".into(),
            origin: relative.to_owned(),
        });
    }

    let recursive = git_output_bytes(
        root,
        &["ls-tree", "-z", "-r", "--long", commit, "--", relative],
    )?;
    fs::create_dir_all(target).map_err(CrabError::Io)?;
    materialize_git_tree_records(root, commit, relative, &recursive, target)?;
    let result = hash_path(target)?;
    cleanup.disarm();
    Ok(result)
}

fn materialize_git_tree_records(
    root: &Path,
    commit: &str,
    relative: &str,
    records: &[u8],
    target: &Path,
) -> Result<()> {
    let prefix = (relative != ".").then_some(format!("{relative}/"));
    for record in records
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let (header, path) = split_tree_record(record)?;
        let fields = split_ascii_fields(header);
        if fields.len() != 4 || fields[1] != b"blob" {
            return Err(CrabError::Configuration {
                key: "data_import_revision_entry_unsupported".into(),
                origin: relative.to_owned(),
            });
        }
        let path = String::from_utf8(path.to_vec()).map_err(|_| CrabError::Configuration {
            key: "data_import_revision_invalid".into(),
            origin: "Git tree entry has an invalid path".into(),
        })?;
        let relative_file = match prefix.as_deref() {
            Some(prefix) => path
                .strip_prefix(prefix)
                .ok_or_else(|| CrabError::Configuration {
                    key: "data_import_revision_invalid".into(),
                    origin: "Git tree entry escaped the selected directory".into(),
                })?,
            None => path.as_str(),
        };
        let relative_file_path = Path::new(relative_file);
        ensure_safe_relative(relative_file_path)?;
        let mode = parse_git_file_mode(fields[0], &path)?;
        let object = format!("{commit}:{path}");
        let destination = target.join(relative_file_path);
        materialize_git_blob_object(root, &object, &path, mode, &destination)?;
    }
    Ok(())
}

fn parse_git_file_mode(mode: &[u8], relative: &str) -> Result<u32> {
    let mode = std::str::from_utf8(mode).map_err(|_| CrabError::Configuration {
        key: "data_import_revision_invalid".into(),
        origin: "Git tree entry has an invalid mode".into(),
    })?;
    let mode = u32::from_str_radix(mode, 8).map_err(|_| CrabError::Configuration {
        key: "data_import_revision_invalid".into(),
        origin: "Git tree entry has an invalid mode".into(),
    })?;
    if !matches!(mode, 0o100644 | 0o100755) {
        return Err(CrabError::Configuration {
            key: "data_import_revision_entry_unsupported".into(),
            origin: relative.to_owned(),
        });
    }
    Ok(mode)
}

fn materialize_git_blob_object(
    root: &Path,
    object: &str,
    origin: &str,
    mode: u32,
    target: &Path,
) -> Result<([u8; 32], u64)> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["cat-file", "blob"])
        .arg(object)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CrabError::Io)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Configuration {
            key: "data_import_revision_failed".into(),
            origin: origin.to_owned(),
        })?;
    let mut file = match File::options().write(true).create_new(true).open(target) {
        Ok(file) => file,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CrabError::Io(error));
        }
    };
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = match stdout.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = remove_existing_path(target);
                return Err(CrabError::Io(error));
            }
        };
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            let _ = remove_existing_path(target);
            CrabError::Io(error)
        })?;
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    file.sync_all().map_err(CrabError::Io)?;
    let status = child.wait().map_err(CrabError::Io)?;
    if !status.success() {
        let _ = remove_existing_path(target);
        return Err(CrabError::Configuration {
            key: "data_import_revision_failed".into(),
            origin: origin.to_owned(),
        });
    }
    set_executable_mode(target, mode)?;
    Ok((*hasher.finalize().as_bytes(), size))
}

fn set_executable_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).map_err(CrabError::Io)?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 });
        fs::set_permissions(path, permissions).map_err(CrabError::Io)?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "data_git_command_failed".into(),
            origin: args.first().copied().unwrap_or("git").to_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| CrabError::Configuration {
        key: "data_git_command_invalid".into(),
        origin: args.first().copied().unwrap_or("git").to_owned(),
    })
}

fn git_output_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "data_git_command_failed".into(),
            origin: args.first().copied().unwrap_or("git").to_owned(),
        });
    }
    Ok(output.stdout)
}

fn split_tree_record(record: &[u8]) -> Result<(&[u8], &[u8])> {
    let separator = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| CrabError::Configuration {
            key: "data_git_tree_invalid".into(),
            origin: "Git tree entry is malformed".into(),
        })?;
    Ok((&record[..separator], &record[separator + 1..]))
}

fn split_ascii_fields(value: &[u8]) -> Vec<&[u8]> {
    value
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect()
}

fn normalize_relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_path(path: &Path) -> Result<([u8; 32], u64)> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    if metadata.is_file() {
        let mut file = File::open(path).map_err(CrabError::Io)?;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(CrabError::Io)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size = size.saturating_add(read as u64);
        }
        return Ok((*hasher.finalize().as_bytes(), size));
    }
    if metadata.is_dir() {
        let tree = crab_workflow::hasher::hash_directory(path, false)?;
        return Ok((
            tree.hash,
            tree.manifest.iter().map(|entry| entry.size).sum(),
        ));
    }
    Err(CrabError::Configuration {
        key: "data_source_type_unsupported".into(),
        origin: path.display().to_string(),
    })
}

fn materialize_from_path(source: &Path, target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_ok() {
        return Err(CrabError::Configuration {
            key: "data_target_exists".into(),
            origin: target.display().to_string(),
        });
    }
    if fs::symlink_metadata(source)
        .map_err(CrabError::Io)?
        .is_dir()
        && source
            .canonicalize()
            .ok()
            .zip(
                target
                    .parent()
                    .and_then(|parent| parent.canonicalize().ok()),
            )
            .is_some_and(|(source, target_parent)| target_parent.starts_with(source))
    {
        return Err(CrabError::Configuration {
            key: "data_target_inside_source".into(),
            origin: target.display().to_string(),
        });
    }
    snapshot_payload(source, target).map_err(|error| CrabError::Configuration {
        key: "data_materialize_failed".into(),
        origin: error.to_string(),
    })?;
    Ok(())
}

fn verify_materialized(target: &Path, expected_hash: [u8; 32], expected_size: u64) -> Result<()> {
    let (actual_hash, actual_size) = hash_path(target)?;
    if actual_hash != expected_hash || actual_size != expected_size {
        return Err(CrabError::Configuration {
            key: "data_source_changed_during_import".into(),
            origin: target.display().to_string(),
        });
    }
    Ok(())
}

fn canonicalize_source(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CrabError::Configuration {
                key: "data_import_source_missing".into(),
                origin: path.display().to_string(),
            }
        } else {
            CrabError::Io(error)
        }
    })
}

fn reject_existing_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(_) => Err(CrabError::Configuration {
            key: "data_target_exists".into(),
            origin: target.display().to_string(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CrabError::Io(error)),
    }
}

fn install_new_payload(source: &Path, target: &Path) -> Result<()> {
    reject_existing_target(target)?;
    let parent = target.parent().ok_or_else(|| CrabError::Configuration {
        key: "data_target_invalid".into(),
        origin: target.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    fs::rename(source, target).map_err(CrabError::Io)
}

struct TemporaryPathCleanup {
    path: Option<PathBuf>,
}

impl TemporaryPathCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryPathCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = remove_existing_path(&path);
        }
    }
}

struct TargetSwap {
    target: PathBuf,
    backup: Option<PathBuf>,
    committed: bool,
}

impl TargetSwap {
    fn apply(temporary: &Path, target: &Path) -> Result<Self> {
        if fs::symlink_metadata(temporary).is_err() {
            return Err(CrabError::Configuration {
                key: "data_update_temporary_missing".into(),
                origin: temporary.display().to_string(),
            });
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        let backup = target.with_file_name(format!(
            ".{}.backup-{}-{}",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("data"),
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let had_target = match fs::symlink_metadata(target) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(CrabError::Io(error)),
        };
        if had_target {
            fs::rename(target, &backup).map_err(CrabError::Io)?;
        }
        if let Err(error) = fs::rename(temporary, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(CrabError::Io(error));
        }
        Ok(Self {
            target: target.to_owned(),
            backup: had_target.then_some(backup),
            committed: false,
        })
    }

    fn finish(mut self) {
        self.committed = true;
        if let Some(backup) = self.backup.take()
            && let Err(error) = remove_existing_path(&backup)
        {
            // The new source is already installed. Keep the committed target
            // and defer backup cleanup instead of reporting a false failure.
            tracing::warn!(path = %backup.display(), error = %error, "data backup cleanup deferred");
        }
    }
}

impl Drop for TargetSwap {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = remove_existing_path(&self.target);
        if let Some(backup) = self.backup.take() {
            let _ = fs::rename(backup, &self.target);
        }
    }
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CrabError::Io(error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(CrabError::Io)
    } else {
        fs::remove_file(path).map_err(CrabError::Io)
    }
}

fn remove_temporary(path: &Path) {
    let _ = remove_existing_path(path);
}

fn stream_to_hash(
    mut response: reqwest::blocking::Response,
    target: &mut File,
) -> Result<([u8; 32], u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut size = 0_u64;
    loop {
        let read = response.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        target.write_all(&buffer[..read]).map_err(CrabError::Io)?;
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((*hasher.finalize().as_bytes(), size))
}

fn safe_target(root: &Path, target: &Path) -> Result<PathBuf> {
    ensure_safe_relative(target)?;
    let full = root.join(target);
    let mut current = root.to_path_buf();
    for component in target.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(CrabError::Configuration {
                key: "data_path_symlink".into(),
                origin: target.display().to_string(),
            });
        }
    }
    if !full.starts_with(root) {
        return Err(CrabError::Configuration {
            key: "data_path_unsafe".into(),
            origin: target.display().to_string(),
        });
    }
    Ok(full)
}

fn ensure_safe_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path == Path::new(".")
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CrabError::Configuration {
            key: "data_path_unsafe".into(),
            origin: path.display().to_string(),
        });
    }
    Ok(())
}

fn temporary_target(target: &Path) -> PathBuf {
    target.with_file_name(format!(
        ".{}.tmp-{}-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("data"),
        std::process::id(),
        uuid::Uuid::now_v7()
    ))
}

fn redact_url(url: &url::Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    let mut pairs = Vec::new();
    for (key, value) in url.query_pairs() {
        let lower = key.to_ascii_lowercase();
        if [
            "token",
            "secret",
            "password",
            "key",
            "signature",
            "credential",
            "access",
            "auth",
        ]
        .iter()
        .any(|s| lower.contains(s))
        {
            continue;
        }
        pairs.push((key.into_owned(), value.into_owned()));
    }
    let query = pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    redacted.set_query((!query.is_empty()).then_some(query.as_str()));
    redacted.set_fragment(None);
    redacted.to_string()
}

fn url_has_embedded_secret(url: &url::Url) -> bool {
    !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query_pairs().any(|(key, _)| {
            let key = key.to_ascii_lowercase();
            [
                "token",
                "secret",
                "password",
                "key",
                "signature",
                "credential",
                "access",
                "auth",
            ]
            .iter()
            .any(|marker| key.contains(marker))
        })
}

fn hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from(*bytes).to_hex().to_string()
}

fn emit_import(mode: OutputMode, descriptor: SourceDescriptor, changed: bool, dry_run: bool) {
    emit(
        mode,
        DataPayload {
            operation: "import",
            entries: ImportPayload {
                descriptor,
                changed,
                dry_run,
            },
        },
        |payload| println!("imported {}", payload.entries.descriptor.target),
    );
}

fn emit<T, F>(mode: OutputMode, payload: T, text: F)
where
    T: Serialize,
    F: FnOnce(&T),
{
    match mode {
        OutputMode::Text => text(&payload),
        OutputMode::Json => emit_json(DATA_SCHEMA, "1.0", payload),
        OutputMode::Jsonl => {
            let mut stream = JsonlStream::new(DATA_EVENT_SCHEMA, "1.0", std::io::stdout());
            stream.emit_result(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::ObjectStoreExt;
    use tempfile::TempDir;

    #[test]
    fn object_store_import_streams_and_verifies_payload() {
        let store = object_store::memory::InMemory::new();
        crate::cmd::lfs::block_on_runtime(async {
            store
                .put(
                    &ObjectPath::from("data/sample.bin"),
                    Bytes::from_static(b"object-store-data").into(),
                )
                .await
                .map_err(CrabError::Storage)
        })
        .unwrap();
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("sample.bin");
        let (_, size, validator) = fetch_object_store_object(
            Box::new(store),
            ObjectPath::from("data/sample.bin"),
            destination.clone(),
        )
        .unwrap();
        assert_eq!(size, b"object-store-data".len() as u64);
        assert!(validator.is_some());
        assert_eq!(fs::read(destination).unwrap(), b"object-store-data");
    }

    #[test]
    fn sqlite_import_writes_verified_jsonl_and_descriptor() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("source.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "create table samples (id integer, label text, blob blob);
                 insert into samples values (2, 'two', x'0102');
                 insert into samples values (1, 'one', null);",
            )
            .unwrap();
        drop(connection);

        run_import_db(
            temp.path(),
            &DataImportDbArgs {
                connector: "sqlite".to_owned(),
                database,
                query: "select id, label, blob from samples order by id".to_owned(),
                output: PathBuf::from("data/samples.jsonl"),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("data/samples.jsonl")).unwrap(),
            "{\"blob\":null,\"id\":1,\"label\":\"one\"}\n\
             {\"blob\":{\"$binary_base64\":\"AQI=\"},\"id\":2,\"label\":\"two\"}\n"
        );
        let descriptors = load_descriptors(temp.path()).unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].kind, "sqlite");
        assert_eq!(
            descriptors[0].metadata.get("query").map(String::as_str),
            Some("select id, label, blob from samples order by id")
        );
    }

    #[test]
    fn sqlite_update_refreshes_snapshot_without_changing_query_identity() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("source.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "create table samples (id integer, label text);
                 insert into samples values (1, 'one');",
            )
            .unwrap();
        drop(connection);

        let args = DataImportDbArgs {
            connector: "sqlite".to_owned(),
            database: database.clone(),
            query: "select id, label from samples order by id".to_owned(),
            output: PathBuf::from("data/samples.jsonl"),
            json: false,
            jsonl: false,
        };
        run_import_db(temp.path(), &args).unwrap();
        let before = load_descriptors(temp.path()).unwrap().pop().unwrap();

        let connection = Connection::open(&database).unwrap();
        connection
            .execute("insert into samples values (2, 'two')", [])
            .unwrap();
        drop(connection);

        run_update(
            temp.path(),
            &DataUpdateArgs {
                target: "data/samples.jsonl".to_owned(),
                dry_run: false,
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        let after = load_descriptors(temp.path()).unwrap().pop().unwrap();
        assert_eq!(after.revision, before.revision);
        assert_ne!(after.content_hash, before.content_hash);
        assert_eq!(
            fs::read_to_string(temp.path().join("data/samples.jsonl")).unwrap(),
            "{\"id\":1,\"label\":\"one\"}\n{\"id\":2,\"label\":\"two\"}\n"
        );
    }

    #[test]
    fn redact_url_drops_secret_query_fields() {
        let url = url::Url::parse("https://user:pass@example.test/a?token=secret&rev=1").unwrap();
        let redacted = redact_url(&url);
        assert_eq!(redacted, "https://example.test/a?rev=1");
    }

    #[test]
    fn import_url_rejects_embedded_credentials_before_network_or_write() {
        let url = url::Url::parse("https://example.test/model?token=secret").unwrap();
        assert!(url_has_embedded_secret(&url));
        let temp = TempDir::new().unwrap();
        let args = DataImportUrlArgs {
            url: url.to_string(),
            output: PathBuf::from("model.bin"),
            json: false,
            jsonl: false,
        };
        let error = run_import_url(temp.path(), &args).unwrap_err();
        assert!(matches!(
            error,
            CrabError::Configuration { key, .. }
                if key == "data_import_url_credentials_unsupported"
        ));
        assert!(!temp.path().join("model.bin").exists());
    }

    #[test]
    fn import_file_url_reports_missing_source_before_replacement() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("missing.bin");
        let args = DataImportUrlArgs {
            url: url::Url::from_file_path(&source).unwrap().to_string(),
            output: PathBuf::from("model.bin"),
            json: false,
            jsonl: false,
        };
        let error = run_import_url(temp.path(), &args).unwrap_err();
        assert!(matches!(
            error,
            CrabError::Configuration { key, .. }
                if key == "data_import_source_missing"
        ));
        assert!(!temp.path().join("model.bin").exists());
    }

    #[test]
    fn source_import_rejects_traversal_before_writing() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"payload").unwrap();
        let args = DataImportArgs {
            source,
            path: None,
            output: PathBuf::from("../escape"),
            rev: None,
            json: false,
            jsonl: false,
        };
        assert!(run_import(temp.path(), &args).is_err());
        assert!(!temp.path().join("../escape").exists());
    }

    #[test]
    fn list_defaults_to_the_worktree_root() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("data.txt"), b"payload").unwrap();
        run_list(
            temp.path(),
            &DataListArgs {
                path: None,
                rev: None,
                recursive: true,
                json: true,
                jsonl: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn list_root_returns_children_without_recursive_directory_placeholder() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("data.txt"), b"payload").unwrap();
        fs::create_dir_all(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/value.txt"), b"value").unwrap();

        let source_ids = BTreeMap::new();
        let mut entries = Vec::new();
        collect_entries(
            temp.path(),
            Path::new("."),
            false,
            &source_ids,
            &mut entries,
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.kind.as_str(), entry.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("file", "data.txt"), ("directory", "nested")]
        );
    }

    #[test]
    fn local_import_update_and_status_preserve_descriptor_identity() {
        let root = TempDir::new().unwrap();
        let source_root = TempDir::new().unwrap();
        let source = source_root.path().join("dataset.txt");
        fs::write(&source, b"v1").unwrap();
        run_import(
            root.path(),
            &DataImportArgs {
                source: source_root.path().to_owned(),
                path: Some(PathBuf::from("dataset.txt")),
                output: PathBuf::from("data/dataset.txt"),
                rev: None,
                json: false,
                jsonl: false,
            },
        )
        .unwrap();
        let descriptor = find_descriptor(root.path(), "data/dataset.txt").unwrap();
        assert_eq!(
            fs::read(root.path().join("data/dataset.txt")).unwrap(),
            b"v1"
        );
        fs::write(&source, b"v2").unwrap();
        run_update(
            root.path(),
            &DataUpdateArgs {
                target: descriptor.id.clone(),
                dry_run: false,
                json: false,
                jsonl: false,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(root.path().join("data/dataset.txt")).unwrap(),
            b"v2"
        );
        let updated = find_descriptor(root.path(), &descriptor.id).unwrap();
        assert_eq!(updated.id, descriptor.id);
        run_status(
            root.path(),
            &DataStatusArgs {
                target: Some(descriptor.id),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        let status = status_entry(root.path(), updated);
        assert_eq!(status.kind, "source:up-to-date");
        assert_eq!(status.dimensions.get("workspace"), Some(&"up-to-date"));
        assert_eq!(status.dimensions.get("descriptor"), Some(&"present"));
        assert_eq!(status.dimensions.get("lock"), Some(&"unlocked"));
        assert_eq!(status.dimensions.get("source"), Some(&"not-checked"));
        assert_eq!(status.dimensions.get("remote"), Some(&"not-checked"));
        assert_eq!(status.dimensions.get("cache"), Some(&"not-managed"));
    }

    fn git_fixture() -> TempDir {
        let repository = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(repository.path())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "Crab Test"]);
        repository
    }

    #[test]
    fn data_commands_resolve_the_worktree_root_from_nested_directories() {
        let repository = git_fixture();
        let nested = repository.path().join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            resolve_data_root(&nested).unwrap(),
            repository.path().canonicalize().unwrap()
        );
    }

    fn commit_fixture(repository: &TempDir) -> String {
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(repository.path())
            .args(["commit", "--quiet", "-m", "fixture"])
            .status()
            .unwrap();
        assert!(status.success());
        resolve_git_revision(repository.path(), "HEAD").unwrap()
    }

    #[test]
    fn list_git_revision_resolves_tree_and_file_paths() {
        let repository = git_fixture();
        fs::write(repository.path().join("root.txt"), b"root").unwrap();
        fs::create_dir_all(repository.path().join("nested")).unwrap();
        fs::write(repository.path().join("nested/value.txt"), b"value").unwrap();
        let commit = commit_fixture(&repository);

        let root_entries = collect_git_entries(
            repository.path(),
            &commit,
            Path::new("."),
            false,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            root_entries
                .iter()
                .map(|entry| (entry.kind.as_str(), entry.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("directory", "nested"), ("file", "root.txt")]
        );

        let nested_entries = collect_git_entries(
            repository.path(),
            &commit,
            Path::new("nested"),
            false,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(nested_entries[0].path, "nested/value.txt");

        let file_entries = collect_git_entries(
            repository.path(),
            &commit,
            Path::new("root.txt"),
            false,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(file_entries[0].path, "root.txt");
    }

    #[test]
    fn import_git_revision_locks_commit_and_preserves_mode() {
        let root = TempDir::new().unwrap();
        let repository = git_fixture();
        let source = repository.path().join("bin/run.sh");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"#!/bin/sh\necho ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let commit = commit_fixture(&repository);
        run_import(
            root.path(),
            &DataImportArgs {
                source: repository.path().to_owned(),
                path: Some(PathBuf::from("bin/run.sh")),
                output: PathBuf::from("data/run.sh"),
                rev: Some("HEAD".to_owned()),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();
        let descriptor = find_descriptor(root.path(), "data/run.sh").unwrap();
        assert_eq!(descriptor.revision.as_deref(), Some(commit.as_str()));
        assert_eq!(
            descriptor.metadata.get("source_path").map(String::as_str),
            Some("bin/run.sh")
        );
        assert_eq!(
            descriptor
                .metadata
                .get("requested_revision")
                .map(String::as_str),
            Some("HEAD")
        );
        assert_eq!(
            fs::read(root.path().join("data/run.sh")).unwrap(),
            b"#!/bin/sh\necho ok\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("data/run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }
    }

    #[test]
    fn import_git_revision_directory_materializes_tree_and_modes() {
        let root = TempDir::new().unwrap();
        let repository = git_fixture();
        let dataset = repository.path().join("dataset");
        let executable = dataset.join("bin/run.sh");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(dataset.join("README.md"), b"dataset\n").unwrap();
        fs::write(&executable, b"#!/bin/sh\necho ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let commit = commit_fixture(&repository);

        run_import(
            root.path(),
            &DataImportArgs {
                source: repository.path().to_owned(),
                path: Some(PathBuf::from("dataset")),
                output: PathBuf::from("data/dataset"),
                rev: Some("HEAD".to_owned()),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        let descriptor = find_descriptor(root.path(), "data/dataset").unwrap();
        assert_eq!(descriptor.revision.as_deref(), Some(commit.as_str()));
        assert_eq!(
            descriptor.metadata.get("source_path").map(String::as_str),
            Some("dataset")
        );
        assert_eq!(
            fs::read(root.path().join("data/dataset/README.md")).unwrap(),
            b"dataset\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("data/dataset/bin/run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }

        fs::write(dataset.join("new.txt"), b"new\n").unwrap();
        commit_fixture(&repository);
        run_update(
            root.path(),
            &DataUpdateArgs {
                target: "data/dataset".to_owned(),
                dry_run: false,
                json: false,
                jsonl: false,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(root.path().join("data/dataset/new.txt")).unwrap(),
            b"new\n"
        );
    }

    #[test]
    fn import_git_revision_defaults_to_repository_tree() {
        let root = TempDir::new().unwrap();
        let repository = git_fixture();
        fs::create_dir_all(repository.path().join("nested")).unwrap();
        fs::write(repository.path().join("nested/value.txt"), b"value").unwrap();
        commit_fixture(&repository);

        run_import(
            root.path(),
            &DataImportArgs {
                source: repository.path().to_owned(),
                path: None,
                output: PathBuf::from("data/repository"),
                rev: Some("HEAD".to_owned()),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read(root.path().join("data/repository/nested/value.txt")).unwrap(),
            b"value"
        );
    }

    #[test]
    fn update_git_revision_follows_requested_ref_and_relocks_commit() {
        let root = TempDir::new().unwrap();
        let repository = git_fixture();
        let source = repository.path().join("data.txt");
        fs::write(&source, b"v1").unwrap();
        let first_commit = commit_fixture(&repository);
        run_import(
            root.path(),
            &DataImportArgs {
                source: repository.path().to_owned(),
                path: Some(PathBuf::from("data.txt")),
                output: PathBuf::from("data/data.txt"),
                rev: Some("HEAD".to_owned()),
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        fs::write(&source, b"v2").unwrap();
        let second_commit = commit_fixture(&repository);
        run_update(
            root.path(),
            &DataUpdateArgs {
                target: "data/data.txt".to_owned(),
                dry_run: false,
                json: false,
                jsonl: false,
            },
        )
        .unwrap();

        assert_eq!(fs::read(root.path().join("data/data.txt")).unwrap(), b"v2");
        let descriptor = find_descriptor(root.path(), "data/data.txt").unwrap();
        assert_eq!(descriptor.revision.as_deref(), Some(second_commit.as_str()));
        assert_ne!(descriptor.revision.as_deref(), Some(first_commit.as_str()));
        assert_eq!(
            descriptor
                .metadata
                .get("requested_revision")
                .map(String::as_str),
            Some("HEAD")
        );
    }
}
