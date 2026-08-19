//! CLI surface for `crab metadb` — operator tooling for the two
//! SlateDB metadata databases.
//!
//! Subcommands:
//!
//! - `diagnose` — read-only health snapshot of the system keys
//!   (`sys:format_version`, `sys:epoch`, `sys:created_at`,
//!   `sys:gc_generation`). Optional `--db` filter narrows to a single
//!   instance. Deeper integrity checks (WAL replay, bloom validity)
//!   would live here too, but the public `slatedb` crate does not
//!   expose those surfaces yet; the diagnose output records the gap
//!   rather than claiming a check ran.
//! - `rebuild` — disaster-recovery reconstruction of one or both
//!   databases from the durable shards under `.crab/shards/`. The
//!   MVP implementation is append-only: every entry is
//!   content-addressed so re-writes are no-ops, which makes the
//!   command safely retriable.
//! - `compact` — request immediate SlateDB compaction. SlateDB drives
//!   compaction in the background and the current public crate does
//!   not expose an imperative trigger, so this subcommand logs a
//!   warning and exits successfully.
//! - `cache {stats | clear}` — inspect or wipe the local
//!   `PersistentChunkIndex` SQLite cache.
//!
//! The `--metadb` branch of `crab doctor` lives here too
//! ([`run_doctor_metadb_in`]) so every metadb-facing report shares
//! one helper set.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use clap::Subcommand;
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tracing::{info, warn};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crate::metadata::{MetaDb, MetaDbGuard, XorbRef};
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::parser::XorbParser;

/// Which databases the subcommand operates on.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DbSelector {
    FileIndex,
    ChunkIndex,
    Both,
}

impl DbSelector {
    fn includes_file_index(self) -> bool {
        matches!(self, Self::FileIndex | Self::Both)
    }

    fn includes_chunk_index(self) -> bool {
        matches!(self, Self::ChunkIndex | Self::Both)
    }
}

/// `crab metadb cache {stats | clear}` subsubcommands.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Report on-disk size, entry count, and installed shard count
    /// for the local chunk-index SQLite cache.
    Stats,
    /// Remove the local chunk-index SQLite file on disk. The next
    /// crab operation will re-open it cold.
    Clear,
}

/// Top-level subcommands for `crab metadb`.
#[derive(Debug, Subcommand)]
pub enum MetadbCommand {
    /// Print `sys:*` key snapshots for one or both databases.
    Diagnose {
        /// Target database (defaults to both).
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
        /// Run deep integrity checks: full key/value scan and
        /// object-store enumeration. Slower but catches corruption
        /// that sys-key reads alone cannot detect.
        #[arg(long)]
        deep: bool,
    },
    /// Rebuild one or both databases from the durable shards under
    /// `.crab/shards/` (disaster recovery).
    Rebuild {
        /// Target database.
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Request immediate compaction of one or both databases.
    Compact {
        /// Target database.
        #[arg(long, value_enum, default_value_t = DbSelector::Both)]
        db: DbSelector,
    },
    /// Inspect or wipe the local chunk-index cache.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// Structured payload for `crab metadb diagnose --json`.
#[derive(Debug, Serialize)]
pub struct DiagnosePayload {
    pub file_index: Option<DbDiagnosis>,
    pub chunk_index: Option<DbDiagnosis>,
}

/// Per-database system-key summary.
#[derive(Debug, Serialize)]
pub struct DbDiagnosis {
    pub label: &'static str,
    pub path: String,
    /// Whether the `Db::open` call succeeded. `false` entries still
    /// carry an `error` string describing the failure.
    pub opened: bool,
    pub error: Option<String>,
    pub format_version: Option<u32>,
    pub epoch: Option<u64>,
    pub created_at: Option<String>,
    pub gc_generation: Option<u64>,
    /// Deep integrity check results. `None` when `--deep` was not
    /// requested or the database failed to open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_integrity: Option<DeepIntegrityResult>,
}

/// Results of a full-scan deep integrity check over one database.
#[derive(Debug, Serialize)]
pub struct DeepIntegrityResult {
    /// Total number of keys scanned (content + system + unknown).
    pub total_keys: u64,
    /// Number of well-formed content keys (prefix 0x01, 33 bytes).
    pub content_keys: u64,
    /// Number of system keys (prefix 0xFF).
    pub system_keys: u64,
    /// Keys that don't match either known prefix.
    pub unknown_keys: u64,
    /// Content keys whose value has an unexpected length.
    pub corrupt_values: u64,
    /// First few corruption details (capped to avoid flooding output).
    pub corruption_samples: Vec<String>,
    /// Number of object-store files under the database path.
    pub object_store_files: u64,
    /// Total bytes across all object-store files.
    pub object_store_bytes: u64,
    /// Whether the scan completed without iterator invalidation.
    pub scan_completed: bool,
    /// Human-readable verdict.
    pub verdict: String,
}

/// Structured payload for `crab doctor --metadb --json`.
#[derive(Debug, Serialize)]
pub struct DoctorMetadbPayload {
    pub repo_prefix: String,
    pub file_index: DbDiagnosis,
    pub chunk_index: DbDiagnosis,
    pub shards_prefix: String,
    pub shard_count: Option<u64>,
    pub shard_enumeration_error: Option<String>,
    pub cache: CacheStatsPayload,
    pub acceleration: AccelerationHealth,
}

#[derive(Debug, Serialize)]
pub struct AccelerationHealth {
    pub manifest_generation: Option<u64>,
    pub generation_receipt_valid: bool,
    pub ref_registry_repo_complete: bool,
    pub ref_registry_bucket_complete: bool,
    pub git_locator_index_available: bool,
    pub git_locator_covered_generation: Option<u64>,
    pub git_locator_covered_pack_index_hash: Option<String>,
    pub git_visibility_index_available: bool,
    pub git_visibility_covered_generation: Option<u64>,
    pub git_visibility_covered_pack_index_hash: Option<String>,
    pub git_visibility_coverage_current: bool,
    pub git_locator_writer_lease_active: bool,
    pub repair_required: bool,
    pub notes: Vec<String>,
}

impl AccelerationHealth {
    fn unavailable(note: impl Into<String>) -> Self {
        Self {
            manifest_generation: None,
            generation_receipt_valid: false,
            ref_registry_repo_complete: false,
            ref_registry_bucket_complete: false,
            git_locator_index_available: false,
            git_locator_covered_generation: None,
            git_locator_covered_pack_index_hash: None,
            git_visibility_index_available: false,
            git_visibility_covered_generation: None,
            git_visibility_covered_pack_index_hash: None,
            git_visibility_coverage_current: false,
            git_locator_writer_lease_active: false,
            repair_required: true,
            notes: vec![note.into()],
        }
    }
}

/// Structured payload for `crab metadb cache stats`.
#[derive(Debug, Serialize)]
pub struct CacheStatsPayload {
    pub cache_path: String,
    pub exists: bool,
    pub file_size_bytes: u64,
    pub entry_count: u64,
    pub installed_shard_count: u64,
    pub cache_gc_generation: u64,
}

/// Dispatch for `crab metadb <sub>`.
pub async fn run_metadb(cmd: MetadbCommand, mode: OutputMode) -> Result<()> {
    match cmd {
        MetadbCommand::Diagnose { db, json, deep } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_diagnose(db, mode, deep).await
        }
        MetadbCommand::Rebuild { db, json } => {
            let mode = if json { OutputMode::Json } else { mode };
            run_rebuild(db, mode).await
        }
        MetadbCommand::Compact { db } => run_compact(db).await,
        MetadbCommand::Cache(sub) => match sub {
            CacheCommand::Stats => run_cache_stats(mode),
            CacheCommand::Clear => run_cache_clear(),
        },
    }
}

/// Resolve the `(store, repo_prefix, bucket_identity, config)` tuple for the current
/// working directory. Returns a user-facing error when no remote is
/// configured — every metadb subcommand needs the bucket.
async fn resolve_repo_store() -> Result<(
    Arc<dyn ObjectStore>,
    String,
    crate::storage::store::BucketIdentity,
    Config,
)> {
    let cwd = std::env::current_dir()?;
    resolve_repo_store_in(&cwd).await
}

async fn resolve_repo_store_in(
    root: &Path,
) -> Result<(
    Arc<dyn ObjectStore>,
    String,
    crate::storage::store::BucketIdentity,
    Config,
)> {
    let remote_path = root.join(".crab/remote");
    let url = std::fs::read_to_string(&remote_path)
        .map_err(|e| CrabError::Internal(format!("could not read {}: {e}", remote_path.display())))?
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err(CrabError::Internal(format!(
            "{} is empty; run `crab init <url>`",
            remote_path.display()
        )));
    }
    let parsed = CrabUrl::parse(&url)?;
    let config = Config::resolve_local().unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let store = crate::auth::build_store(&config, &parsed, "metadb", &cancel).await?;
    // `build_store` hands back a `ProbingStoreHandle` which wraps an
    // `Arc<dyn ObjectStore>` via `inner()`. Clone the inner handle out
    // so the metadb layer holds a plain `Arc<dyn ObjectStore>`.
    let bucket_identity = crate::git::url::ObjectUrl::parse(url.trim())?.bucket_identity();
    Ok((
        Arc::clone(store.inner()),
        parsed.repo_path,
        bucket_identity,
        config,
    ))
}

/// Open a metadb session anchored at `repo_prefix`.
///
/// `read_only` selects the SlateDB open mode. Subcommands that only
/// read system keys (`diagnose`, `doctor`) pass `true` so they can run
/// alongside a concurrent `crab push` without fencing it. `rebuild`
/// passes `false` because it emits batched writes against both
/// databases.
///
/// The per-DB tunables come from `config.metadb`; the local chunk cache
/// is bucket-global unless the operator set `metadb.chunk_index.local_path`.
fn build_metadb(
    store: Arc<dyn ObjectStore>,
    repo_prefix: String,
    bucket_identity: &crate::storage::store::BucketIdentity,
    read_only: bool,
    config: &Config,
) -> MetaDb {
    let mut metadb_config = config.build_metadb_config(&repo_prefix);
    metadb_config.read_only = read_only;

    if config.metadb.chunk_index.local_path.is_none() {
        metadb_config.local_chunk_index_path = crate::cache::chunk_index_cache_path(
            &crate::cache::default_cache_root(),
            bucket_identity,
        );
    }

    MetaDb::new(store, repo_prefix, metadb_config)
}

// --- diagnose -------------------------------------------------------

async fn run_diagnose(db: DbSelector, mode: OutputMode, deep: bool) -> Result<()> {
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store().await?;
    let metadb_config = config.build_metadb_config(&repo_prefix);
    // Diagnose only reads sys:* keys — open read-only so a
    // concurrent push is not fenced.
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix,
        &bucket_identity,
        true,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let file_index = if db.includes_file_index() {
        Some(diagnose_file_index(&guard, deep, &store, &metadb_config.file_index_path).await)
    } else {
        None
    };
    let chunk_index = if db.includes_chunk_index() {
        Some(diagnose_chunk_index(&guard, deep, &store, &metadb_config.chunk_index_path).await)
    } else {
        None
    };

    let payload = DiagnosePayload {
        file_index,
        chunk_index,
    };

    render_diagnose(&payload, mode);
    guard.close().await?;
    Ok(())
}

async fn diagnose_file_index(
    guard: &MetaDbGuard,
    deep: bool,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
) -> DbDiagnosis {
    match guard.file_index_system_keys().await {
        Ok(snap) => {
            let deep_integrity = if deep {
                Some(
                    run_deep_integrity(
                        guard.file_index_db_handle().await.ok(),
                        store,
                        db_path,
                        DbKind::FileIndex,
                    )
                    .await,
                )
            } else {
                None
            };
            DbDiagnosis {
                label: "file_index_db",
                path: String::from("file_index_db/"),
                opened: true,
                error: None,
                format_version: snap.format_version,
                epoch: snap.epoch,
                created_at: snap.created_at_unix_ms.map(unix_ms_to_iso8601),
                gc_generation: snap.gc_generation,
                deep_integrity,
            }
        }
        Err(CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnlyUninitialized {
            ..
        })) => DbDiagnosis {
            label: "file_index_db",
            path: String::from("file_index_db/"),
            opened: false,
            error: Some(String::from(
                "database not initialized (no manifest on object storage yet)",
            )),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
        Err(e) => DbDiagnosis {
            label: "file_index_db",
            path: String::from("file_index_db/"),
            opened: false,
            error: Some(e.to_string()),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
    }
}

async fn diagnose_chunk_index(
    guard: &MetaDbGuard,
    deep: bool,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
) -> DbDiagnosis {
    match guard.chunk_index_system_keys().await {
        Ok(snap) => {
            let deep_integrity = if deep {
                Some(
                    run_deep_integrity(
                        guard.chunk_index_db_handle().await.ok(),
                        store,
                        db_path,
                        DbKind::ChunkIndex,
                    )
                    .await,
                )
            } else {
                None
            };
            DbDiagnosis {
                label: "chunk_index_db",
                path: String::from(".crab/chunk_index_db/"),
                opened: true,
                error: None,
                format_version: snap.format_version,
                epoch: snap.epoch,
                created_at: snap.created_at_unix_ms.map(unix_ms_to_iso8601),
                gc_generation: snap.gc_generation,
                deep_integrity,
            }
        }
        Err(CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnlyUninitialized {
            ..
        })) => DbDiagnosis {
            label: "chunk_index_db",
            path: String::from(".crab/chunk_index_db/"),
            opened: false,
            error: Some(String::from(
                "database not initialized (no manifest on object storage yet)",
            )),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
        Err(e) => DbDiagnosis {
            label: "chunk_index_db",
            path: String::from(".crab/chunk_index_db/"),
            opened: false,
            error: Some(e.to_string()),
            format_version: None,
            epoch: None,
            created_at: None,
            gc_generation: None,
            deep_integrity: None,
        },
    }
}

// --- deep integrity -------------------------------------------------

use crate::metadata::metadb::stores;
use crab_metadata::key_codec::{CONTENT_KEY_LEN, PREFIX_CONTENT, PREFIX_SYSTEM};
use crab_metadata::value_codec::{CHUNK_INDEX_VALUE_LEN, FILE_INDEX_VALUE_LEN};

/// Which logical database we're checking — determines expected value
/// sizes.
#[derive(Debug, Clone, Copy)]
enum DbKind {
    /// file_index_db: values are 32-byte shard hashes.
    FileIndex,
    /// chunk_index_db: values are 40-byte XorbRef encodings.
    ChunkIndex,
}

impl DbKind {
    fn expected_value_len(self) -> usize {
        match self {
            Self::FileIndex => FILE_INDEX_VALUE_LEN,
            Self::ChunkIndex => CHUNK_INDEX_VALUE_LEN,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::FileIndex => stores::file_index::DB_LABEL,
            Self::ChunkIndex => stores::chunk_index::DB_LABEL,
        }
    }
}

/// Maximum number of corruption samples to collect before stopping
/// detailed recording. Keeps output bounded for badly damaged DBs.
const MAX_CORRUPTION_SAMPLES: usize = 20;

/// Lowercase hex encoding for short byte slices in diagnostic output.
fn short_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Run a full-scan deep integrity check over one database.
///
/// Two independent checks run in parallel:
/// 1. **Key/value scan** — iterates every entry via `Db::scan`,
///    classifies keys by prefix, and validates value lengths.
/// 2. **Object-store enumeration** — lists all files under the
///    database path to report storage-level health (file count,
///    total bytes).
async fn run_deep_integrity(
    db_handle: Option<std::sync::Arc<crate::metadata::metadb::Db>>,
    store: &Arc<dyn ObjectStore>,
    db_path: &str,
    kind: DbKind,
) -> DeepIntegrityResult {
    let (scan_result, storage_result) = tokio::join!(
        scan_all_keys(db_handle.as_ref(), kind),
        enumerate_storage(store, db_path),
    );

    let (
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        scan_completed,
    ) = scan_result;
    let (object_store_files, object_store_bytes) = storage_result;

    let verdict = if !scan_completed {
        String::from("INCOMPLETE — iterator was invalidated before scan finished")
    } else if corrupt_values > 0 {
        format!(
            "CORRUPT — {corrupt_values} value(s) have unexpected length out of {content_keys} content entries"
        )
    } else if unknown_keys > 0 {
        format!("WARNING — {unknown_keys} key(s) with unrecognized prefix byte (not 0x01 or 0xFF)")
    } else if content_keys == 0 && total_keys == 0 {
        String::from("EMPTY — database contains no entries")
    } else {
        format!("OK — {content_keys} content entries verified, all values well-formed")
    };

    DeepIntegrityResult {
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        object_store_files,
        object_store_bytes,
        scan_completed,
        verdict,
    }
}

/// Iterate every key in the database, classify by prefix, and validate
/// value lengths for content keys.
async fn scan_all_keys(
    db_handle: Option<&std::sync::Arc<crate::metadata::metadb::Db>>,
    kind: DbKind,
) -> (u64, u64, u64, u64, u64, Vec<String>, bool) {
    let Some(db) = db_handle else {
        return (0, 0, 0, 0, 0, Vec::new(), false);
    };

    let mut iter = match db.scan().await {
        Ok(it) => it,
        Err(e) => {
            warn!(db = kind.label(), error = %e, "deep integrity: scan open failed");
            return (0, 0, 0, 0, 0, vec![format!("scan open failed: {e}")], false);
        }
    };

    let expected_value_len = kind.expected_value_len();
    let mut total_keys: u64 = 0;
    let mut content_keys: u64 = 0;
    let mut system_keys: u64 = 0;
    let mut unknown_keys: u64 = 0;
    let mut corrupt_values: u64 = 0;
    let mut corruption_samples: Vec<String> = Vec::new();

    loop {
        match iter.next().await {
            Ok(Some(kv)) => {
                total_keys += 1;
                let key = kv.key.as_ref();

                if key.first() == Some(&PREFIX_CONTENT) {
                    content_keys += 1;

                    // Validate key length.
                    if key.len() != CONTENT_KEY_LEN {
                        corrupt_values += 1;
                        if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                            corruption_samples.push(format!(
                                "key length {}, expected {CONTENT_KEY_LEN} (key prefix: {:02x?})",
                                key.len(),
                                &key[..key.len().min(4)]
                            ));
                        }
                        continue;
                    }

                    // Validate value length.
                    let value = kv.value.as_ref();
                    if value.len() != expected_value_len {
                        corrupt_values += 1;
                        if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                            let key_hex = short_hex(&key[1..key.len().min(9)]);
                            corruption_samples.push(format!(
                                "key 0x01{key_hex}…: value length {}, expected {expected_value_len}",
                                value.len()
                            ));
                        }
                    }
                } else if key.first() == Some(&PREFIX_SYSTEM) {
                    system_keys += 1;
                } else {
                    unknown_keys += 1;
                    if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                        corruption_samples.push(format!(
                            "unknown prefix byte {:#04x} on key of length {}",
                            key.first().copied().unwrap_or(0),
                            key.len()
                        ));
                    }
                }
            }
            Ok(None) => {
                // Scan completed normally.
                break;
            }
            Err(e) => {
                // Iterator invalidated (resource reclamation).
                warn!(
                    db = kind.label(),
                    error = %e,
                    scanned = total_keys,
                    "deep integrity: iterator invalidated mid-scan"
                );
                if corruption_samples.len() < MAX_CORRUPTION_SAMPLES {
                    corruption_samples
                        .push(format!("iterator invalidated after {total_keys} keys: {e}"));
                }
                return (
                    total_keys,
                    content_keys,
                    system_keys,
                    unknown_keys,
                    corrupt_values,
                    corruption_samples,
                    false,
                );
            }
        }
    }

    (
        total_keys,
        content_keys,
        system_keys,
        unknown_keys,
        corrupt_values,
        corruption_samples,
        true,
    )
}

/// Enumerate all object-store files under the database path and sum
/// their sizes.
async fn enumerate_storage(store: &Arc<dyn ObjectStore>, db_path: &str) -> (u64, u64) {
    let prefix = ObjectPath::from(db_path);
    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    let mut stream = store.list(Some(&prefix));
    loop {
        match stream.try_next().await {
            Ok(Some(meta)) => {
                file_count += 1;
                total_bytes += meta.size as u64;
            }
            Ok(None) => break,
            Err(e) => {
                warn!(path = db_path, error = %e, "deep integrity: object-store list failed");
                break;
            }
        }
    }

    (file_count, total_bytes)
}

/// Render a Unix millisecond timestamp as an ISO 8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`). Sub-second precision is dropped so the
/// output stays stable across locales. Inline so the diagnose text
/// render doesn't need a date-time crate.
fn unix_ms_to_iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let hours = tod / 3600;
    let minutes = (tod % 3600) / 60;
    let seconds = tod % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since the Unix epoch into a `(year, month, day)`
/// triple using Howard Hinnant's civil-from-days algorithm. Matches
/// the helper in `git::push::days_to_ymd` so the two formatters stay
/// consistent.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn render_diagnose(payload: &DiagnosePayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.diagnose", "1.0", payload);
        return;
    }

    println!("crab metadb diagnose\n");
    for db in [payload.file_index.as_ref(), payload.chunk_index.as_ref()]
        .into_iter()
        .flatten()
    {
        render_db_diagnosis(db);
    }
}

fn render_db_diagnosis(d: &DbDiagnosis) {
    println!("[{}]  path={}", d.label, d.path);
    if !d.opened {
        let err = d.error.as_deref().unwrap_or("<unknown>");
        println!("  status: FAILED TO OPEN — {err}");
        println!();
        return;
    }
    println!("  status: open");
    match d.format_version {
        Some(v) => println!("  format_version: {v}"),
        None => println!("  format_version: <unset>"),
    }
    match d.epoch {
        Some(v) => println!("  epoch: {v}"),
        None => println!("  epoch: <unset>"),
    }
    match d.created_at.as_deref() {
        Some(v) => println!("  created_at: {v}"),
        None => println!("  created_at: <unset>"),
    }
    match d.gc_generation {
        Some(v) => println!("  gc_generation: {v}"),
        None => println!("  gc_generation: <not applicable>"),
    }
    match &d.deep_integrity {
        Some(di) => {
            println!("  deep_integrity:");
            println!("    verdict: {}", di.verdict);
            println!("    total_keys: {}", di.total_keys);
            println!("    content_keys: {}", di.content_keys);
            println!("    system_keys: {}", di.system_keys);
            if di.unknown_keys > 0 {
                println!("    unknown_keys: {}", di.unknown_keys);
            }
            if di.corrupt_values > 0 {
                println!("    corrupt_values: {}", di.corrupt_values);
            }
            println!("    object_store_files: {}", di.object_store_files);
            println!(
                "    object_store_bytes: {} ({:.2} MiB)",
                di.object_store_bytes,
                di.object_store_bytes as f64 / (1024.0 * 1024.0)
            );
            println!("    scan_completed: {}", di.scan_completed);
            if !di.corruption_samples.is_empty() {
                println!("    corruption_samples:");
                for sample in &di.corruption_samples {
                    println!("      - {sample}");
                }
            }
        }
        None => {
            println!("  deep_integrity: not requested (use --deep to enable)");
        }
    }
    println!();
}

// --- rebuild --------------------------------------------------------

#[derive(Debug, Serialize)]
struct RebuildPayload {
    repo_prefix: String,
    file_index_entries_written: u64,
    chunk_index_entries_written: u64,
    legacy_file_rows_removed: u64,
    legacy_chunk_rows_removed: u64,
    shards_processed: u64,
    shards_failed: u64,
    git_packs_processed: u64,
    git_packs_failed: u64,
    git_objects_written: u64,
    elapsed_ms: u64,
    notes: Vec<String>,
}

/// Batch size used to flush accumulated entries into SlateDB during a
/// rebuild pass. Chosen to keep a single `Transaction` bounded in
/// memory without making the commit fan-out run at tiny-batch
/// granularity.
const REBUILD_COMMIT_BATCH: usize = 1000;

async fn run_rebuild(db: DbSelector, mode: OutputMode) -> Result<()> {
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store().await?;
    run_rebuild_in(store, repo_prefix, &bucket_identity, db, mode, &config).await
}

/// Core rebuild entry point parameterised on the object store and
/// repo prefix so tests can drive it against an in-memory store
/// without touching `resolve_repo_store`.
async fn run_rebuild_in(
    store: Arc<dyn ObjectStore>,
    repo_prefix: String,
    bucket_identity: &crate::storage::store::BucketIdentity,
    db: DbSelector,
    mode: OutputMode,
    config: &Config,
) -> Result<()> {
    // Rebuild writes fresh entries into one or both databases — must
    // use read-write mode, which fences any concurrent writer on
    // purpose.
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        bucket_identity,
        false,
        config,
    );
    let guard = MetaDbGuard::new(metadb);
    let emit_progress = !matches!(mode, OutputMode::Json);
    let result = rebuild_with_guard(&store, &repo_prefix, db, emit_progress, &guard).await;
    let payload = close_rebuild_guard(guard, result).await?;
    render_rebuild_payload(&payload, mode);
    Ok(())
}

/// Rebuild `file_index_db` for the current repository and verify that
/// selected file-to-shard mappings are present afterwards.
pub(crate) async fn rebuild_file_index_for_current_repo_and_verify(
    entries: &[(MerkleHash, MerkleHash)],
) -> Result<Vec<bool>> {
    let (store, repo_prefix, bucket_identity, config) = resolve_repo_store().await?;
    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        &bucket_identity,
        false,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let result: Result<Vec<bool>> = async {
        rebuild_with_guard(&store, &repo_prefix, DbSelector::FileIndex, false, &guard).await?;
        let file_store = guard.file_index().await?;
        let file_hashes: Vec<MerkleHash> =
            entries.iter().map(|(file_hash, _)| *file_hash).collect();
        let rebuilt = file_store.get_committed_batch(&file_hashes).await?;
        Ok(rebuilt
            .into_iter()
            .zip(entries.iter())
            .map(|(actual, (_, expected))| {
                actual.is_some_and(|record| record.shard_hash == *expected)
            })
            .collect())
    }
    .await;

    close_rebuild_guard(guard, result).await
}

async fn close_rebuild_guard<T>(guard: MetaDbGuard, result: Result<T>) -> Result<T> {
    let close_result = guard.close().await;
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(close_err)) => {
            warn!(error = %close_err, "metadb rebuild close failed after rebuild error");
            Err(err)
        }
    }
}

/// Inner rebuild driver. Kept separate so tests can feed a tempdir-
/// anchored `MetaDb` in without going through the `build_metadb`
/// cache-root plumbing.
async fn rebuild_with_guard(
    store: &Arc<dyn ObjectStore>,
    repo_prefix: &str,
    db: DbSelector,
    emit_progress: bool,
    guard: &MetaDbGuard,
) -> Result<RebuildPayload> {
    let start = std::time::Instant::now();
    let storage = crab_storage::Store::new(Arc::clone(store));
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let (manifest, _) = crab_metadata::manifest_store::read_manifest(&storage, &router).await?;
    let committed_shards = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_shard_list(
            &storage,
            &router,
            &manifest.shard_index_hash,
        )
        .await?
    };
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.shard_index_hash).map_err(|error| {
            CrabError::Internal(format!("manifest shard-index hash invalid: {error}"))
        })?
    };
    let gc_registry_generation = if db.includes_chunk_index() && !committed_shards.is_empty() {
        crab_metadata::ref_registry::union_register_repo_shards(
            &storage,
            &router,
            committed_shards.clone(),
        )
        .await?
    } else {
        0
    };

    let file_store = if db.includes_file_index() {
        Some(guard.file_index().await?)
    } else {
        None
    };
    let chunk_store = if db.includes_chunk_index() {
        Some(guard.chunk_index().await?)
    } else {
        None
    };

    let mut shards_processed: u64 = 0;
    let mut shards_failed: u64 = 0;
    let mut file_entries_written: u64 = 0;
    let mut chunk_entries_written: u64 = 0;
    let mut notes: Vec<String> = Vec::new();

    // Entries pending commit. Batched across shards so operators
    // don't pay one commit per shard when bucket shards are small.
    let mut pending_file: Vec<(MerkleHash, crab_metadata::value_codec::CommittedFileRecord)> =
        Vec::new();
    let mut pending_chunk: Vec<(MerkleHash, XorbRef)> = Vec::new();
    let mut pending_committed_chunk: Vec<(
        MerkleHash,
        crab_metadata::receipts::CommittedChunkReceipt,
    )> = Vec::new();
    let mut verified_xorbs = HashMap::new();

    for shard_hash_hex in committed_shards {
        let Ok(shard_hash) = MerkleHash::from_hex(&shard_hash_hex) else {
            warn!(shard_hash = %shard_hash_hex, "skipping committed shard with invalid hash");
            shards_failed += 1;
            continue;
        };
        let shard_path = router.shard_path(&shard_hash_hex);

        let body = match storage.get_with_etag(&shard_path).await {
            Ok((body, _)) => body,
            Err(e) => {
                warn!(shard = %shard_hash.hex(), error = %e, "failed to download shard during rebuild");
                shards_failed += 1;
                continue;
            }
        };
        if crab_xet::hash::compute_data_hash(&body) != shard_hash {
            warn!(shard = %shard_hash.hex(), "committed shard body hash mismatch during rebuild");
            shards_failed += 1;
            continue;
        }

        let recipes = match crab_xet::shard_parse::extract_file_recipes(&body) {
            Ok(recipes) => recipes,
            Err(error) => {
                warn!(shard = %shard_hash.hex(), error = %error, "failed to reconstruct committed shard recipes");
                shards_failed += 1;
                continue;
            }
        };

        let chunk_entries = if db.includes_chunk_index() {
            crab_xet::shard_parse::extract_chunk_entries_streaming(&body)
        } else {
            Vec::new()
        };
        let committed_chunk_entries = if chunk_entries.is_empty() {
            Vec::new()
        } else {
            match rebuild_committed_chunk_receipts(
                &storage,
                &router,
                repo_prefix,
                shard_hash,
                manifest.generation,
                shard_index_hash,
                gc_registry_generation,
                &chunk_entries,
                &mut verified_xorbs,
            )
            .await
            {
                Ok(receipts) => receipts,
                Err(error) => {
                    warn!(
                        shard = %shard_hash.hex(),
                        error = %error,
                        "committed shard xorb proof failed during rebuild"
                    );
                    shards_failed += 1;
                    continue;
                }
            }
        };

        if db.includes_file_index() {
            for recipe in recipes {
                let file_size = recipe.chunks.iter().try_fold(0u64, |total, (_, size)| {
                    total.checked_add(*size).ok_or_else(|| {
                        CrabError::StagingCorrupt("rebuilt recipe size overflow".to_owned())
                    })
                })?;
                let recipe_hash = FileRecipe::from_staged_chunks(
                    ChunkingPolicyId::XetGearV1_64KiB,
                    recipe.file_hash,
                    file_size,
                    &recipe.chunks,
                )?
                .hash();
                pending_file.push((
                    recipe.file_hash,
                    crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash,
                        shard_hash,
                        committed_generation: manifest.generation,
                        shard_index_hash,
                    },
                ));
            }
        }
        if db.includes_chunk_index() {
            pending_chunk.extend(chunk_entries);
            pending_committed_chunk.extend(committed_chunk_entries);
        }

        shards_processed += 1;

        // Flush in batches so a single transaction stays bounded.
        if pending_file.len() >= REBUILD_COMMIT_BATCH || pending_chunk.len() >= REBUILD_COMMIT_BATCH
        {
            let (fi, ci) = flush_rebuild_batch(
                guard,
                file_store.as_ref(),
                chunk_store.as_ref(),
                &mut pending_file,
                &mut pending_chunk,
                &mut pending_committed_chunk,
            )
            .await?;
            file_entries_written += fi;
            chunk_entries_written += ci;

            if emit_progress {
                println!(
                    "  rebuilding: {shards_processed} shard(s) processed, \
                     {file_entries_written} file entries / {chunk_entries_written} chunk entries emitted",
                );
            }
        }
    }

    // Flush any trailing entries below the batch threshold.
    let (fi, ci) = flush_rebuild_batch(
        guard,
        file_store.as_ref(),
        chunk_store.as_ref(),
        &mut pending_file,
        &mut pending_chunk,
        &mut pending_committed_chunk,
    )
    .await?;
    file_entries_written += fi;
    chunk_entries_written += ci;

    let (legacy_file_rows_removed, legacy_chunk_rows_removed) = if shards_failed == 0 {
        retire_legacy_namespaces(guard, file_store.as_ref(), chunk_store.as_ref()).await?
    } else {
        notes.push(
            "legacy namespaces retained because one or more committed shards failed proof"
                .to_owned(),
        );
        (0, 0)
    };

    let (git_packs_processed, git_packs_failed, git_objects_written, git_object_locator_digest) =
        if matches!(db, DbSelector::Both) {
            rebuild_git_object_locators(&storage, &router, &manifest).await?
        } else {
            (0, 0, 0, [0; 32])
        };

    if matches!(db, DbSelector::Both) && shards_failed == 0 && git_packs_failed == 0 {
        let pack_index_hash = if manifest.pack_index_hash.is_empty() {
            MerkleHash::default()
        } else {
            MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
                CrabError::Internal(format!("manifest pack-index hash invalid: {error}"))
            })?
        };
        let receipt = crab_metadata::receipts::GenerationIndexReceipt {
            schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
            generation: manifest.generation,
            shard_index_hash: shard_index_hash.into(),
            pack_index_hash: pack_index_hash.into(),
            file_index_digest: crab_metadata::receipts::generation_file_index_digest(
                shard_index_hash.into(),
            ),
            git_object_locator_digest,
        };
        receipt
            .validate(
                manifest.generation,
                shard_index_hash.into(),
                pack_index_hash.into(),
            )
            .map_err(CrabError::from)?;
        let receipt_path = router.repo_path(&format!(
            "metadata/generation-receipts/{:020}.json",
            manifest.generation
        ));
        let body = serde_json::to_vec(&receipt)
            .map_err(|error| CrabError::Internal(format!("receipt serialize: {error}")))?;
        match storage.put(&receipt_path, Bytes::from(body.clone())).await {
            Ok(()) => {}
            Err(crab_storage::StorageError::StateConflict { .. }) => {
                let (existing, _) = storage.get_with_etag(&receipt_path).await?;
                let existing: crab_metadata::receipts::GenerationIndexReceipt =
                    serde_json::from_slice(&existing).map_err(|error| {
                        CrabError::CorruptObject {
                            path: receipt_path.to_string(),
                            reason: format!("generation-index receipt decode failed: {error}"),
                        }
                    })?;
                existing
                    .validate(
                        manifest.generation,
                        shard_index_hash.into(),
                        pack_index_hash.into(),
                    )
                    .map_err(CrabError::from)?;
                if existing != receipt {
                    return Err(CrabError::CorruptObject {
                        path: receipt_path.to_string(),
                        reason:
                            "generation-index receipt conflicts with the committed index digest"
                                .to_owned(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    if shards_processed == 0 {
        notes.push("manifest has no committed shards; nothing to rebuild".to_owned());
    }
    if shards_failed > 0 {
        notes.push(format!(
            "{shards_failed} shard(s) skipped after download or parse failure; see warn! logs"
        ));
    }
    if !db.includes_file_index() {
        notes.push(String::from("--db chunk_index: file-index entries skipped"));
    }
    if !db.includes_chunk_index() {
        notes.push(String::from("--db file_index: chunk-index entries skipped"));
    }

    let payload = RebuildPayload {
        repo_prefix: String::from(repo_prefix),
        file_index_entries_written: file_entries_written,
        chunk_index_entries_written: chunk_entries_written,
        legacy_file_rows_removed,
        legacy_chunk_rows_removed,
        shards_processed,
        shards_failed,
        git_packs_processed,
        git_packs_failed,
        git_objects_written,
        elapsed_ms: start.elapsed().as_millis() as u64,
        notes,
    };

    Ok(payload)
}

async fn rebuild_git_object_locators(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    manifest: &crab_metadata::manifests::Manifest,
) -> Result<(u64, u64, u64, [u8; 32])> {
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
            CrabError::Internal(format!("manifest pack-index hash invalid: {error}"))
        })?
    };
    let packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
            .await?
    };
    let visibility_temp = tempfile::tempdir()?;
    let visibility_pack_dir = visibility_temp.path().join("objects/pack");
    std::fs::create_dir_all(&visibility_pack_dir)?;
    let mut failed = 0u64;
    let mut derived = Vec::with_capacity(packs.len());
    for pack in packs {
        let pack_id = match MerkleHash::from_hex(&pack.pack_id) {
            Ok(pack_id) => pack_id,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "skipping invalid committed pack id during locator rebuild");
                failed += 1;
                continue;
            }
        };
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pack");
        let downloaded = match store
            .download_to_path(&router.pack_path(&pack.pack_id), &source)
            .await
        {
            Ok(size) => size,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "failed to read committed pack during locator rebuild");
                failed += 1;
                continue;
            }
        };
        if downloaded != pack.size {
            warn!(pack_id = %pack.pack_id, downloaded, expected = pack.size, "committed pack size mismatch during locator rebuild");
            failed += 1;
            continue;
        }
        let canonical_name = pack.pack_id.clone();
        let expected_object_count = pack.object_count;
        let visibility_pack_dir_for_pack = visibility_pack_dir.clone();
        let verified = tokio::task::spawn_blocking(move || -> Result<_> {
            let pack_dir = temp.path().join("objects/pack");
            std::fs::create_dir_all(&pack_dir)?;
            let installed = crab_git::pack::install_pack_file_from_path(
                &pack_dir,
                &source,
                &canonical_name,
                0,
                false,
            )?;
            let mut locations = crab_git::pack_locator::PackLocationIter::open(
                &installed.idx_path,
                &installed.rev_path,
                downloaded,
            )
            .map_err(crab_git::pack::PackError::from)?;
            if locations.object_count() != expected_object_count {
                return Err(CrabError::CorruptObject {
                    path: source.display().to_string(),
                    reason: format!(
                        "manifest records {expected_object_count} objects but index contains {}",
                        locations.object_count()
                    ),
                });
            }
            if locations.pack_checksum().to_string() != installed.git_sha1 {
                return Err(CrabError::CorruptObject {
                    path: source.display().to_string(),
                    reason: "pack index checksum disagrees with pack trailer".to_owned(),
                });
            }
            crab_git::pack::install_pack_file_from_path(
                &visibility_pack_dir_for_pack,
                &source,
                &canonical_name,
                0,
                false,
            )?;
            let sample_indexes = sampled_location_indexes(locations.len());
            let mut samples = Vec::with_capacity(sample_indexes.len());
            for (index, location) in (&mut locations).enumerate() {
                let location = location.map_err(crab_git::pack::PackError::from)?;
                if sample_indexes.binary_search(&index).is_ok() {
                    samples.push(location);
                }
            }
            verify_sampled_pack_ranges(&source, &samples)?;
            let mut file = std::fs::File::open(&source)?;
            let mut hasher = blake3::Hasher::new();
            std::io::copy(&mut file, &mut hasher)?;
            let mut index_file = std::fs::File::open(&installed.idx_path)?;
            let index_size = index_file.metadata()?.len();
            let mut index_hasher = blake3::Hasher::new();
            std::io::copy(&mut index_file, &mut index_hasher)?;
            Ok((
                hasher.finalize().to_hex().to_string(),
                temp,
                installed.idx_path,
                installed.rev_path,
                installed.git_sha1,
                index_size,
                *index_hasher.finalize().as_bytes(),
            ))
        })
        .await
        .map_err(|error| CrabError::Internal(format!("pack rebuild join failed: {error}")))?;
        let (
            actual_pack_id,
            temp,
            index_path,
            reverse_index_path,
            git_sha1,
            index_size,
            index_hash,
        ) = match verified {
            Ok(verified) => verified,
            Err(error) => {
                warn!(pack_id = %pack.pack_id, error = %error, "committed pack verification failed during locator rebuild");
                failed += 1;
                continue;
            }
        };
        if actual_pack_id != pack.pack_id {
            warn!(pack_id = %pack.pack_id, "committed pack hash mismatch during locator rebuild");
            failed += 1;
            continue;
        }
        if let Err(error) = store
            .put_multipart_file_retry(
                &router.pack_index_path(&pack.pack_id),
                &index_path,
                index_size,
                index_hash,
                8 * 1024 * 1024,
                &tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
        {
            warn!(pack_id = %pack.pack_id, error = %error, "failed to upload rebuilt canonical pack index");
            failed += 1;
            continue;
        }
        derived.push((
            pack,
            pack_id,
            temp,
            index_path,
            reverse_index_path,
            git_sha1,
        ));
    }

    if failed != 0 {
        return Ok((
            0,
            failed,
            0,
            crab_metadata::receipts::generation_git_object_locator_digest(pack_index_hash.into()),
        ));
    }

    let lock = crab_coordination::PushLock::acquire_internal_default(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
    )
    .await?;
    let write_result = crate::git::push::while_renewing_internal_lock(&lock, async {
        let (current, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
        {
            return Err(CrabError::CasConflict {
                path: router.manifest_path().as_ref().to_owned(),
                expected_etag: None,
            });
        }
        let records = derived
            .iter()
            .map(|(pack, pack_id, _, _, _, _)| {
                crab_metadata::git_object_locator::GitPackLocatorRecord {
                    pack_id: *pack_id,
                    committed_generation: manifest.generation,
                    pack_index_hash,
                    object_count: pack.object_count,
                    pack_size: pack.size,
                }
            })
            .collect::<Vec<_>>();
        let mut writer = crab_metadata::git_object_locator::GitObjectLocatorWriter::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await?;
        let operation = async {
            let bindings = writer.bind_packs(&records).await?;
            let retained_slots: HashSet<_> =
                bindings.iter().map(|binding| binding.pack_slot).collect();
            for (binding, (_, _, _, index_path, reverse_index_path, git_sha1)) in
                bindings.into_iter().zip(&derived)
            {
                let mut locations = crab_git::pack_locator::PackLocationIter::open(
                    index_path,
                    reverse_index_path,
                    binding.record.pack_size,
                )
                .map_err(crab_git::pack::PackError::from)?;
                if locations.pack_checksum().to_string() != *git_sha1 {
                    return Err(CrabError::CorruptObject {
                        path: index_path.display().to_string(),
                        reason: "pack index checksum changed during locator rebuild".to_owned(),
                    });
                }
                let mut entries = Vec::with_capacity(25_000);
                for location in &mut locations {
                    let location = location.map_err(crab_git::pack::PackError::from)?;
                    let oid = location.oid.as_bytes().try_into().map_err(|_| {
                        CrabError::Internal(
                            "rebuilt pack index contains non-SHA1 object".to_owned(),
                        )
                    })?;
                    entries.push(crab_metadata::git_object_locator::GitObjectLocatorEntry {
                        oid,
                        location: crab_metadata::git_object_locator::GitObjectLocation {
                            pack_offset: location.pack_offset,
                            entry_len: location.entry_len,
                            crc32: location.crc32,
                        },
                    });
                    if entries.len() == 25_000 {
                        writer.write_locations(binding, &entries).await?;
                        entries.clear();
                    }
                }
                if !entries.is_empty() {
                    writer.write_locations(binding, &entries).await?;
                }
            }
            writer.flush_objects().await?;
            writer.sweep_unreferenced(&retained_slots).await?;
            let (after, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
            if after.generation != manifest.generation
                || after.pack_index_hash != manifest.pack_index_hash
            {
                return Err(CrabError::CasConflict {
                    path: router.manifest_path().as_ref().to_owned(),
                    expected_etag: None,
                });
            }
            writer
                .set_coverage(crab_metadata::git_object_locator::GitLocatorCoverage {
                    generation: manifest.generation,
                    pack_index_hash,
                })
                .await?;
            Ok::<_, CrabError>(())
        }
        .await;
        let close_result = writer.close().await.map_err(CrabError::from);
        match (operation, close_result) {
            (Ok(()), Ok(stats)) if stats.coverage_updated => Ok(stats),
            (Ok(()), Ok(_)) => Err(CrabError::Internal(
                "rebuilt Git locator did not advance coverage".to_owned(),
            )),
            (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(close_error)) => {
                warn!(error = %close_error, "Git locator close also failed after rebuild error");
                Err(error)
            }
        }
    })
    .await;
    let release_result = lock.release().await.map_err(CrabError::from);
    let _stats = write_result?;
    release_result?;
    crate::git::push::publish_git_visibility_index_from_storage_git_dir(
        visibility_temp.path(),
        manifest,
        store,
        router,
    )
    .await?;
    let processed = u64::try_from(derived.len()).map_err(|_| {
        CrabError::Internal("rebuilt Git pack count cannot be represented".to_owned())
    })?;
    let objects_written = derived
        .iter()
        .map(|(pack, _, _, _, _, _)| pack.object_count)
        .sum();
    Ok((
        processed,
        failed,
        objects_written,
        crab_metadata::receipts::generation_git_object_locator_digest(pack_index_hash.into()),
    ))
}

fn sampled_location_indexes(location_count: usize) -> Vec<usize> {
    if location_count == 0 {
        return Vec::new();
    }
    let mut indexes = vec![0, location_count / 2, location_count - 1];
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

fn verify_sampled_pack_ranges(
    pack_path: &Path,
    locations: &[crab_git::pack_locator::PackObjectLocation],
) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    if locations.is_empty() {
        return Ok(());
    }
    let mut sample_indexes = vec![0, locations.len() / 2, locations.len() - 1];
    sample_indexes.sort_unstable();
    sample_indexes.dedup();
    let mut file = std::fs::File::open(pack_path)?;
    let mut buffer = vec![0u8; 64 * 1024];
    for index in sample_indexes {
        let location = &locations[index];
        file.seek(SeekFrom::Start(location.pack_offset))?;
        let mut remaining = location.entry_len;
        let mut hasher = crc32fast::Hasher::new();
        while remaining > 0 {
            let read_len = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| CrabError::Internal("sample range length overflow".to_owned()))?;
            let bytes_read = file.read(&mut buffer[..read_len])?;
            if bytes_read == 0 {
                return Err(CrabError::CorruptObject {
                    path: pack_path.display().to_string(),
                    reason: format!(
                        "sampled object {} ends before its indexed range",
                        location.oid
                    ),
                });
            }
            hasher.update(&buffer[..bytes_read]);
            remaining = remaining.saturating_sub(bytes_read as u64);
        }
        let actual = hasher.finalize();
        if actual != location.crc32 {
            return Err(CrabError::CorruptObject {
                path: pack_path.display().to_string(),
                reason: format!(
                    "sampled object {} CRC mismatch: expected {:08x}, got {actual:08x}",
                    location.oid, location.crc32
                ),
            });
        }
    }
    Ok(())
}

fn render_rebuild_payload(payload: &RebuildPayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.rebuild", "1.0", &payload);
    } else {
        println!("\ncrab metadb rebuild\n");
        println!("  repo_prefix:                 {}", payload.repo_prefix);
        println!(
            "  shards_processed:            {}",
            payload.shards_processed
        );
        println!("  shards_failed:               {}", payload.shards_failed);
        println!(
            "  git_packs_processed:         {}",
            payload.git_packs_processed
        );
        println!(
            "  git_packs_failed:            {}",
            payload.git_packs_failed
        );
        println!(
            "  git_objects_written:         {}",
            payload.git_objects_written
        );
        println!(
            "  file_index_entries_written:  {}",
            payload.file_index_entries_written
        );
        println!(
            "  chunk_index_entries_written: {}",
            payload.chunk_index_entries_written
        );
        println!(
            "  legacy_file_rows_removed:    {}",
            payload.legacy_file_rows_removed
        );
        println!(
            "  legacy_chunk_rows_removed:   {}",
            payload.legacy_chunk_rows_removed
        );
        println!("  elapsed_ms:                  {}", payload.elapsed_ms);
        if !payload.notes.is_empty() {
            println!("  notes:");
            for note in &payload.notes {
                println!("    - {note}");
            }
        }
    }

    info!(
        shards_processed = payload.shards_processed,
        shards_failed = payload.shards_failed,
        file_entries_written = payload.file_index_entries_written,
        chunk_entries_written = payload.chunk_index_entries_written,
        legacy_file_rows_removed = payload.legacy_file_rows_removed,
        legacy_chunk_rows_removed = payload.legacy_chunk_rows_removed,
        git_packs_processed = payload.git_packs_processed,
        git_packs_failed = payload.git_packs_failed,
        git_objects_written = payload.git_objects_written,
        elapsed_ms = payload.elapsed_ms,
        "metadb rebuild complete"
    );
}

/// Flush the pending per-database entry buffers through one
/// `MetaDb::commit`, returning `(file_written, chunk_written)`.
///
/// Empty buffers produce a zero-op commit which the session
/// short-circuits, so this is safe to call unconditionally after
/// every shard.
#[derive(Clone)]
struct RebuildVerifiedXorb {
    origin: crab_metadata::receipts::OriginReceipt,
    chunks: Vec<(MerkleHash, u32)>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "repair proof binds independent repository, manifest, registry, shard, and placement anchors"
)]
async fn rebuild_committed_chunk_receipts(
    storage: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    repo_prefix: &str,
    source_shard_hash: MerkleHash,
    committed_generation: u64,
    shard_index_hash: MerkleHash,
    gc_registry_generation: u64,
    entries: &[(MerkleHash, XorbRef)],
    verified_xorbs: &mut HashMap<MerkleHash, RebuildVerifiedXorb>,
) -> Result<Vec<(MerkleHash, crab_metadata::receipts::CommittedChunkReceipt)>> {
    if committed_generation == 0 || gc_registry_generation == 0 {
        return Err(CrabError::Internal(
            "committed chunk rebuild requires manifest and GC registry generations".to_owned(),
        ));
    }
    let mut receipts = Vec::with_capacity(entries.len());
    for (chunk_hash, xorb_ref) in entries {
        if !verified_xorbs.contains_key(&xorb_ref.xorb_hash) {
            let path = router.xorb_path(&xorb_ref.xorb_hash);
            let (body, etag) = storage.get_with_etag(&path).await?;
            let parser = XorbParser::parse(body.clone())?;
            if parser.hash() != xorb_ref.xorb_hash {
                return Err(CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "xorb metadata hash mismatch: expected {}, got {}",
                        xorb_ref.xorb_hash.hex(),
                        parser.hash().hex()
                    ),
                });
            }
            parser.verify_payload_digest()?;
            parser.verify_all_chunks()?;
            let mut chunks = Vec::with_capacity(parser.num_chunks() as usize);
            for index in 0..parser.num_chunks() {
                let meta = parser.chunk_meta(index)?;
                chunks.push((meta.hash, meta.uncompressed_len));
            }
            verified_xorbs.insert(
                xorb_ref.xorb_hash,
                RebuildVerifiedXorb {
                    origin: crab_metadata::receipts::OriginReceipt::new(
                        "canonical-origin".to_owned(),
                        path.to_string(),
                        xorb_ref.xorb_hash.into(),
                        parser.payload_digest(),
                        body.len() as u64,
                        etag.e_tag,
                        etag.version,
                    ),
                    chunks,
                },
            );
        }
        let verified = verified_xorbs.get(&xorb_ref.xorb_hash).ok_or_else(|| {
            CrabError::Internal("verified xorb cache insertion was lost".to_owned())
        })?;
        let index =
            usize::try_from(xorb_ref.chunk_index).map_err(|_| CrabError::CorruptObject {
                path: router.xorb_path(&xorb_ref.xorb_hash).to_string(),
                reason: "chunk index cannot be represented".to_owned(),
            })?;
        if verified.chunks.get(index) != Some(&(*chunk_hash, xorb_ref.uncompressed_size)) {
            return Err(CrabError::CorruptObject {
                path: router.xorb_path(&xorb_ref.xorb_hash).to_string(),
                reason: format!(
                    "shard placement for chunk {} does not match xorb index {}",
                    chunk_hash.hex(),
                    xorb_ref.chunk_index
                ),
            });
        }
        receipts.push((
            *chunk_hash,
            crab_metadata::receipts::CommittedChunkReceipt {
                schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
                chunk_hash: (*chunk_hash).into(),
                xorb_hash: xorb_ref.xorb_hash.into(),
                chunk_index: xorb_ref.chunk_index,
                uncompressed_size: xorb_ref.uncompressed_size,
                origin: verified.origin.clone(),
                source_repo_prefix: repo_prefix.to_owned(),
                source_shard_hash: source_shard_hash.into(),
                committed_generation,
                shard_index_hash: shard_index_hash.into(),
                gc_registry_generation,
            },
        ));
    }
    Ok(receipts)
}

async fn flush_rebuild_batch(
    guard: &MetaDbGuard,
    file_store: Option<&crate::metadata::FileIndexStore>,
    chunk_store: Option<&crate::metadata::ChunkIndexStore>,
    pending_file: &mut Vec<(MerkleHash, crab_metadata::value_codec::CommittedFileRecord)>,
    pending_chunk: &mut Vec<(MerkleHash, XorbRef)>,
    pending_committed_chunk: &mut Vec<(MerkleHash, crab_metadata::receipts::CommittedChunkReceipt)>,
) -> Result<(u64, u64)> {
    if pending_file.is_empty() && pending_chunk.is_empty() && pending_committed_chunk.is_empty() {
        return Ok((0, 0));
    }

    let mut txn = guard.new_transaction()?;
    let file_written = match file_store {
        Some(store) if !pending_file.is_empty() => {
            for (file_hash, _) in pending_file.iter() {
                store.delete_legacy(&mut txn, file_hash);
            }
            store.save_committed_batch(&mut txn, pending_file);
            pending_file.len() as u64
        }
        _ => 0,
    };
    let chunk_written = match chunk_store {
        Some(store) if !pending_chunk.is_empty() || !pending_committed_chunk.is_empty() => {
            for (chunk_hash, _) in pending_committed_chunk.iter() {
                store.delete_legacy(&mut txn, chunk_hash);
            }
            store.save_committed_receipts(&mut txn, pending_committed_chunk)?;
            pending_chunk.len() as u64
        }
        _ => 0,
    };

    guard.commit(txn).await?;
    pending_file.clear();
    pending_chunk.clear();
    pending_committed_chunk.clear();
    Ok((file_written, chunk_written))
}

async fn retire_legacy_namespaces(
    guard: &MetaDbGuard,
    file_store: Option<&crate::metadata::FileIndexStore>,
    chunk_store: Option<&crate::metadata::ChunkIndexStore>,
) -> Result<(u64, u64)> {
    let mut file_removed = 0u64;
    let mut chunk_removed = 0u64;
    loop {
        let file_keys = match file_store {
            Some(store) => store.legacy_keys_batch(REBUILD_COMMIT_BATCH).await?,
            None => Vec::new(),
        };
        let chunk_keys = match chunk_store {
            Some(store) => store.legacy_keys_batch(REBUILD_COMMIT_BATCH).await?,
            None => Vec::new(),
        };
        if file_keys.is_empty() && chunk_keys.is_empty() {
            break;
        }

        let mut txn = guard.new_transaction()?;
        if let Some(store) = file_store {
            store.delete_legacy_keys(&mut txn, &file_keys);
        }
        if let Some(store) = chunk_store {
            store.delete_legacy_keys(&mut txn, &chunk_keys);
        }
        guard.commit(txn).await?;
        file_removed = file_removed.saturating_add(file_keys.len() as u64);
        chunk_removed = chunk_removed.saturating_add(chunk_keys.len() as u64);
    }
    Ok((file_removed, chunk_removed))
}

// --- compact --------------------------------------------------------

async fn run_compact(_db: DbSelector) -> Result<()> {
    warn!(
        "crab metadb compact: SlateDB compaction runs automatically in the background; \
         this subcommand currently has no effect. It is provided so operator runbooks can \
         call the command without \"unknown subcommand\" errors."
    );
    info!("metadb compact invoked (no-op)");
    Ok(())
}

// --- cache ----------------------------------------------------------

fn default_local_chunk_index_path() -> Result<PathBuf> {
    let remote = crate::git::discover::resolve_crab_dir()
        .map(|d| d.join("remote"))
        .unwrap_or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_default();
            cwd.join(".crab/remote")
        });
    if let Ok(url) = std::fs::read_to_string(&remote) {
        if let Ok(parsed) = crate::git::url::ObjectUrl::parse(url.trim()) {
            return Ok(crate::cache::chunk_index_cache_path(
                &crate::cache::default_cache_root(),
                &parsed.bucket_identity(),
            ));
        }
    }
    // Fall back to a generic path under the cache root so `cache
    // stats` still works outside a repo.
    Ok(crate::cache::chunk_index_cache_path(
        &crate::cache::default_cache_root(),
        &crate::storage::store::BucketIdentity::local_unset(),
    ))
}

fn run_cache_stats(mode: OutputMode) -> Result<()> {
    let path = default_local_chunk_index_path()?;
    let payload = cache_stats_for(&path)?;

    if matches!(mode, OutputMode::Json) {
        emit_json("metadb.cache.stats", "1.0", &payload);
    } else {
        println!("crab metadb cache stats\n");
        println!("  path:              {}", payload.cache_path);
        println!("  exists:            {}", payload.exists);
        println!("  file_size_bytes:   {}", payload.file_size_bytes);
        println!("  entry_count:       {}", payload.entry_count);
        println!("  installed_shards:  {}", payload.installed_shard_count);
        println!("  cache_gc_generation: {}", payload.cache_gc_generation);
    }
    Ok(())
}

fn cache_stats_for(path: &Path) -> Result<CacheStatsPayload> {
    let cache_path = path.display().to_string();
    if !path.exists() {
        return Ok(CacheStatsPayload {
            cache_path,
            exists: false,
            file_size_bytes: 0,
            entry_count: 0,
            installed_shard_count: 0,
            cache_gc_generation: 0,
        });
    }
    let file_size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Reuse the process-shared handle so diagnostics run through the
    // same SQLite connection queue as push/import code in this process.
    let index = crab_metadata::persistent_chunk_index::PersistentChunkIndex::open_shared(path)?;
    let entries = index.load_all()?.len() as u64;
    let installed_shards = index.installed_shards()?.len() as u64;
    let cache_gc_generation = index.cache_gc_generation()?;

    Ok(CacheStatsPayload {
        cache_path,
        exists: true,
        file_size_bytes,
        entry_count: entries,
        installed_shard_count: installed_shards,
        cache_gc_generation,
    })
}

fn run_cache_clear() -> Result<()> {
    let path = default_local_chunk_index_path()?;
    if !path.exists() {
        println!(
            "crab metadb cache clear: nothing to do (no cache file at {})",
            path.display()
        );
        return Ok(());
    }
    std::fs::remove_file(&path)
        .map_err(|e| CrabError::Internal(format!("could not remove {}: {e}", path.display())))?;
    println!("crab metadb cache clear: removed {}", path.display());
    info!(path = %path.display(), "metadb cache cleared");
    Ok(())
}

// --- `crab doctor --metadb` --------------------------------------

/// Entry point for the `crab doctor --metadb` subcommand.
///
/// Emits a text (or JSON) report covering open-state for both
/// databases, a rough shard count under `.crab/shards/`, and the
/// local cache stats. The report intentionally stays shallow:
/// anything deeper (WAL replay, bloom validation) belongs to
/// `crab metadb diagnose`.
pub async fn run_doctor_metadb_in(root: &Path, mode: OutputMode) -> Result<()> {
    let (store, repo_prefix, bucket_identity, config) = match resolve_repo_store_in(root).await {
        Ok(v) => v,
        Err(e) => {
            // No remote configured — still emit something useful.
            let empty = DoctorMetadbPayload {
                repo_prefix: String::from("<unconfigured>"),
                file_index: DbDiagnosis {
                    label: "file_index_db",
                    path: String::new(),
                    opened: false,
                    error: Some(e.to_string()),
                    format_version: None,
                    epoch: None,
                    created_at: None,
                    gc_generation: None,
                    deep_integrity: None,
                },
                chunk_index: DbDiagnosis {
                    label: "chunk_index_db",
                    path: String::new(),
                    opened: false,
                    error: Some(String::from("skipped: no remote configured")),
                    format_version: None,
                    epoch: None,
                    created_at: None,
                    gc_generation: None,
                    deep_integrity: None,
                },
                shards_prefix: String::from("<unknown>"),
                shard_count: None,
                shard_enumeration_error: None,
                cache: cache_stats_for(&crate::cache::chunk_index_cache_path(
                    &crate::cache::default_cache_root(),
                    &crate::storage::store::BucketIdentity::local_unset(),
                ))
                .unwrap_or_else(|_| CacheStatsPayload {
                    cache_path: String::new(),
                    exists: false,
                    file_size_bytes: 0,
                    entry_count: 0,
                    installed_shard_count: 0,
                    cache_gc_generation: 0,
                }),
                acceleration: AccelerationHealth::unavailable(
                    "remote is not configured; generation/index proof unavailable",
                ),
            };
            render_doctor_metadb(&empty, mode);
            return Ok(());
        }
    };

    let metadb = build_metadb(
        Arc::clone(&store),
        repo_prefix.clone(),
        &bucket_identity,
        true,
        &config,
    );
    let guard = MetaDbGuard::new(metadb);

    let metadb_config = config.build_metadb_config(&repo_prefix);
    let file_index =
        diagnose_file_index(&guard, false, &store, &metadb_config.file_index_path).await;
    let chunk_index =
        diagnose_chunk_index(&guard, false, &store, &metadb_config.chunk_index_path).await;

    // Shard count (best effort). Shards live at the bucket-global
    // `.crab/shards/` prefix, never under the per-repo prefix —
    // content-addressed xorbs and shards are shared across every
    // repo in the bucket.
    let shards_prefix = String::from(".crab/shards/");
    let shards_path = ObjectPath::from(shards_prefix.as_str());
    let (shard_count, shard_enumeration_error) = match count_shards(&store, &shards_path).await {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e.to_string())),
    };

    let cache_path =
        crate::cache::chunk_index_cache_path(&crate::cache::default_cache_root(), &bucket_identity);
    let cache = cache_stats_for(&cache_path).unwrap_or_else(|_| CacheStatsPayload {
        cache_path: cache_path.display().to_string(),
        exists: false,
        file_size_bytes: 0,
        entry_count: 0,
        installed_shard_count: 0,
        cache_gc_generation: 0,
    });
    let acceleration = diagnose_acceleration_health(&store, &repo_prefix).await;

    let payload = DoctorMetadbPayload {
        repo_prefix,
        file_index,
        chunk_index,
        shards_prefix,
        shard_count,
        shard_enumeration_error,
        cache,
        acceleration,
    };

    render_doctor_metadb(&payload, mode);
    guard.close().await?;
    Ok(())
}

async fn diagnose_acceleration_health(
    store: &Arc<dyn ObjectStore>,
    repo_prefix: &str,
) -> AccelerationHealth {
    let storage = crab_storage::Store::new(Arc::clone(store));
    let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
    let (manifest, _) = match crab_metadata::manifest_store::read_manifest(&storage, &router).await
    {
        Ok(manifest) => manifest,
        Err(error) => {
            return AccelerationHealth::unavailable(format!(
                "manifest unavailable: {error}; retry remote access before repair"
            ));
        }
    };
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        match MerkleHash::from_hex(&manifest.shard_index_hash) {
            Ok(hash) => hash,
            Err(error) => {
                return AccelerationHealth::unavailable(format!(
                    "manifest shard-index hash is invalid: {error}"
                ));
            }
        }
    };
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        match MerkleHash::from_hex(&manifest.pack_index_hash) {
            Ok(hash) => hash,
            Err(error) => {
                return AccelerationHealth::unavailable(format!(
                    "manifest pack-index hash is invalid: {error}"
                ));
            }
        }
    };
    let mut notes = Vec::new();
    let (
        git_visibility_index_available,
        git_visibility_covered_generation,
        git_visibility_covered_pack_index_hash,
        git_visibility_coverage_current,
    ) = if manifest.refs.is_empty() {
        // Empty repositories have no pack-index identity to bind. The
        // protocol uses an empty in-memory proof for this immutable state.
        (true, Some(manifest.generation), None, true)
    } else if manifest.pack_index_hash.is_empty() {
        notes.push(
            "Git visibility proof has no pack-index identity; run `crab metadb rebuild`".to_owned(),
        );
        (false, None, None, false)
    } else {
        match crab_metadata::git_visibility::read(
            &storage,
            &router,
            manifest.generation,
            &manifest.pack_index_hash,
        )
        .await
        {
            Ok(index) => {
                let covers_manifest = index.refs.len() == manifest.refs.len()
                    && manifest.refs.iter().all(|(name, oid)| {
                        index.refs.get(name).is_some_and(|objects| {
                            objects.binary_search(oid).is_ok()
                                && manifest
                                    .peeled_refs
                                    .get(name)
                                    .is_none_or(|peeled| objects.binary_search(peeled).is_ok())
                        })
                    });
                if !covers_manifest {
                    notes.push(
                        "Git visibility proof does not cover the current manifest refs; run `crab metadb rebuild`"
                            .to_owned(),
                    );
                }
                (
                    true,
                    Some(index.generation),
                    Some(index.pack_index_hash),
                    covers_manifest,
                )
            }
            Err(error) => {
                notes.push(format!(
                    "Git visibility proof unavailable: {error}; run `crab metadb rebuild`"
                ));
                (false, None, None, false)
            }
        }
    };
    let receipt_path = router.repo_path(&format!(
        "metadata/generation-receipts/{:020}.json",
        manifest.generation
    ));
    let generation_receipt_valid = match storage.get_with_etag(&receipt_path).await {
        Ok((body, _)) => {
            serde_json::from_slice::<crab_metadata::receipts::GenerationIndexReceipt>(&body)
                .map_err(|error| error.to_string())
                .and_then(|receipt| {
                    receipt
                        .validate(
                            manifest.generation,
                            shard_index_hash.into(),
                            pack_index_hash.into(),
                        )
                        .map_err(|error| error.to_string())
                })
                .map(|()| true)
                .unwrap_or_else(|error| {
                    notes.push(format!("generation-index receipt invalid: {error}"));
                    false
                })
        }
        Err(crab_storage::StorageError::NotFound { .. }) => {
            notes.push("generation-index receipt missing; run `crab metadb rebuild`".to_owned());
            false
        }
        Err(error) => {
            notes.push(format!("generation-index receipt unreadable: {error}"));
            false
        }
    };

    let (ref_registry_repo_complete, ref_registry_bucket_complete) = match storage
        .get_with_etag(&router.ref_registry_path())
        .await
    {
        Ok((body, _)) => {
            match serde_json::from_slice::<crab_metadata::ref_registry::RefRegistry>(&body) {
                Ok(registry) => {
                    let repo_complete = registry.schema_version
                        == crab_metadata::ref_registry::REF_REGISTRY_SCHEMA_VERSION
                        && registry.complete_repos.contains(repo_prefix)
                        && registry.repos.contains_key(repo_prefix);
                    let bucket_complete = registry.is_complete_for_destructive_gc();
                    if !repo_complete {
                        notes.push(
                            "repository GC roots are incomplete; run `crab gc --repair-registry --bucket <bucket>`"
                                .to_owned(),
                        );
                    }
                    if !bucket_complete {
                        notes.push(
                            "bucket registry discovery is incomplete; destructive bucket GC remains disabled"
                                .to_owned(),
                        );
                    }
                    (repo_complete, bucket_complete)
                }
                Err(error) => {
                    notes.push(format!("ref registry is corrupt: {error}"));
                    (false, false)
                }
            }
        }
        Err(error) => {
            notes.push(format!("ref registry unavailable: {error}"));
            (false, false)
        }
    };

    let git_session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
        Arc::clone(store),
        repo_prefix,
    )
    .await;
    let (
        git_locator_index_available,
        git_locator_covered_generation,
        git_locator_covered_pack_index_hash,
    ) = match git_session {
        Ok(session) => {
            let available = session.is_available();
            let coverage = session.coverage();
            if let Err(error) = session.close().await {
                notes.push(format!("Git locator index close failed: {error}"));
            }
            (
                available,
                coverage.map(|coverage| coverage.generation),
                coverage.map(|coverage| coverage.pack_index_hash.hex()),
            )
        }
        Err(error) => {
            notes.push(format!("Git locator index unavailable: {error}"));
            (false, None, None)
        }
    };
    let git_locator_writer_lease_active = match crab_coordination::internal_lock_path(
        repo_prefix,
        crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
    ) {
        Ok(path) => match storage.get_with_etag(&ObjectPath::from(path)).await {
            Ok((body, _)) => serde_json::from_slice::<crab_coordination::PushLockPayload>(&body)
                .is_ok_and(|payload| {
                    !payload.is_released() && !payload.is_expired_at(crab_coordination::unix_now())
                }),
            Err(crab_storage::StorageError::NotFound { .. }) => false,
            Err(error) => {
                notes.push(format!("Git locator writer lease unavailable: {error}"));
                false
            }
        },
        Err(error) => {
            notes.push(format!("Git locator writer lease path invalid: {error}"));
            false
        }
    };
    let expected_pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default().hex()
    } else {
        manifest.pack_index_hash.clone()
    };
    let git_locator_coverage_current = git_locator_covered_generation == Some(manifest.generation)
        && git_locator_covered_pack_index_hash.as_deref()
            == Some(expected_pack_index_hash.as_str());
    if !git_locator_index_available {
        notes.push("Git locator index missing; run `crab metadb rebuild`".to_owned());
    } else if !git_locator_coverage_current {
        notes.push("Git locator coverage is stale; run `crab metadb rebuild`".to_owned());
    }
    let repair_required = !generation_receipt_valid
        || !ref_registry_repo_complete
        || !ref_registry_bucket_complete
        || !git_locator_index_available
        || !git_locator_coverage_current
        || !git_visibility_index_available
        || !git_visibility_coverage_current;
    AccelerationHealth {
        manifest_generation: Some(manifest.generation),
        generation_receipt_valid,
        ref_registry_repo_complete,
        ref_registry_bucket_complete,
        git_locator_index_available,
        git_locator_covered_generation,
        git_locator_covered_pack_index_hash,
        git_visibility_index_available,
        git_visibility_covered_generation,
        git_visibility_covered_pack_index_hash,
        git_visibility_coverage_current,
        git_locator_writer_lease_active,
        repair_required,
        notes,
    }
}

async fn count_shards(store: &Arc<dyn ObjectStore>, prefix: &ObjectPath) -> Result<u64> {
    let mut total: u64 = 0;
    let mut stream = store.list(Some(prefix));
    while let Some(_meta) = stream
        .try_next()
        .await
        .map_err(|e| CrabError::Internal(format!("listing shards: {e}")))?
    {
        total += 1;
    }
    Ok(total)
}

fn render_doctor_metadb(payload: &DoctorMetadbPayload, mode: OutputMode) {
    if matches!(mode, OutputMode::Json) {
        emit_json("doctor.metadb", "1.0", payload);
        return;
    }

    println!("crab doctor --metadb\n");
    println!("  repo_prefix: {}", payload.repo_prefix);
    println!();
    render_db_diagnosis(&payload.file_index);
    render_db_diagnosis(&payload.chunk_index);

    println!("[shards]  prefix={}", payload.shards_prefix);
    match payload.shard_count {
        Some(n) => println!("  shard_count: {n}"),
        None => {
            let err = payload
                .shard_enumeration_error
                .as_deref()
                .unwrap_or("<unknown>");
            println!("  shard_count: <failed to enumerate> — {err}");
        }
    }
    println!();

    println!("[local cache]  path={}", payload.cache.cache_path);
    println!("  exists: {}", payload.cache.exists);
    println!("  file_size_bytes: {}", payload.cache.file_size_bytes);
    println!("  entry_count: {}", payload.cache.entry_count);
    println!(
        "  installed_shards: {}",
        payload.cache.installed_shard_count
    );
    println!(
        "  cache_gc_generation: {}",
        payload.cache.cache_gc_generation
    );
    println!();
    println!("[generation acceleration]");
    println!(
        "  manifest_generation: {}",
        payload.acceleration.manifest_generation.map_or_else(
            || "<unknown>".to_owned(),
            |generation| generation.to_string()
        )
    );
    println!(
        "  generation_receipt_valid: {}",
        payload.acceleration.generation_receipt_valid
    );
    println!(
        "  ref_registry_repo_complete: {}",
        payload.acceleration.ref_registry_repo_complete
    );
    println!(
        "  ref_registry_bucket_complete: {}",
        payload.acceleration.ref_registry_bucket_complete
    );
    println!(
        "  git_locator_index_available: {}",
        payload.acceleration.git_locator_index_available
    );
    println!(
        "  git_locator_covered_generation: {}",
        payload
            .acceleration
            .git_locator_covered_generation
            .map_or_else(|| "<none>".to_owned(), |generation| generation.to_string())
    );
    println!(
        "  git_locator_covered_pack_index_hash: {}",
        payload
            .acceleration
            .git_locator_covered_pack_index_hash
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  git_visibility_index_available: {}",
        payload.acceleration.git_visibility_index_available
    );
    println!(
        "  git_visibility_covered_generation: {}",
        payload
            .acceleration
            .git_visibility_covered_generation
            .map_or_else(|| "<none>".to_owned(), |generation| generation.to_string())
    );
    println!(
        "  git_visibility_covered_pack_index_hash: {}",
        payload
            .acceleration
            .git_visibility_covered_pack_index_hash
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "  git_visibility_coverage_current: {}",
        payload.acceleration.git_visibility_coverage_current
    );
    println!(
        "  git_locator_writer_lease_active: {}",
        payload.acceleration.git_locator_writer_lease_active
    );
    println!(
        "  repair_required: {}",
        payload.acceleration.repair_required
    );
    for note in &payload.acceleration.notes {
        println!("  note: {note}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt};
    use tempfile::TempDir;

    use super::*;
    use crate::metadata::metadb::{Db, MetaDb, MetaDbConfig, stores};
    use crab_metadata::key_codec::{self, SYS_CREATED_AT, SYS_EPOCH, SYS_FORMAT_VERSION};

    /// Build a `MetaDb` anchored at a temp cache path and an in-memory
    /// object store. Returns the store handle too so tests can seed
    /// raw sys:* values through a short-lived `Db` handle before the
    /// diagnose helper opens its own.
    fn test_metadb(store: Arc<dyn ObjectStore>) -> (MetaDb, TempDir) {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        (
            MetaDb::new(store, String::from("org/test-repo"), cfg),
            cache_dir,
        )
    }

    #[test]
    fn pack_locator_rebuild_uses_manifest_blake3_identity() {
        let bytes = b"pack bytes use raw blake3 identity, not the Xet file hash domain";
        let raw_pack_id = blake3::hash(bytes).to_hex().to_string();
        let expected = blake3::Hash::from_hex(&raw_pack_id).expect("parse raw pack hash");

        assert_eq!(blake3::hash(bytes), expected);
        assert_ne!(
            <[u8; 32]>::from(MerkleHash::from_hex(&raw_pack_id).expect("parse index pack id")),
            *expected.as_bytes(),
            "Xet MerkleHash wire order must not be used to validate raw Blake3 pack IDs"
        );
    }

    #[test]
    fn sampled_locator_crc_detects_corrupt_pack_bytes() {
        let temp = TempDir::new().expect("tempdir");
        let pack_path = temp.path().join("sample.pack");
        let mut bytes = (0u8..64).collect::<Vec<_>>();
        std::fs::write(&pack_path, &bytes).expect("write sample pack");
        let location = crab_git::pack_locator::PackObjectLocation {
            oid: gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
                .expect("test oid"),
            pack_offset: 12,
            entry_len: 32,
            crc32: crc32fast::hash(&bytes[12..44]),
        };

        verify_sampled_pack_ranges(&pack_path, std::slice::from_ref(&location))
            .expect("valid sampled range");
        bytes[20] ^= 0xff;
        std::fs::write(&pack_path, &bytes).expect("corrupt sample pack");
        assert!(matches!(
            verify_sampled_pack_ranges(&pack_path, &[location]),
            Err(CrabError::CorruptObject { .. })
        ));
    }

    #[tokio::test]
    async fn diagnose_chunk_index_surfaces_seeded_sys_values() {
        // Seed sys:format_version = 1, sys:epoch = 42, and
        // sys:created_at = 1_700_000_000_000 (approx 2023-11-14) into
        // an in-memory chunk_index_db. A diagnose pass must decode
        // each value and surface it on the returned DbDiagnosis.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        // Seed BEFORE the MetaDb-owned handle opens the same path —
        // SlateDB fences older handles on a reopen, so establishing
        // the remote state up front keeps the diagnose path
        // authoritative.
        {
            let seed_db = Db::open(
                Arc::clone(&store),
                ObjectPath::from(metadb.config().chunk_index_path.as_str()),
                stores::chunk_index::DB_LABEL,
            )
            .await
            .expect("seed open");
            let mut batch = slatedb::WriteBatch::new();
            batch.put(
                key_codec::encode_system_key(SYS_FORMAT_VERSION).as_slice(),
                1u32.to_le_bytes().as_slice(),
            );
            batch.put(
                key_codec::encode_system_key(SYS_EPOCH).as_slice(),
                42u64.to_le_bytes().as_slice(),
            );
            batch.put(
                key_codec::encode_system_key(SYS_CREATED_AT).as_slice(),
                1_700_000_000_000u64.to_le_bytes().as_slice(),
            );
            seed_db.write(batch).await.expect("seed write");
            seed_db.close().await.expect("seed close");
        }

        let guard = MetaDbGuard::new(metadb);
        let d = diagnose_chunk_index(&guard, false, &store, ".crab/chunk_index_db/").await;

        assert!(d.opened, "diagnose should have opened chunk_index_db");
        assert_eq!(d.error, None);
        assert_eq!(d.label, "chunk_index_db");
        assert_eq!(d.format_version, Some(1));
        assert_eq!(d.epoch, Some(42));
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20Z
        assert_eq!(d.created_at.as_deref(), Some("2023-11-14T22:13:20Z"));
        // gc_generation was not seeded, so the key is absent and the
        // accessor returns None (NOT a corrupt-value error).
        assert_eq!(d.gc_generation, None);

        guard.close().await.expect("guard close");
    }

    #[tokio::test]
    async fn diagnose_file_index_on_fresh_db_reports_all_none() {
        // A freshly-constructed MetaDb has no sys:* keys written to
        // either database. The diagnose helper must open cleanly and
        // report every field as None rather than surfacing an error.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        let d = diagnose_file_index(&guard, false, &store, "org/test-repo/file_index_db/").await;

        assert!(d.opened, "fresh file_index_db must open cleanly");
        assert_eq!(d.error, None);
        assert_eq!(d.label, "file_index_db");
        assert_eq!(d.format_version, None);
        assert_eq!(d.epoch, None);
        assert_eq!(d.created_at, None);
        assert_eq!(d.gc_generation, None);

        guard.close().await.expect("guard close");
    }

    // --- rebuild -------------------------------------------------------

    /// A minimal shard with one real xorb and two reconstructable files.
    fn build_test_shard(seed: u64) -> (bytes::Bytes, MerkleHash, bytes::Bytes, MerkleHash) {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;
        use std::sync::Arc;

        let chunks = [
            vec![seed as u8; 1024],
            vec![seed.wrapping_add(1) as u8; 1024],
        ];
        let mut builder = XorbBuilder::new();
        for data in &chunks {
            builder
                .push(
                    &Chunk {
                        hash: crab_xet::hash::compute_data_hash(data),
                        data: bytes::Bytes::copy_from_slice(data),
                    },
                    RunId(0),
                )
                .expect("pack test chunk");
        }
        let mut packed = builder.finalize().expect("finalize test xorb");
        assert_eq!(packed.len(), 1, "small test chunks should share one xorb");
        let packed = packed.pop().expect("one test xorb");

        let mut writer = crab_xet::shard::ShardWriter::new();
        let xorb_entries = packed
            .placements
            .iter()
            .enumerate()
            .map(|(index, placement)| {
                XorbChunkSequenceEntry::new(
                    placement.chunk_hash,
                    placement.uncompressed_size,
                    u32::try_from(index * 1024).expect("test offset fits u32"),
                )
            })
            .collect();
        writer
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(packed.hash, 2, 2 * 1024),
                chunks: xorb_entries,
            }))
            .expect("add xorb");

        for (index, data) in chunks.iter().enumerate() {
            let file_hash = crab_xet::hash::compute_data_hash(data);
            writer
                .add_file(MDBFileInfo {
                    metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
                    segments: vec![FileDataSequenceEntry::new(
                        packed.hash,
                        1024u32,
                        u32::try_from(index).expect("test chunk index fits u32"),
                        u32::try_from(index + 1).expect("test chunk end fits u32"),
                    )],
                    verification: vec![],
                    metadata_ext: None,
                })
                .expect("add file");
        }

        let (bytes, hash) = writer.finalize().expect("finalize");
        (bytes::Bytes::from(bytes), hash, packed.bytes, packed.hash)
    }

    async fn seed_committed_shard_index(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        shard_hashes: &[MerkleHash],
    ) {
        let storage = crab_storage::Store::new(store);
        let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
        let hashes: Vec<String> = shard_hashes.iter().map(MerkleHash::hex).collect();
        let (index_hash, _, write) = crab_metadata::manifests::append_shard_index(
            crab_metadata::segmented::SegmentIndex::default(),
            1,
            &hashes,
        )
        .unwrap();
        crab_metadata::manifest_store::upload_segmented_bulk(
            &storage,
            &router,
            &crab_metadata::manifests::BulkData {
                shard_index: write,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await
        .unwrap();
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = index_hash;
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(&storage, &router, &manifest)
            .await
            .unwrap();
    }

    /// Rebuild end-to-end: seed two shards under `.crab/shards/`,
    /// run the rebuild driver with `--db both`, and verify that both
    /// SlateDB instances answer the synthesised file and chunk
    /// lookups correctly.
    #[tokio::test]
    async fn rebuild_repopulates_both_databases_from_shards() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // Seed two distinct shards at their content-addressed keys.
        let (shard_a_bytes, shard_a_hash, xorb_a_bytes, xorb_a_hash) = build_test_shard(7);
        let (shard_b_bytes, shard_b_hash, xorb_b_bytes, xorb_b_hash) = build_test_shard(77);
        let shard_a_path = ObjectPath::from(format!(".crab/shards/{}", shard_a_hash.hex()));
        let shard_b_path = ObjectPath::from(format!(".crab/shards/{}", shard_b_hash.hex()));
        let xorb_a_path = ObjectPath::from(format!(".crab/xorbs/{}", xorb_a_hash.hex()));
        let xorb_b_path = ObjectPath::from(format!(".crab/xorbs/{}", xorb_b_hash.hex()));
        store
            .put(&shard_a_path, shard_a_bytes.clone().into())
            .await
            .expect("put shard a");
        store
            .put(&shard_b_path, shard_b_bytes.clone().into())
            .await
            .expect("put shard b");
        store
            .put(&xorb_a_path, xorb_a_bytes.into())
            .await
            .expect("put xorb a");
        store
            .put(&xorb_b_path, xorb_b_bytes.into())
            .await
            .expect("put xorb b");

        seed_committed_shard_index(
            Arc::clone(&store),
            "org/test-repo",
            &[shard_a_hash, shard_b_hash],
        )
        .await;

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        let orphan_legacy_file = MerkleHash::from([901, 902, 903, 904]);
        let orphan_legacy_chunk = MerkleHash::from([905, 906, 907, 908]);
        let mut legacy_txn = guard.new_transaction().expect("transaction");
        guard
            .file_index()
            .await
            .expect("legacy file store")
            .save_legacy(
                &mut legacy_txn,
                &orphan_legacy_file,
                &MerkleHash::from([909, 910, 911, 912]),
            );
        legacy_txn.put(
            crate::metadata::metadb::transaction::DbTarget::ChunkIndex,
            bytes::Bytes::copy_from_slice(&crab_metadata::key_codec::encode_content_key(
                &orphan_legacy_chunk,
            )),
            bytes::Bytes::copy_from_slice(&crab_metadata::value_codec::encode_chunk_index_value(
                &XorbRef {
                    xorb_hash: MerkleHash::from([913, 914, 915, 916]),
                    chunk_index: 0,
                    uncompressed_size: 1024,
                },
            )),
        );
        guard.commit(legacy_txn).await.expect("seed legacy rows");

        let payload = rebuild_with_guard(&store, "org/test-repo", DbSelector::Both, true, &guard)
            .await
            .expect("rebuild");
        assert_eq!(payload.legacy_file_rows_removed, 1);
        assert_eq!(payload.legacy_chunk_rows_removed, 1);

        // Expected state: each shard contributes 2 chunks (one xorb
        // with 2 chunks) and 2 file entries.
        let file_index = guard.file_index().await.expect("file index");
        let chunk_index = guard.chunk_index().await.expect("chunk index");
        assert!(
            file_index
                .get_legacy(&orphan_legacy_file)
                .await
                .expect("legacy lookup")
                .is_none()
        );
        assert!(
            chunk_index
                .legacy_keys_batch(REBUILD_COMMIT_BATCH)
                .await
                .expect("legacy chunk scan")
                .is_empty()
        );

        // Pull every (file_hash, shard_hash) pair back via the
        // streaming extractor and verify it round-trips through
        // file_index_db.
        let expected_files = {
            let mut v =
                crab_xet::shard_parse::extract_file_entries_streaming(&shard_a_bytes, shard_a_hash);
            v.extend(crab_xet::shard_parse::extract_file_entries_streaming(
                &shard_b_bytes,
                shard_b_hash,
            ));
            v
        };
        assert_eq!(
            expected_files.len(),
            4,
            "2 shards with 2 files each must yield 4 entries"
        );
        for (file_hash, shard_hash) in &expected_files {
            let got = file_index
                .get_committed_batch(&[*file_hash])
                .await
                .expect("file_index get")
                .into_iter()
                .next()
                .flatten()
                .expect("file entry present after rebuild");
            assert_eq!(got.shard_hash, *shard_hash, "file→shard pair round-trips");
        }

        let expected_chunks = {
            let mut v = crab_xet::shard_parse::extract_chunk_entries_streaming(&shard_a_bytes);
            v.extend(crab_xet::shard_parse::extract_chunk_entries_streaming(
                &shard_b_bytes,
            ));
            v
        };
        assert_eq!(
            expected_chunks.len(),
            4,
            "2 shards with 2 chunks each must yield 4 chunk entries"
        );
        for (chunk_hash, expected_ref) in &expected_chunks {
            let got = chunk_index
                .get(chunk_hash)
                .await
                .expect("chunk_index get")
                .expect("chunk entry present after rebuild");
            assert_eq!(got, *expected_ref, "chunk→xorb ref round-trips");
        }

        guard.close().await.expect("close");
    }

    /// Re-running `rebuild` over the same bucket must converge:
    /// content-addressed writes make the second pass a no-op in
    /// terms of final state.
    #[tokio::test]
    async fn rebuild_is_idempotent_across_reruns() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (shard_bytes, shard_hash, xorb_bytes, xorb_hash) = build_test_shard(5);
        let shard_path = ObjectPath::from(format!(".crab/shards/{}", shard_hash.hex()));
        let xorb_path = ObjectPath::from(format!(".crab/xorbs/{}", xorb_hash.hex()));
        store
            .put(&shard_path, shard_bytes.clone().into())
            .await
            .expect("put");
        store
            .put(&xorb_path, xorb_bytes.into())
            .await
            .expect("put xorb");
        seed_committed_shard_index(Arc::clone(&store), "org/test-repo", &[shard_hash]).await;

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        let guard = MetaDbGuard::new(metadb);

        for _ in 0..2 {
            rebuild_with_guard(&store, "org/test-repo", DbSelector::Both, true, &guard)
                .await
                .expect("rebuild");
        }

        // After two passes, every file and chunk key must still
        // resolve to the same value.
        let file_index = guard.file_index().await.expect("file index");
        let chunk_index = guard.chunk_index().await.expect("chunk index");

        for (f, s) in
            crab_xet::shard_parse::extract_file_entries_streaming(&shard_bytes, shard_hash)
        {
            assert_eq!(
                file_index
                    .get_committed_batch(&[f])
                    .await
                    .expect("get")
                    .into_iter()
                    .next()
                    .flatten()
                    .expect("present")
                    .shard_hash,
                s,
                "file entry still correct after second rebuild pass"
            );
        }
        for (c, r) in crab_xet::shard_parse::extract_chunk_entries_streaming(&shard_bytes) {
            assert_eq!(
                chunk_index.get(&c).await.expect("get").expect("present"),
                r,
                "chunk entry still correct after second rebuild pass"
            );
        }

        guard.close().await.expect("close");
    }
}
