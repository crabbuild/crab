//! Configuration types for the crab system.
//!
//! [`Config`] is the top-level resolved configuration struct that carries
//! every setting a subsystem might need. It is produced by the config
//! resolution pipeline (layers 1–5) and threaded through [`super::AppContext`].
//!
//! Resolution is split into two phases to solve the bootstrap chicken-and-egg:
//! - [`Config::resolve_local()`] merges layers 1–3 + env allowlist — enough
//!   to construct a `Store` (credentials come from env/AWS SDK).
//! - [`Config::resolve_remote()`] merges layer 4 (remote JSON) onto the
//!   local config to produce the final resolved config.
//!
//! Offline commands (`version`, `errors`, `cache`) skip the remote phase.
//!
//! [`EngineConfig`] covers the `[perf]` section — all engine optimizations
//! are gated behind its flags and default to on. Setting `enabled = false`
//! disables every optimization regardless of individual field values,
//! falling back to v1 behavior.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crab_types::{replication::ReplicationConfig, storage::StorageProviderKind};

pub use crab_auth::AuthProviderKind as AuthProvider;

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// Compression algorithm and level for xorb storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionConfig {
    /// No compression — store raw bytes.
    None,
    /// Zstandard compression at the given level (typically 1–19).
    Zstd { level: i32 },
    /// LZ4 fast compression — lower ratio than zstd but significantly faster.
    Lz4,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self::Zstd { level: 3 }
    }
}

impl fmt::Display for CompressionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Zstd { level } => write!(f, "zstd({level})"),
            Self::Lz4 => f.write_str("lz4"),
        }
    }
}

// ---------------------------------------------------------------------------
// Checkout and hydration config
// ---------------------------------------------------------------------------

/// Checkout behavior settings.
#[derive(Debug, Clone, Default)]
pub struct CheckoutConfig {
    /// When true, smudge returns pointer blobs unchanged.
    pub lazy: bool,
}

/// Default hydrate download concurrency. Higher than the top-level
/// `download_concurrency` (which services push) because hydrate benefits
/// from more parallel xorb fetches when reconstructing multi-term files.
const DEFAULT_HYDRATE_DOWNLOAD_CONCURRENCY: usize = 4;

/// Default hydration memory budget: 384 MiB. Four active 64 MiB terms leave
/// two terms of read-ahead while xet-core's background writer drains output.
const DEFAULT_HYDRATE_PREFETCH_BUDGET: u64 = 384 * 1024 * 1024;

/// Default speculative hydration concurrency.
const DEFAULT_SPECULATIVE_CONCURRENCY: usize = 2;

/// Hydration pattern settings.
#[derive(Debug, Clone)]
pub struct HydrateConfig {
    /// Glob patterns for files to auto-hydrate on checkout.
    pub include: Vec<String>,
    /// Glob patterns to exclude from auto-hydration.
    pub exclude: Vec<String>,
    /// When true, smudge selectively hydrates files matching
    /// include/exclude patterns even in lazy mode.
    pub auto: bool,
    /// Maximum concurrent xorb downloads during hydration. Serves as the
    /// max bound for the shared `AdaptiveConcurrencyController` that
    /// throttles `StoreClient` and the prefetch queue.
    pub download_concurrency: usize,
    /// Memory budget for concurrent hydration and filter-process prefetch.
    /// Total in-flight reconstructed bytes are bounded by a semaphore
    /// derived from this value. Default: 384 MiB.
    pub prefetch_budget: u64,
    /// When true, archived xorbs are automatically restored during hydrate.
    /// When false, hydrate fails immediately with `ArchiveRestoreRequired`.
    pub auto_restore: bool,
    /// When true (default), `crab clone` auto-hydrates the `always`
    /// prefetch profile from `.crab/prefetch.toml` after the working
    /// tree is set up. Set to `false` to skip post-clone prefetch.
    pub auto_prefetch: bool,
    /// When true, the filter driver records co-access events and
    /// speculatively pre-fetches predicted neighbors on smudge.
    /// Conservative default (off) — speculation can surprise users in
    /// bandwidth-metered environments.
    pub speculative: bool,
    /// Maximum concurrent speculative hydrations. Capped by a tokio
    /// semaphore so speculative work never starves foreground requests.
    pub speculative_concurrency: usize,
}

impl Default for HydrateConfig {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            auto: false,
            download_concurrency: DEFAULT_HYDRATE_DOWNLOAD_CONCURRENCY,
            prefetch_budget: DEFAULT_HYDRATE_PREFETCH_BUDGET,
            auto_restore: true,
            auto_prefetch: true,
            speculative: false,
            speculative_concurrency: DEFAULT_SPECULATIVE_CONCURRENCY,
        }
    }
}

// ---------------------------------------------------------------------------
// Tier configuration
// ---------------------------------------------------------------------------

/// Default: tiering subsystem is opt-in.
const DEFAULT_TIER_ENABLED: bool = false;

/// Default days before transition to IA/warm-cold class.
const DEFAULT_TIER_TO_IA_DAYS: u32 = 30;

/// Default days before transition to deep-cold class.
const DEFAULT_TIER_TO_DEEP_DAYS: u32 = 180;

/// Default noncurrent version expiration days.
const DEFAULT_TIER_NONCURRENT_DAYS: u32 = 30;

/// Default restore tier for archived objects.
const DEFAULT_TIER_RESTORE_TIER: &str = "standard";

/// Default restore duration in days.
const DEFAULT_TIER_RESTORE_DURATION_DAYS: u32 = 7;

/// Default maximum concurrent restore requests.
const DEFAULT_TIER_RESTORE_MAX_CONCURRENCY: u32 = 16;

/// Default restore timeout: 6 hours.
const DEFAULT_TIER_RESTORE_TIMEOUT_SECS: u64 = 21600;

/// Default output storage class for restripe destinations.
const DEFAULT_TIER_RESTRIPE_OUTPUT_CLASS: &str = "standard";

/// Lifecycle tiering configuration from the `[tier]` TOML section.
///
/// The entire subsystem is opt-in (`enabled = false` by default). When
/// disabled, `crab tier` commands are inert and hydrate never issues
/// restore requests.
#[derive(Debug, Clone)]
pub struct TierConfig {
    /// Master switch for the tiering subsystem.
    pub enabled: bool,
    /// Days before transition to IA / warm-cold class.
    pub to_ia_days: u32,
    /// Days before transition to deep-cold class.
    pub to_deep_days: u32,
    /// Noncurrent version expiration days.
    pub noncurrent_days: u32,
    /// Restore tier: `expedited`, `standard`, or `bulk` (S3);
    /// `high` or `standard` (Azure).
    pub restore_tier: String,
    /// Restore duration in days.
    pub restore_duration_days: u32,
    /// Maximum concurrent restore requests.
    pub restore_max_concurrency: u32,
    /// Restore timeout in seconds.
    pub restore_timeout_secs: u64,
    /// Output storage class for restripe destinations.
    pub restripe_output_class: String,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_TIER_ENABLED,
            to_ia_days: DEFAULT_TIER_TO_IA_DAYS,
            to_deep_days: DEFAULT_TIER_TO_DEEP_DAYS,
            noncurrent_days: DEFAULT_TIER_NONCURRENT_DAYS,
            restore_tier: DEFAULT_TIER_RESTORE_TIER.to_string(),
            restore_duration_days: DEFAULT_TIER_RESTORE_DURATION_DAYS,
            restore_max_concurrency: DEFAULT_TIER_RESTORE_MAX_CONCURRENCY,
            restore_timeout_secs: DEFAULT_TIER_RESTORE_TIMEOUT_SECS,
            restripe_output_class: DEFAULT_TIER_RESTRIPE_OUTPUT_CLASS.to_string(),
        }
    }
}

impl TierConfig {
    /// Apply `CRAB_TIER_*` environment-variable overrides in place.
    ///
    /// Malformed values are logged and skipped.
    pub fn apply_env_overrides(&mut self) {
        if let Some(v) = env_bool("CRAB_TIER_ENABLED") {
            self.enabled = v;
        }
        if let Some(v) = env_parse::<u32>("CRAB_TIER_TO_IA_DAYS") {
            self.to_ia_days = v;
        }
        if let Some(v) = env_parse::<u32>("CRAB_TIER_TO_DEEP_DAYS") {
            self.to_deep_days = v;
        }
        if let Some(v) = env_parse::<u32>("CRAB_TIER_NONCURRENT_DAYS") {
            self.noncurrent_days = v;
        }
        if let Ok(raw) = std::env::var("CRAB_TIER_RESTORE_TIER") {
            let trimmed = raw.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                self.restore_tier = trimmed;
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_TIER_RESTORE_DURATION_DAYS") {
            self.restore_duration_days = v;
        }
        if let Some(v) = env_parse::<u32>("CRAB_TIER_RESTORE_MAX_CONCURRENCY") {
            self.restore_max_concurrency = v;
        }
        if let Some(v) = env_parse::<u64>("CRAB_TIER_RESTORE_TIMEOUT_SECS") {
            self.restore_timeout_secs = v;
        }
        if let Ok(raw) = std::env::var("CRAB_TIER_RESTRIPE_OUTPUT_CLASS") {
            let trimmed = raw.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                self.restripe_output_class = trimmed;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cost configuration
// ---------------------------------------------------------------------------

/// Default inventory source: auto-select.
const DEFAULT_COST_INVENTORY_SOURCE: &str = "auto";

/// Default list concurrency for live inventory walks.
const DEFAULT_COST_LIST_CONCURRENCY: u32 = 32;

/// Default sample ratio: 1.0 (no sampling).
const DEFAULT_COST_SAMPLE_RATIO: f64 = 1.0;

/// Default access window for cost analysis.
const DEFAULT_COST_ACCESS_WINDOW_DAYS: u32 = 90;

/// Default: do not apply free-tier modeling.
const DEFAULT_COST_APPLY_FREE_TIER: bool = false;

/// Default maximum staleness for inventory reports.
const DEFAULT_COST_REPORT_MAX_STALENESS_HOURS: u32 = 48;

/// Cost optimizer configuration from the `[cost]` TOML section.
#[derive(Debug, Clone)]
pub struct CostConfig {
    /// Inventory source: `auto`, `live`, or `report`.
    pub inventory_source: String,
    /// Maximum concurrent LIST requests for live inventory walks.
    pub list_concurrency: u32,
    /// Sample ratio for inventory (1.0 = no sampling).
    pub sample_ratio: f64,
    /// Optional path to a pricing override YAML file.
    pub pricing_file: String,
    /// Override for the embedded price table version.
    pub price_table_version: String,
    /// Access window in days for cost analysis.
    pub access_window_days: u32,
    /// Whether to apply free-tier modeling.
    pub apply_free_tier: bool,
    /// Maximum staleness in hours for inventory reports.
    pub report_max_staleness_hours: u32,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            inventory_source: DEFAULT_COST_INVENTORY_SOURCE.to_string(),
            list_concurrency: DEFAULT_COST_LIST_CONCURRENCY,
            sample_ratio: DEFAULT_COST_SAMPLE_RATIO,
            pricing_file: String::new(),
            price_table_version: String::new(),
            access_window_days: DEFAULT_COST_ACCESS_WINDOW_DAYS,
            apply_free_tier: DEFAULT_COST_APPLY_FREE_TIER,
            report_max_staleness_hours: DEFAULT_COST_REPORT_MAX_STALENESS_HOURS,
        }
    }
}

impl CostConfig {
    /// Apply `CRAB_COST_*` environment-variable overrides in place.
    ///
    /// Malformed values are logged and skipped.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(raw) = std::env::var("CRAB_COST_INVENTORY_SOURCE") {
            let trimmed = raw.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                self.inventory_source = trimmed;
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_COST_LIST_CONCURRENCY") {
            self.list_concurrency = v;
        }
        if let Some(v) = env_parse::<f64>("CRAB_COST_SAMPLE_RATIO") {
            self.sample_ratio = v;
        }
        if let Ok(raw) = std::env::var("CRAB_COST_PRICING_FILE") {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                self.pricing_file = trimmed;
            }
        }
        if let Ok(raw) = std::env::var("CRAB_COST_PRICE_TABLE_VERSION") {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                self.price_table_version = trimmed;
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_COST_ACCESS_WINDOW_DAYS") {
            self.access_window_days = v;
        }
        if let Some(v) = env_bool("CRAB_COST_APPLY_FREE_TIER") {
            self.apply_free_tier = v;
        }
        if let Some(v) = env_parse::<u32>("CRAB_COST_REPORT_MAX_STALENESS_HOURS") {
            self.report_max_staleness_hours = v;
        }
    }
}

// ---------------------------------------------------------------------------
// Restripe configuration
// ---------------------------------------------------------------------------

/// A single restripe profile override from `[restripe.profiles.<name>]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileOverride {
    /// Target xorb size in bytes.
    pub target_xorb_bytes: Option<u64>,
    /// Maximum xorbs per file.
    pub max_xorbs_per_file: Option<u32>,
    /// Grouping strategy: `file`, `directory`, or `hash`.
    pub group_by: Option<String>,
    /// Compression config string (e.g. `"zstd:3"`, `"lz4"`, `"none"`).
    pub compression: Option<String>,
}

/// Restripe configuration from the `[restripe]` TOML section.
#[derive(Debug, Clone, Default)]
pub struct RestripeConfig {
    /// Named profile overrides keyed by profile name.
    pub profiles: std::collections::HashMap<String, ProfileOverride>,
}

// ---------------------------------------------------------------------------
// GC configuration
// ---------------------------------------------------------------------------

/// Default: class-aware GC is opt-in for the first release.
const DEFAULT_GC_CLASS_AWARE: bool = false;

/// GC configuration from the `[gc]` TOML section.
///
/// The existing GC settings (`gc_grace_period`, `gc_delete_concurrency`,
/// `gc_list_concurrency`) remain as flat fields on [`Config`] for backward
/// compatibility. This struct carries the new class-aware flag.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// When true, GC checks storage class and refuses early-delete for
    /// objects within their minimum retention window.
    pub class_aware: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            class_aware: DEFAULT_GC_CLASS_AWARE,
        }
    }
}

impl GcConfig {
    /// Apply `CRAB_GC_*` environment-variable overrides in place.
    pub fn apply_env_overrides(&mut self) {
        if let Some(v) = env_bool("CRAB_GC_CLASS_AWARE") {
            self.class_aware = v;
        }
    }
}

// ---------------------------------------------------------------------------
// MetaDB configuration (TOML / env surface)
// ---------------------------------------------------------------------------

/// `[metadb]` TOML section — knobs for the two SlateDB metadata
/// databases plus the local chunk-index cache.
///
/// Every field is optional; an absent field keeps the value that
/// [`crate::metadata::MetaDbConfig::for_repo`] would supply at session
/// construction time. The resolver merges this section onto the
/// derived defaults inside
/// [`Config::build_metadb_config`], so callers never have to thread
/// raw overrides through the push / hydrate pipelines themselves.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaDbTomlConfig {
    /// `[metadb.file_index]` — tunables for the per-repo
    /// `file_index_db`.
    #[serde(default)]
    pub file_index: MetaDbSubTomlConfig,

    /// `[metadb.chunk_index]` — tunables for the globally shared
    /// `chunk_index_db` plus the local on-disk cache.
    #[serde(default)]
    pub chunk_index: MetaDbChunkIndexTomlConfig,
}

/// Per-database SlateDB tunables shared by `file_index` and
/// `chunk_index`.
///
/// Present on both `[metadb.file_index]` and — via
/// [`MetaDbChunkIndexTomlConfig`]'s flattened inheritance — on
/// `[metadb.chunk_index]`. Fields left unset fall through to the
/// compiled-in defaults in [`crate::metadata::MetaDbConfig::default`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaDbSubTomlConfig {
    /// Override for the object-store path. Leave unset to use the
    /// derived default (`{repo_prefix}/file_index_db/` for file_index,
    /// `.crab/chunk_index_db/` for chunk_index).
    pub path: Option<String>,

    /// Compaction threshold: SSTables at level 0 before a compaction
    /// runs.
    pub compaction_threshold: Option<u32>,

    /// WAL flush size per instance, in bytes.
    pub wal_flush_size: Option<u64>,

    /// Bloom-filter bits per key per SSTable.
    pub bloom_bits_per_key: Option<u32>,
}

/// `[metadb.chunk_index]` tunables. Inherits the per-database fields
/// via `#[serde(flatten)]` and adds the local chunk-index cache
/// knobs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaDbChunkIndexTomlConfig {
    /// Per-database SlateDB tunables (path + compaction / WAL /
    /// bloom).
    #[serde(flatten)]
    pub db: MetaDbSubTomlConfig,

    /// Path to the local `PersistentChunkIndex` SQLite cache. Leave unset
    /// to derive from `~/.cache/crab/buckets/{bucket-hash}/chunk-index.sqlite`.
    pub local_path: Option<PathBuf>,

    /// Memory ceiling for the in-memory chunk-index hot tier (bytes).
    pub in_memory_ceiling_bytes: Option<u64>,

    /// Grace window (in GC generations) before the local cache is
    /// wiped to stay consistent with the remote.
    pub cache_gc_grace: Option<u64>,
}

impl MetaDbTomlConfig {
    /// Apply `CRAB_METADB_*` environment-variable overrides in
    /// place. Malformed values are logged and skipped, matching the
    /// existing [`TierConfig::apply_env_overrides`] pattern.
    ///
    /// Recognized variables:
    /// - `CRAB_METADB_FILE_INDEX_PATH` — object-store path
    /// - `CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD` — u32
    /// - `CRAB_METADB_FILE_INDEX_WAL_FLUSH_SIZE` — u64 bytes
    /// - `CRAB_METADB_FILE_INDEX_BLOOM_BITS_PER_KEY` — u32
    /// - `CRAB_METADB_CHUNK_INDEX_PATH` — object-store path
    /// - `CRAB_METADB_CHUNK_INDEX_COMPACTION_THRESHOLD` — u32
    /// - `CRAB_METADB_CHUNK_INDEX_WAL_FLUSH_SIZE` — u64 bytes
    /// - `CRAB_METADB_CHUNK_INDEX_BLOOM_BITS_PER_KEY` — u32
    /// - `CRAB_METADB_CHUNK_INDEX_LOCAL_PATH` — filesystem path
    /// - `CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES` — u64
    /// - `CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE` — u64
    pub fn apply_env_overrides(&mut self) {
        // file_index per-DB tunables
        if let Ok(raw) = std::env::var("CRAB_METADB_FILE_INDEX_PATH") {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                self.file_index.path = Some(trimmed);
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD") {
            self.file_index.compaction_threshold = Some(v);
        }
        if let Some(v) = env_parse::<u64>("CRAB_METADB_FILE_INDEX_WAL_FLUSH_SIZE") {
            self.file_index.wal_flush_size = Some(v);
        }
        if let Some(v) = env_parse::<u32>("CRAB_METADB_FILE_INDEX_BLOOM_BITS_PER_KEY") {
            self.file_index.bloom_bits_per_key = Some(v);
        }

        // chunk_index per-DB tunables
        if let Ok(raw) = std::env::var("CRAB_METADB_CHUNK_INDEX_PATH") {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                self.chunk_index.db.path = Some(trimmed);
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_METADB_CHUNK_INDEX_COMPACTION_THRESHOLD") {
            self.chunk_index.db.compaction_threshold = Some(v);
        }
        if let Some(v) = env_parse::<u64>("CRAB_METADB_CHUNK_INDEX_WAL_FLUSH_SIZE") {
            self.chunk_index.db.wal_flush_size = Some(v);
        }
        if let Some(v) = env_parse::<u32>("CRAB_METADB_CHUNK_INDEX_BLOOM_BITS_PER_KEY") {
            self.chunk_index.db.bloom_bits_per_key = Some(v);
        }

        // chunk_index-only knobs
        if let Ok(raw) = std::env::var("CRAB_METADB_CHUNK_INDEX_LOCAL_PATH") {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                self.chunk_index.local_path = Some(PathBuf::from(trimmed));
            }
        }
        if let Some(v) = env_parse::<u64>("CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES") {
            self.chunk_index.in_memory_ceiling_bytes = Some(v);
        }
        if let Some(v) = env_parse::<u64>("CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE") {
            self.chunk_index.cache_gc_grace = Some(v);
        }
    }
}

// ---------------------------------------------------------------------------
// Cache service client config
// ---------------------------------------------------------------------------

/// Cache-service client auth and mode contracts.
///
/// The owning Interface lives in `crab-cache`; these names remain here so the
/// CLI config parser and existing callers can keep using
/// `crate::core::config::{ServiceAuth, ServiceMode}` while cache code migrates.
pub use crab_cache::{CacheServiceAuth as ServiceAuth, CacheServiceMode as ServiceMode};

pub(crate) const CACHE_SERVICE_URL_ENV: &str = "CRAB_CACHE_SERVICE_URL";

/// Client-side cache service configuration from the `[cache]` TOML section.
///
/// The `chunk_cache_bytes` size limit lives at the top-level config
/// ([`Config::chunk_cache_bytes`]) — that field is reused by the
/// xet-core `DiskCache` integration so users who already set it keep a
/// single ceiling across both the legacy [`crate::cache::ChunkCache`]
/// and the xet-core cache. Only the directory is a new per-section key.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Cache service URL (e.g., `"https://crab-cache.internal:8443"`).
    pub service_url: Option<String>,
    /// Service mode: cache, dedup, or both.
    pub service_mode: ServiceMode,
    /// Warm cache on push.
    pub push_warming: bool,
    /// Directory for xet-core's xorb-range `DiskCache`. `None` resolves
    /// at runtime to `{default_cache_root}/chunks/`. Setting this to a
    /// custom path lets operators pin the cache to a dedicated volume.
    pub chunk_cache_dir: Option<PathBuf>,
    /// Authentication mode for the cache service.
    pub service_auth: ServiceAuth,
    /// Path to a PEM CA bundle for connecting to cache services using
    /// private CAs.
    pub service_ca_cert: Option<PathBuf>,
    /// Path to the PEM client certificate chain for native mTLS.
    pub service_client_cert: Option<PathBuf>,
    /// Path to the PEM private key for native mTLS.
    pub service_client_key: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            service_url: None,
            service_mode: ServiceMode::CacheAndDedup,
            push_warming: true,
            chunk_cache_dir: None,
            service_auth: ServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        }
    }
}

impl From<&CacheConfig> for crab_cache_store::CacheConfig {
    fn from(config: &CacheConfig) -> Self {
        Self {
            service_url: config.service_url.clone(),
            service_mode: config.service_mode,
            push_warming: config.push_warming,
            service_auth: config.service_auth.clone(),
            service_ca_cert: config.service_ca_cert.clone(),
            service_client_cert: config.service_client_cert.clone(),
            service_client_key: config.service_client_key.clone(),
        }
    }
}

impl CacheConfig {
    /// Apply `CRAB_CACHE_*` environment-variable overrides in place.
    ///
    /// `CRAB_CACHE_SERVICE_URL` takes priority over `cache.service_url`.
    /// `CRAB_CACHE_PSK` and `CRAB_CACHE_TOKEN` take priority over any
    /// TOML-configured auth. If both auth env vars are set, PSK wins.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(url) = std::env::var(CACHE_SERVICE_URL_ENV) {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                self.service_url = Some(trimmed.to_owned());
            }
        }

        if let Ok(psk) = std::env::var("CRAB_CACHE_PSK")
            && !psk.trim().is_empty()
        {
            self.service_auth = ServiceAuth::Psk(psk);
            return;
        }
        if let Ok(token) = std::env::var("CRAB_CACHE_TOKEN")
            && !token.trim().is_empty()
        {
            self.service_auth = ServiceAuth::Bearer(token);
        }
    }
}

/// Cloud storage backend type.
///
/// Determines which `ObjectStore` builder to use. When the auth provider is
/// a cloud-specific OIDC variant (e.g., `aws-oidc`), the storage provider
/// is inferred automatically and this value is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum StorageProvider {
    /// Amazon S3 (and S3-compatible stores like MinIO, R2).
    #[serde(rename = "s3")]
    S3,
    /// Google Cloud Storage.
    #[serde(rename = "gcs")]
    Gcs,
    /// Azure Blob Storage.
    #[serde(rename = "azure")]
    Azure,
    /// Auto-detect from `CRAB_STORAGE_PROVIDER` env var, default to S3.
    #[default]
    #[serde(rename = "auto")]
    Auto,
}

impl StorageProvider {
    /// Parse a persisted or CLI-facing storage provider value.
    #[must_use]
    pub fn parse_config_value(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        StorageProviderKind::parse_cloud_alias(trimmed).and_then(Self::from_storage_provider_kind)
    }

    /// Return the value written to Crab config files.
    #[must_use]
    pub fn toml_value(&self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
            Self::Auto => "auto",
        }
    }

    /// Return the human-facing label used by CLI prompts.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::S3 => "s3 (Amazon S3 or S3-compatible)",
            Self::Gcs => "gcs (Google Cloud Storage)",
            Self::Azure => "azure (Azure Blob Storage)",
            Self::Auto => "auto (CRAB_STORAGE_PROVIDER or s3)",
        }
    }

    /// Return the credential-discovery URL scheme for providers that use one.
    #[must_use]
    pub fn credential_discovery_scheme(&self) -> Option<&'static str> {
        match self {
            Self::Gcs => Some("gs"),
            Self::Azure => Some("az"),
            Self::S3 | Self::Auto => None,
        }
    }

    /// Return the concrete cloud provider represented by this config value.
    #[must_use]
    pub fn storage_provider_kind(&self) -> Option<StorageProviderKind> {
        match self {
            Self::S3 => Some(StorageProviderKind::S3),
            Self::Gcs => Some(StorageProviderKind::Gcs),
            Self::Azure => Some(StorageProviderKind::Azure),
            Self::Auto => None,
        }
    }

    /// Return the config value for a concrete cloud storage provider.
    #[must_use]
    pub fn from_storage_provider_kind(provider: StorageProviderKind) -> Option<Self> {
        match provider {
            StorageProviderKind::S3 => Some(Self::S3),
            StorageProviderKind::Gcs => Some(Self::Gcs),
            StorageProviderKind::Azure => Some(Self::Azure),
            StorageProviderKind::Local => None,
        }
    }
}

/// AWS-specific auth settings from `[auth.aws]`.
#[derive(Debug, Clone, Deserialize)]
pub struct AwsAuthConfig {
    /// IAM role ARN to assume via `AssumeRoleWithWebIdentity`.
    pub role_arn: Option<String>,
    /// STS endpoint region (default: `AWS_REGION` or `us-east-1`).
    pub region: Option<String>,
    /// Session duration in seconds (default 3600, clamped 900–43200).
    #[serde(default = "default_aws_session_duration")]
    pub session_duration_secs: u64,
}

fn default_aws_session_duration() -> u64 {
    3600
}

impl Default for AwsAuthConfig {
    fn default() -> Self {
        Self {
            role_arn: None,
            region: None,
            session_duration_secs: default_aws_session_duration(),
        }
    }
}

impl AwsAuthConfig {
    /// Clamp `session_duration_secs` to the valid STS range (900–43200).
    fn clamp_session_duration(&mut self) {
        self.session_duration_secs = self.session_duration_secs.clamp(900, 43200);
    }
}

/// GCP-specific auth settings from `[auth.gcp]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GcpAuthConfig {
    /// Full resource name of the Workload Identity Pool provider.
    pub workload_identity_pool: Option<String>,
    /// Service account email to impersonate.
    pub service_account: Option<String>,
    /// GCP project ID.
    pub project_id: Option<String>,
}

/// Azure-specific auth settings from `[auth.azure]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AzureAuthConfig {
    /// Entra ID tenant ID.
    pub tenant_id: Option<String>,
    /// Azure subscription ID (optional, for SAS scoping).
    pub subscription_id: Option<String>,
    /// Storage account name (optional, for SAS scoping).
    pub storage_account: Option<String>,
}

/// Top-level auth configuration from the `[auth]` section.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Provider type.
    pub provider: AuthProvider,
    /// Cloud storage backend (S3, GCS, Azure, or auto-detect).
    /// Ignored when provider is a cloud-specific OIDC variant.
    pub storage_provider: StorageProvider,
    /// OIDC issuer URL (corporate IdP).
    pub issuer_url: Option<String>,
    /// OAuth 2.0 client ID for the crab CLI app.
    pub client_id: Option<String>,
    /// Crab Auth endpoint URL.
    pub auth_endpoint: Option<String>,
    /// Additional OAuth 2.0 scopes (default: `"openid email profile"`).
    pub scopes: String,
    /// Token cache directory path.
    pub token_cache_path: String,
    /// AWS-specific settings.
    pub aws: AwsAuthConfig,
    /// GCP-specific settings.
    pub gcp: GcpAuthConfig,
    /// Azure-specific settings.
    pub azure: AzureAuthConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            provider: AuthProvider::Static,
            storage_provider: StorageProvider::Auto,
            issuer_url: None,
            client_id: None,
            auth_endpoint: None,
            scopes: "openid email profile".into(),
            token_cache_path: "~/.config/crab/tokens/".into(),
            aws: AwsAuthConfig::default(),
            gcp: GcpAuthConfig::default(),
            azure: AzureAuthConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level resolved config
// ---------------------------------------------------------------------------

/// Default chunk threshold: 64 KiB.
const DEFAULT_CHUNK_THRESHOLD_BYTES: u64 = 64 * 1024;

/// Default upload concurrency.
const DEFAULT_UPLOAD_CONCURRENCY: usize = 8;

/// Default download concurrency.
const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 8;

/// Default operation timeout: 5 minutes.
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Default max retries for transient failures.
const DEFAULT_MAX_RETRIES: u32 = 5;

/// Default chunk cache size: 256 MiB.
const DEFAULT_CHUNK_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Default maximum size of each pack generated on push: 2 GiB.
///
/// Mirrors git's `receive.maxInputSize` default, which also defends
/// against storage-exhaustion and index-pack timeout attacks from
/// hostile or broken clients. Aggregate closures are split into
/// bounded packs; a value of `0` disables the per-pack bound.
const DEFAULT_RECEIVE_MAX_INPUT_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Default upper bound on commits walked by the commit-graph-summary
/// ancestry fallback used when `git merge-base --is-ancestor` can't
/// answer (shallow clients, missing local objects). Caps worst-case
/// cost of the summary walk and matches the commit-graph-summary's
/// default compaction window so the walk stays within the cached
/// history. `0` disables the fallback and conservatively rejects the
/// push as non-fast-forward.
const DEFAULT_RECEIVE_FF_SUMMARY_WINDOW_COMMITS: u64 = 1000;

/// Default upload-pack egress cap: 10 GiB. Sized so normal clones
/// and fetches stay well under the ceiling while still clipping a
/// runaway client that tries to drain unlimited transfer cost.
/// Setting the config value to `0` disables the check.
const DEFAULT_UPLOADPACK_MAX_EGRESS_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Default GC grace period: 24 hours.
const DEFAULT_GC_GRACE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Default GC delete concurrency.
const DEFAULT_GC_DELETE_CONCURRENCY: usize = 64;

/// Default GC list concurrency (prefix-sharded parallel LIST).
const DEFAULT_GC_LIST_CONCURRENCY: usize = 32;

/// Default pack count threshold for repack auto-warning.
const DEFAULT_REPACK_AUTO_THRESHOLD: usize = 50;

/// Default lock heartbeat interval in seconds.
const DEFAULT_PUSH_LOCK_HEARTBEAT_INTERVAL: u64 = 100;

/// Default push lock TTL: 5 minutes.
const DEFAULT_PUSH_LOCK_TTL_SECS: u64 = 300;

/// Default push lock wait: fail fast on contention.
const DEFAULT_PUSH_LOCK_WAIT_SECS: u64 = 0;

/// Default maximum manifest CAS retries for push finalization.
const DEFAULT_PUSH_MAX_CAS_RETRIES: u32 = 64;

/// Default concurrency for HEAD-check resume requests in push step 6.
const DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY: usize = 64;

/// Default: adaptive upload concurrency is off (use fixed semaphore).
const DEFAULT_PUSH_ADAPTIVE_CONCURRENCY: bool = false;

/// Default lower bound for the adaptive concurrency controller.
const DEFAULT_PUSH_MIN_CONCURRENCY: usize = 4;

/// Default upper bound for the adaptive concurrency controller.
const DEFAULT_PUSH_MAX_CONCURRENCY: usize = 64;

/// Default xorb target size: 64 MiB.
const DEFAULT_XORB_TARGET_SIZE: u64 = 64 * 1024 * 1024;

/// Default minimum remaining run bytes to trigger xorb extension: 1 MiB.
const DEFAULT_MIN_RUN_TAIL: u64 = 1024 * 1024;

/// Default maximum xorb overshoot fraction (10% of target).
const DEFAULT_MAX_XORB_OVERSHOOT_PERCENT: f64 = 0.10;

/// Default preferred file-boundary distance for split decisions: 2 MiB.
const DEFAULT_SPLIT_PREFER_BOUNDARY: u64 = 2 * 1024 * 1024;

/// Default adaptive xorb sizing: disabled.
const DEFAULT_ADAPTIVE_XORB_SIZE: bool = false;

/// Default minimum xorb target size: 16 MiB.
const DEFAULT_MIN_XORB_SIZE: u64 = 16 * 1024 * 1024;

/// Default maximum xorb target size: 256 MiB.
const DEFAULT_MAX_XORB_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum byte/count field encodable by the xorb v1 and shard metadata layout.
const MAX_XORB_LAYOUT_U32: u64 = u32::MAX as u64;

/// Default rolling window size for `DefragPrevention`.
const DEFAULT_DEFRAG_WINDOW_SIZE: usize = 10;

/// Default minimum chunks-per-range threshold for `DefragPrevention`.
const DEFAULT_MIN_CHUNKS_PER_RANGE: f64 = 16.0;

/// Default perf state file path.
const DEFAULT_PERF_PATH: &str = ".crab/perf-state.json";

/// Default memory ceiling for the in-memory ChunkIndex table: 64 MiB.
///
/// At ~40 bytes per entry this allows ≈ 1.6 M chunk entries in the HashMap.
/// When exceeded, newly downloaded shards spill to on-disk `MDBShardFile`
/// handles instead of being loaded into memory.
const DEFAULT_SHARD_CHUNK_INDEX_TABLE_MAX_SIZE: u64 = 64 * 1024 * 1024;

/// Fully resolved configuration produced by the config resolution pipeline.
///
/// Carries every setting a subsystem might need. Built by layering compiled
/// defaults ← user TOML ← repo TOML ← remote JSON ← env allowlist.
#[derive(Debug, Clone)]
pub struct Config {
    // -- Remote settings (authoritative from remote JSON) --
    /// Minimum file size (bytes) before content-defined chunking kicks in.
    pub chunk_threshold_bytes: u64,
    /// Whether the XET large-file protocol is enabled for this repo.
    pub xet_enabled: bool,
    /// Object-store key prefix for XET objects.
    pub xet_prefix: String,
    /// Compression algorithm and level for stored xorbs.
    pub compression: CompressionConfig,
    /// When `true`, use adaptive compression (BG4 + entropy probe).
    /// When `false`, use fixed compression with the configured algorithm.
    pub compression_adaptive: bool,
    /// Default branch name for new repos.
    pub default_branch: String,
    /// Minimum CLI version required by the remote. Writes are refused when
    /// the running binary is older than this.
    pub required_cli_version: Option<semver::VersionReq>,

    // -- Network settings --
    /// Maximum concurrent xorb/pack uploads.
    pub upload_concurrency: usize,
    /// Maximum concurrent xorb/pack downloads.
    pub download_concurrency: usize,
    /// Per-operation timeout for object-store requests.
    pub operation_timeout: Duration,
    /// Maximum retries for transient failures.
    pub max_retries: u32,

    // -- Cache settings --
    /// Maximum bytes for the local chunk cache.
    pub chunk_cache_bytes: u64,
    /// Maximum bytes for the local shard cache (`None` = unbounded).
    pub shard_cache_bytes: Option<u64>,

    // -- Shard settings --
    /// Memory ceiling for the in-memory ChunkIndex HashMap (bytes).
    /// When exceeded, new shards spill to on-disk `MDBShardFile` handles.
    pub shard_chunk_index_table_max_size: u64,

    // -- GC settings --
    /// Grace period: objects younger than this are never deleted by GC.
    pub gc_grace_period: Duration,
    /// Maximum concurrent DELETE requests during GC sweep.
    pub gc_delete_concurrency: usize,
    /// Maximum concurrent prefix-sharded LIST requests during GC enumeration.
    pub gc_list_concurrency: usize,

    // -- Checkout settings --
    /// Checkout behavior settings from the `[checkout]` section.
    pub checkout: CheckoutConfig,

    // -- Hydrate settings --
    /// Hydration pattern settings from the `[hydrate]` section.
    pub hydrate: HydrateConfig,

    // -- Tier settings --
    /// Lifecycle tiering configuration from the `[tier]` section.
    pub tier: TierConfig,

    // -- Cost settings --
    /// Cost optimizer configuration from the `[cost]` section.
    pub cost: CostConfig,

    // -- Restripe settings --
    /// Restripe profile configuration from the `[restripe]` section.
    pub restripe: RestripeConfig,

    // -- GC settings (structured) --
    /// GC class-aware configuration from the `[gc]` section.
    pub gc: GcConfig,

    // -- MetaDB settings --
    /// `[metadb]` section: SlateDB tunables per database plus the
    /// local chunk-index cache. Merged into a
    /// [`crate::metadata::MetaDbConfig`] by
    /// [`Config::build_metadb_config`] at session construction time.
    pub metadb: MetaDbTomlConfig,

    // -- Staging settings --
    /// Segment-based staging area configuration.
    pub staging: StagingConfig,

    // -- Cache service settings --
    /// Client-side cache service configuration from the `[cache]` section.
    pub cache: CacheConfig,

    // -- Auth settings --
    /// Authentication configuration from the `[auth]` section.
    pub auth: AuthConfig,

    // -- Perf settings --
    /// Engine tuning knobs from the `[perf]` section.
    pub perf: EngineConfig,

    // -- Repack settings --
    /// Pack count threshold for auto-warning after fetch.
    pub repack_auto_threshold: usize,

    // -- Push settings --
    /// Enable thin pack generation (delta against remote bases).
    pub push_thin_packs: bool,
    /// Lock heartbeat interval in seconds. Clamped to `10..=(ttl - 10)`.
    pub push_lock_heartbeat_interval: u64,
    /// Push lock TTL in seconds. Defaults to 300 (5 minutes).
    pub push_lock_ttl_secs: u64,
    /// Seconds to wait for contested push locks before failing.
    pub push_lock_wait_secs: u64,
    /// Maximum manifest CAS retry attempts before surfacing retryable stale-info.
    pub push_max_cas_retries: u32,
    /// Maximum number of concurrent HEAD/LIST requests issued by the
    /// HEAD-check resume step (step 6 of the push pipeline). Defaults to 64.
    pub push_head_check_concurrency: usize,
    /// Reserved for adaptive upload concurrency; currently logs and uses a
    /// bounded fixed semaphore because the pinned xet-core API keeps final
    /// transfer reporting crate-private.
    pub push_adaptive_concurrency: bool,
    /// Lower bound for the adaptive concurrency controller (default: 4).
    pub push_min_concurrency: usize,
    /// Upper bound for the adaptive concurrency controller (default: 64).
    pub push_max_concurrency: usize,
    /// Target xorb size in bytes for the packing pipeline.
    pub xorb_target_size: u64,
    /// When `true`, dynamically adjust xorb target size based on upload throughput.
    pub adaptive_xorb_size: bool,
    /// Minimum xorb target size in bytes for adaptive sizing (default: 16 MiB).
    pub min_xorb_size: u64,
    /// Maximum xorb target size in bytes for adaptive sizing (default: 256 MiB).
    pub max_xorb_size: u64,
    /// Minimum remaining run bytes to trigger xorb extension (default: 1 MiB).
    pub min_run_tail: u64,
    /// Maximum xorb overshoot as a fraction of target size (default: 0.10).
    pub max_xorb_overshoot_percent: f64,
    /// Prefer file boundary within this distance when splitting (default: 2 MiB).
    pub split_prefer_boundary: u64,
    /// Rolling window size for the `DefragPrevention` estimator (default: 10).
    pub defrag_window_size: usize,
    /// Minimum chunks-per-range threshold for `DefragPrevention` (default: 16.0).
    pub min_chunks_per_range: f64,

    // -- Receive policy (server-side knobs mirroring git's
    //    `receive.denyDeletes` / `receive.denyNonFastForwards` /
    //    `receive.denyCurrentBranch`). All default to `false` / `"warn"`
    //    so existing installations see no behavior change. --
    /// Reject any `git push origin :branch` delete-refspec.
    pub receive_deny_deletes: bool,
    /// Reject any non-fast-forward update (including `+branch:branch`
    /// force pushes). Git's default in shared repos is `true`; we
    /// default to `false` to match single-user workflows and let
    /// multi-tenant repos opt in via config.
    pub receive_deny_non_fast_forwards: bool,
    /// Behavior when a push targets the remote's HEAD symref. Values:
    /// `"refuse"` (reject), `"warn"` (allow with warning log),
    /// `"ignore"` (allow silently). `"updateInstead"` (git's fourth
    /// option) is not supported — crab is serverless and cannot
    /// safely update a non-bare remote's working tree. Defaults to
    /// `"ignore"` since crab repos are bare-like by nature (no
    /// working tree on the remote).
    pub receive_deny_current_branch: String,
    /// Maximum size of each pack generated on push, in bytes.
    /// Mirrors git's `receive.maxInputSize`. Aggregate closures are
    /// split into bounded packs; a single object that cannot fit is
    /// rejected before upload. A value of `0` disables the bound.
    pub receive_max_input_size: u64,
    /// Maximum number of commits walked by the commit-graph-summary
    /// ancestry fallback used when `git merge-base --is-ancestor`
    /// can't answer (e.g. shallow/sparse clients missing the old
    /// SHA locally). The walk starts from the incoming tip and
    /// follows parent edges recorded in the summary; if `old` isn't
    /// found within this many commits, the push is conservatively
    /// rejected as non-fast-forward. `0` disables the fallback.
    pub receive_ff_summary_window_commits: u64,

    /// Glob patterns for refs to hide from `list` / `list for-push`
    /// output and to reject as fetch targets. Mirrors git's
    /// `transfer.hideRefs`. Matched against the full ref name with
    /// `globset` (the same matcher git uses internally — not
    /// filesystem globs). Empty by default.
    ///
    /// Hidden refs still exist on the remote and can be pushed to —
    /// this gates only the advertisement and the fetch-side policy.
    /// Rejected fetches surface as [`FetchRejectReason::NotAllowed`]
    /// with `hidden-ref target` detail.
    pub transfer_hide_refs: Vec<String>,

    // -- Upload-pack policy (server-side knobs mirroring git's
    //    `uploadpack.allowAnySHA1InWant` /
    //    `uploadpack.allowTipSHA1InWant` /
    //    `uploadpack.allowReachableSHA1InWant`). These gate raw-SHA
    //    `fetch <sha> <ref>` lines so a client cannot request an
    //    arbitrary interior commit SHA — a well-known smart-HTTP
    //    information-leak surface. --
    /// Permit `fetch <sha>` for any SHA in the ODB regardless of ref
    /// reachability. Maps to git's `uploadpack.allowAnySHA1InWant`.
    /// When `true`, overrides `allow_tip` and `allow_reachable`.
    pub uploadpack_allow_any_sha_in_want: bool,
    /// Permit `fetch <sha>` when the SHA is the tip of an advertised
    /// ref (i.e. `manifest.refs.values()` contains it). Maps to
    /// git's `uploadpack.allowTipSHA1InWant`. Default `true` — this
    /// is the conservative mid-tier that covers the normal
    /// `git fetch origin <sha>` on a published tip.
    pub uploadpack_allow_tip_sha_in_want: bool,
    /// Permit `fetch <sha>` when the SHA is reachable from any
    /// advertised ref via the commit graph. Maps to git's
    /// `uploadpack.allowReachableSHA1InWant`. Requires a commit-graph
    /// summary; walks fall back to "not reachable" when the summary
    /// is absent.
    pub uploadpack_allow_reachable_sha_in_want: bool,

    /// Maximum bytes the server will return in a single fetch batch.
    /// Mirrors git's `uploadpack.maxEgressBytes`. Checked as a running
    /// total across all packs in the batch. A value of `0` disables the
    /// check. Default 10 GiB — large enough that normal clones/fetches
    /// stay under the limit, small enough that a runaway client cannot
    /// burn through unlimited transfer cost.
    pub uploadpack_max_egress_bytes: u64,

    // -- Fetch settings --
    /// Use Pack_Metadata to download only packs matching requested refs.
    pub fetch_ref_filtering: bool,
    /// Skip indexing objects already present locally after download.
    pub fetch_object_level_filtering: bool,

    // -- Remote settings (from [remote] section) --
    /// Remote URL from the `[remote]` section (e.g. `crab://bucket/repo`).
    pub remote_url: Option<String>,
    /// AWS region for the remote bucket (e.g. `us-east-1`).
    pub remote_region: Option<String>,
    /// Read-replica configuration. Writes always target `remote_url`.
    pub replication: Option<ReplicationConfig>,

    // -- Perf persistence settings --
    /// Whether to persist perf counters to disk.
    pub perf_persist: bool,
    /// Path for persisted perf counter state file.
    pub perf_path: String,

    // -- Workflow settings --
    /// Workflow-layer configuration from the `[workflow]` section.
    pub workflow: WorkflowConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chunk_threshold_bytes: DEFAULT_CHUNK_THRESHOLD_BYTES,
            xet_enabled: true,
            xet_prefix: String::from("xet"),
            compression: CompressionConfig::default(),
            compression_adaptive: true,
            default_branch: String::from("main"),
            required_cli_version: None,
            upload_concurrency: DEFAULT_UPLOAD_CONCURRENCY,
            download_concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            chunk_cache_bytes: DEFAULT_CHUNK_CACHE_BYTES,
            shard_cache_bytes: None,
            shard_chunk_index_table_max_size: DEFAULT_SHARD_CHUNK_INDEX_TABLE_MAX_SIZE,
            gc_grace_period: DEFAULT_GC_GRACE_PERIOD,
            gc_delete_concurrency: DEFAULT_GC_DELETE_CONCURRENCY,
            gc_list_concurrency: DEFAULT_GC_LIST_CONCURRENCY,
            checkout: CheckoutConfig::default(),
            hydrate: HydrateConfig::default(),
            tier: TierConfig::default(),
            cost: CostConfig::default(),
            restripe: RestripeConfig::default(),
            gc: GcConfig::default(),
            metadb: MetaDbTomlConfig::default(),
            staging: StagingConfig::default(),
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            perf: EngineConfig::default(),
            repack_auto_threshold: DEFAULT_REPACK_AUTO_THRESHOLD,
            push_thin_packs: false,
            push_lock_heartbeat_interval: DEFAULT_PUSH_LOCK_HEARTBEAT_INTERVAL,
            push_lock_ttl_secs: DEFAULT_PUSH_LOCK_TTL_SECS,
            push_lock_wait_secs: DEFAULT_PUSH_LOCK_WAIT_SECS,
            push_max_cas_retries: DEFAULT_PUSH_MAX_CAS_RETRIES,
            push_head_check_concurrency: DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY,
            push_adaptive_concurrency: DEFAULT_PUSH_ADAPTIVE_CONCURRENCY,
            push_min_concurrency: DEFAULT_PUSH_MIN_CONCURRENCY,
            push_max_concurrency: DEFAULT_PUSH_MAX_CONCURRENCY,
            xorb_target_size: DEFAULT_XORB_TARGET_SIZE,
            adaptive_xorb_size: DEFAULT_ADAPTIVE_XORB_SIZE,
            min_xorb_size: DEFAULT_MIN_XORB_SIZE,
            max_xorb_size: DEFAULT_MAX_XORB_SIZE,
            min_run_tail: DEFAULT_MIN_RUN_TAIL,
            max_xorb_overshoot_percent: DEFAULT_MAX_XORB_OVERSHOOT_PERCENT,
            split_prefer_boundary: DEFAULT_SPLIT_PREFER_BOUNDARY,
            defrag_window_size: DEFAULT_DEFRAG_WINDOW_SIZE,
            min_chunks_per_range: DEFAULT_MIN_CHUNKS_PER_RANGE,
            receive_deny_deletes: false,
            receive_deny_non_fast_forwards: false,
            receive_deny_current_branch: String::from("ignore"),
            receive_max_input_size: DEFAULT_RECEIVE_MAX_INPUT_SIZE,
            receive_ff_summary_window_commits: DEFAULT_RECEIVE_FF_SUMMARY_WINDOW_COMMITS,

            transfer_hide_refs: Vec::new(),
            uploadpack_allow_any_sha_in_want: false,
            uploadpack_allow_tip_sha_in_want: true,
            uploadpack_allow_reachable_sha_in_want: false,
            uploadpack_max_egress_bytes: DEFAULT_UPLOADPACK_MAX_EGRESS_BYTES,
            fetch_ref_filtering: false,
            fetch_object_level_filtering: false,
            remote_url: None,
            remote_region: None,
            replication: None,
            perf_persist: true,
            perf_path: String::from(DEFAULT_PERF_PATH),
            workflow: WorkflowConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config overlay — partial config from a single layer
// ---------------------------------------------------------------------------

/// Per-user config directory relative to `$HOME`.
const USER_CONFIG_REL: &str = ".config/crab/config.toml";

/// Per-repo config path relative to the repo root.
const REPO_CONFIG_REL: &str = ".crab/config.toml";

/// Resolve the absolute paths to the per-repo and project config files.
///
/// For linked worktrees, the `.crab/` directory lives in the main
/// worktree root (the parent of the common git dir), not in the linked
/// worktree's working directory. This function discovers the git dir,
/// resolves `commondir` for linked worktrees, and returns the absolute
/// path to `.crab/config.toml` in the main worktree. The project config
/// stays rooted in the current worktree so branch-specific `.crab.toml`
/// files resolve the same way Git resolves tracked files.
///
/// Returns `None` if discovery fails (e.g. not inside a git repo),
/// in which case the caller falls back to the relative path.
fn resolve_repo_config_paths() -> Option<(PathBuf, PathBuf)> {
    repo_config_paths_for_root(Path::new("."))
}

fn repo_config_paths_for_root(start: &Path) -> Option<(PathBuf, PathBuf)> {
    if let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve_from_path(start) {
        return Some((
            ctx.shared_crab_dir.join("config.toml"),
            ctx.current_worktree_root.join(".crab.toml"),
        ));
    }

    let (git_dir, current_worktree_root) = match gix_discover::upwards(start) {
        Ok((repo_path, _trust)) => {
            let (git_dir, work_tree) = repo_path.into_repository_and_work_tree_directories();
            (git_dir, work_tree?)
        }
        Err(_) => return None,
    };

    let common_dir = resolve_common_dir_for_config(&git_dir);
    let repo_root = common_dir.parent()?;

    Some((
        repo_root.join(REPO_CONFIG_REL),
        current_worktree_root.join(".crab.toml"),
    ))
}

/// Resolve the common directory for a git dir. For linked worktrees,
/// the git dir contains a `commondir` file with a relative path to
/// the shared `.git/` directory. For normal repos, returns git_dir.
fn resolve_common_dir_for_config(git_dir: &std::path::Path) -> PathBuf {
    let commondir_file = git_dir.join("commondir");
    if let Ok(content) = std::fs::read_to_string(&commondir_file) {
        let relative = content.trim();
        if !relative.is_empty() {
            let resolved = git_dir.join(relative);
            if let Ok(canonical) = resolved.canonicalize() {
                return canonical;
            }
            return resolved;
        }
    }
    git_dir.to_path_buf()
}

/// A partial configuration from a single layer (TOML file or remote JSON).
///
/// Every field is `Option<T>` — only keys present in the source are `Some`.
/// The resolve pipeline merges overlays in priority order: later layers
/// override earlier ones for any key they set.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigOverlay {
    pub chunk_threshold_bytes: Option<u64>,
    pub xet_enabled: Option<bool>,
    pub xet_prefix: Option<String>,
    pub compression: Option<String>,
    pub compression_adaptive: Option<bool>,
    pub default_branch: Option<String>,
    pub required_cli_version: Option<String>,

    pub upload_concurrency: Option<usize>,
    pub download_concurrency: Option<usize>,
    pub operation_timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,

    pub chunk_cache_bytes: Option<u64>,
    pub shard_cache_bytes: Option<u64>,

    pub gc_grace_period_secs: Option<u64>,
    pub gc_delete_concurrency: Option<usize>,
    pub gc_list_concurrency: Option<usize>,

    pub staging: Option<StagingOverlay>,
    pub perf: Option<PerfOverlay>,
    pub checkout: Option<CheckoutOverlay>,
    pub hydrate: Option<HydrateOverlay>,
    pub repack: Option<RepackOverlay>,
    pub push: Option<PushOverlay>,
    pub receive: Option<ReceiveOverlay>,
    pub uploadpack: Option<UploadpackOverlay>,
    pub transfer: Option<TransferOverlay>,
    pub fetch: Option<FetchOverlay>,
    pub remote: Option<RemoteOverlay>,
    pub replication: Option<ReplicationConfig>,
    pub cache: Option<CacheOverlay>,
    pub shard: Option<ShardOverlay>,
    pub auth: Option<AuthOverlay>,
    pub workflow: Option<WorkflowOverlay>,
    pub hydra: Option<HydraOverlay>,
    pub tier: Option<TierOverlay>,
    pub cost: Option<CostOverlay>,
    pub restripe: Option<RestripeOverlay>,
    pub gc: Option<GcOverlay>,
    pub metadb: Option<MetaDbTomlConfig>,
}

/// Partial staging configuration overlay.
///
/// Accepts both canonical names (`segment_target_bytes`, `compact_dead_ratio`)
/// and the aliases used in the architecture doc (`segment_target_size`,
/// `compaction_dead_ratio`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagingOverlay {
    pub segment_target_bytes: Option<u64>,
    /// Alias for `segment_target_bytes` (architecture doc name).
    pub segment_target_size: Option<u64>,
    pub segment_hard_cap_bytes: Option<u64>,
    pub fd_pool_size: Option<usize>,
    pub batch_read_size: Option<usize>,
    pub auto_compact: Option<bool>,
    pub compact_dead_ratio: Option<f64>,
    /// Alias for `compact_dead_ratio` (architecture doc name).
    pub compaction_dead_ratio: Option<f64>,
    pub durable_register: Option<bool>,
    pub retention_hours: Option<u64>,
}

/// Partial perf/engine configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerfOverlay {
    pub enabled: Option<bool>,
    pub shard_bloom: Option<bool>,
    pub pointer_shard_hint: Option<bool>,
    pub compress_staging: Option<bool>,
    pub adaptive_threshold: Option<bool>,
    pub fastpath_min_size: Option<u64>,
    pub persist: Option<bool>,
    pub path: Option<String>,
}

/// Partial checkout configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutOverlay {
    pub lazy: Option<bool>,
}

/// Partial hydration configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydrateOverlay {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub auto: Option<bool>,
    pub download_concurrency: Option<usize>,
    pub prefetch_budget: Option<u64>,
    pub auto_restore: Option<bool>,
    pub auto_prefetch: Option<bool>,
    pub speculative: Option<bool>,
    pub speculative_concurrency: Option<usize>,
}

/// Partial tier configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierOverlay {
    pub enabled: Option<bool>,
    pub to_ia_days: Option<u32>,
    pub to_deep_days: Option<u32>,
    pub noncurrent_days: Option<u32>,
    pub restore_tier: Option<String>,
    pub restore_duration_days: Option<u32>,
    pub restore_max_concurrency: Option<u32>,
    pub restore_timeout_secs: Option<u64>,
    pub restripe_output_class: Option<String>,
}

/// Partial cost configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostOverlay {
    pub inventory_source: Option<String>,
    pub list_concurrency: Option<u32>,
    pub sample_ratio: Option<f64>,
    pub pricing_file: Option<String>,
    pub price_table_version: Option<String>,
    pub access_window_days: Option<u32>,
    pub apply_free_tier: Option<bool>,
    pub report_max_staleness_hours: Option<u32>,
}

/// Partial restripe configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestripeOverlay {
    pub profiles: Option<std::collections::HashMap<String, ProfileOverride>>,
}

/// Partial GC configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcOverlay {
    pub class_aware: Option<bool>,
}

/// Partial repack configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepackOverlay {
    pub auto_threshold: Option<usize>,
}

/// Partial push configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushOverlay {
    pub thin_packs: Option<bool>,
    pub lock_heartbeat_interval: Option<u64>,
    pub lock_ttl_secs: Option<u64>,
    pub lock_wait_secs: Option<u64>,
    pub max_cas_retries: Option<u32>,
    pub upload_concurrency: Option<usize>,
    pub head_check_concurrency: Option<usize>,
    pub adaptive_concurrency: Option<bool>,
    pub min_concurrency: Option<usize>,
    pub max_concurrency: Option<usize>,
    pub xorb_target_size: Option<u64>,
    pub adaptive_xorb_size: Option<bool>,
    pub min_xorb_size: Option<u64>,
    pub max_xorb_size: Option<u64>,
    pub min_run_tail: Option<u64>,
    pub max_xorb_overshoot_percent: Option<f64>,
    pub split_prefer_boundary: Option<u64>,
    pub defrag_window_size: Option<usize>,
    pub min_chunks_per_range: Option<f64>,
}

/// Partial receive-policy overlay. Mirrors git's `receive.*` config
/// keys that the remote-helper push pipeline honors as preconditions
/// before any S3 write happens.
///
/// Keys are accepted in both snake_case (the crab convention,
/// `deny_deletes`) and Git's original camelCase (`denyDeletes`) so
/// admins migrating from a server config can copy values across
/// without renaming.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveOverlay {
    #[serde(alias = "denyDeletes")]
    pub deny_deletes: Option<bool>,
    #[serde(alias = "denyNonFastForwards")]
    pub deny_non_fast_forwards: Option<bool>,
    #[serde(alias = "denyCurrentBranch")]
    pub deny_current_branch: Option<String>,
    #[serde(alias = "maxInputSize")]
    pub max_input_size: Option<u64>,
    #[serde(alias = "ffSummaryWindowCommits")]
    pub ff_summary_window_commits: Option<u64>,
}

/// Partial upload-pack policy overlay. Mirrors git's
/// `uploadpack.*` keys that gate raw-SHA fetches. Accepted in
/// both snake_case and Git's camelCase forms so admins can
/// copy values across from a server config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadpackOverlay {
    #[serde(alias = "allowAnySHA1InWant")]
    pub allow_any_sha_in_want: Option<bool>,
    #[serde(alias = "allowTipSHA1InWant")]
    pub allow_tip_sha_in_want: Option<bool>,
    #[serde(alias = "allowReachableSHA1InWant")]
    pub allow_reachable_sha_in_want: Option<bool>,
    #[serde(alias = "maxEgressBytes")]
    pub max_egress_bytes: Option<u64>,
}

/// Partial transfer-policy overlay. Mirrors git's `transfer.*` keys.
/// The only field today is `hideRefs`; the overlay leaves room for
/// `transfer.unpackLimit`, `transfer.advertiseObjectInfo`, etc.
/// without another schema bump.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferOverlay {
    /// Glob patterns matching refs to hide from advertisement and
    /// reject as fetch targets. Accepted as `hide_refs` (snake_case,
    /// crab convention) or `hideRefs` (Git's camelCase).
    #[serde(alias = "hideRefs")]
    pub hide_refs: Option<Vec<String>>,
}

/// Partial fetch configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchOverlay {
    pub ref_filtering: Option<bool>,
    pub object_level_filtering: Option<bool>,
}

/// Partial remote configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOverlay {
    pub url: Option<String>,
    pub region: Option<String>,
}

/// Partial cache service configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheOverlay {
    pub service_url: Option<String>,
    pub service_mode: Option<String>,
    pub push_warming: Option<bool>,
    /// Directory for the xet-core `DiskCache`. Accepts a path string;
    /// `None` or absent keeps the default (`~/.cache/crab/chunks/`).
    pub chunk_cache_dir: Option<String>,
    /// Authentication mode: `"psk"`, `"bearer"`, `"mtls"`, or `"none"`.
    pub service_auth: Option<String>,
    /// Pre-shared key value (used when `service_auth = "psk"`).
    pub service_psk: Option<String>,
    /// Path to a file containing the bearer token (used when
    /// `service_auth = "bearer"`).
    pub service_token_path: Option<String>,
    /// Path to a PEM CA bundle for private TLS CAs.
    pub service_ca_cert: Option<String>,
    /// Path to the PEM client certificate chain for native mTLS.
    pub service_client_cert: Option<String>,
    /// Path to the PEM private key for native mTLS.
    pub service_client_key: Option<String>,
}

/// Partial shard configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardOverlay {
    /// Memory ceiling for the in-memory ChunkIndex HashMap (bytes).
    /// Default: 64 MiB. When exceeded, new shards spill to on-disk
    /// `MDBShardFile` handles for interpolation search.
    pub chunk_index_table_max_size: Option<u64>,
}

/// Partial auth configuration overlay from the `[auth]` TOML section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOverlay {
    pub provider: Option<AuthProvider>,
    pub storage_provider: Option<StorageProvider>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub auth_endpoint: Option<String>,
    pub scopes: Option<String>,
    pub token_cache_path: Option<String>,
    pub aws: Option<AwsAuthOverlay>,
    pub gcp: Option<GcpAuthOverlay>,
    pub azure: Option<AzureAuthOverlay>,
}

/// Partial AWS auth configuration overlay from `[auth.aws]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsAuthOverlay {
    pub role_arn: Option<String>,
    pub region: Option<String>,
    pub session_duration_secs: Option<u64>,
}

/// Partial GCP auth configuration overlay from `[auth.gcp]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcpAuthOverlay {
    pub workload_identity_pool: Option<String>,
    pub service_account: Option<String>,
    pub project_id: Option<String>,
}

/// Partial Azure auth configuration overlay from `[auth.azure]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureAuthOverlay {
    pub tenant_id: Option<String>,
    pub subscription_id: Option<String>,
    pub storage_account: Option<String>,
}

/// Partial workflow configuration overlay from the `[workflow]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOverlay {
    pub enabled: Option<bool>,
    pub discover: Option<WorkflowDiscover>,
    pub lockfile: Option<WorkflowLockfile>,
    pub parallelism: Option<u32>,
    pub graceful_shutdown_timeout_secs: Option<u64>,
    pub max_outs_per_stage: Option<usize>,
    pub max_out_bytes: Option<u64>,
    pub lock_timeout_secs: Option<u64>,
    pub remote_cache_readonly: Option<bool>,
    pub remotes: Option<BTreeMap<String, WorkflowRemoteConfig>>,
}

/// Partial Hydra experiment composition configuration from `[hydra]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydraOverlay {
    pub enabled: Option<bool>,
    pub config_dir: Option<String>,
    pub config_name: Option<String>,
}

impl Config {
    /// Phase 1: resolve layers 1–3 + env allowlist.
    ///
    /// Returns a config sufficient to construct a `Store` (credentials come
    /// from the AWS SDK / env, not from crab config). Offline commands
    /// can stop here.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::CrabError::Configuration`] on malformed
    /// TOML or invalid field values.
    pub fn resolve_local() -> super::error::Result<Self> {
        let user_path = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(USER_CONFIG_REL));
        let (repo_path, project_path) = match resolve_repo_config_paths() {
            Some(paths) => paths,
            None => (PathBuf::from(REPO_CONFIG_REL), PathBuf::from(".crab.toml")),
        };

        Self::resolve_local_from_with_project(user_path, repo_path, Some(project_path))
    }

    /// Resolve config for a specific repository root without changing the
    /// process working directory.
    pub fn resolve_for_repo(repo_root: &std::path::Path) -> super::error::Result<Self> {
        let user_path = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(USER_CONFIG_REL));
        let (repo_path, project_path) =
            repo_config_paths_for_root(repo_root).unwrap_or_else(|| {
                (
                    repo_root.join(REPO_CONFIG_REL),
                    repo_root.join(".crab.toml"),
                )
            });
        Self::resolve_local_from_with_project(user_path, repo_path, Some(project_path))
    }

    /// Testable inner implementation that accepts explicit paths.
    #[cfg(test)]
    fn resolve_local_from(
        user_path: Option<PathBuf>,
        repo_path: PathBuf,
    ) -> super::error::Result<Self> {
        let project_path = project_config_path_from_repo_config(&repo_path);
        Self::resolve_local_from_with_project(user_path, repo_path, project_path)
    }

    fn resolve_local_from_with_project(
        user_path: Option<PathBuf>,
        repo_path: PathBuf,
        project_path: Option<PathBuf>,
    ) -> super::error::Result<Self> {
        let mut config = Config::default();

        // Layer 2: per-user TOML
        if let Some(ref path) = user_path
            && path.is_file()
        {
            let overlay = read_toml_overlay(path)?;
            config.apply_overlay(overlay, path.display().to_string())?;
        }

        // Layer 3: per-repo TOML
        if repo_path.is_file() {
            let overlay = read_toml_overlay(&repo_path)?;
            config.apply_overlay(overlay, repo_path.display().to_string())?;
        }

        if let Some(project_path) = project_path
            && project_path.is_file()
            && let Ok(project) = crate::core::project_config::ProjectConfig::load(&project_path)
        {
            if config.remote_url.is_none() {
                config.remote_url = Some(project.remote.url);
            }
            if let Some(replication) = project.replication {
                config.replication = Some(replication);
            }
            if let Some(auth) = project.auth
                && let Some(storage_provider) = auth.storage_provider
            {
                config.auth.storage_provider = storage_provider;
            }
        }

        // Layer 5: env allowlist — only CRAB_LOG, CRAB_OTLP_ENDPOINT,
        // CRAB_PROFILE are recognized for tracing (consumed by the
        // subscriber, not Config fields). The workflow subsystem
        // additionally honors the `CRAB_WORKFLOW_*` prefix so operators
        // can toggle the feature flag and tune limits without editing TOML.
        config.workflow.apply_env_overrides();
        config.tier.apply_env_overrides();
        config.cost.apply_env_overrides();
        config.gc.apply_env_overrides();
        config.metadb.apply_env_overrides();
        config.cache.apply_env_overrides();

        config.validate_resolved()?;
        Ok(config)
    }

    /// Phase 2: merge remote config JSON (layer 4) onto a local config.
    ///
    /// Called after constructing a `Store` with the local config. The remote
    /// JSON is fetched from the object store by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::CrabError::Configuration`] on malformed
    /// JSON or invalid field values.
    pub fn resolve_remote(mut self, remote_json: &[u8]) -> super::error::Result<Self> {
        if remote_json.is_empty() {
            return Ok(self);
        }

        let overlay: ConfigOverlay = serde_json::from_slice(remote_json).map_err(|e| {
            super::error::CrabError::Configuration {
                key: e.to_string(),
                origin: "remote config JSON".into(),
            }
        })?;

        self.apply_overlay(overlay, "remote config JSON".to_string())?;
        self.validate_resolved()?;
        Ok(self)
    }

    /// Binary version string set by `build.rs` from `CARGO_PKG_VERSION`.
    const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

    /// Check that the running binary satisfies `required_cli_version`.
    ///
    /// Returns `Ok(())` when no version requirement is set or when the
    /// binary version matches. Returns
    /// [`CrabError::IncompatibleFormat`] when the requirement is not
    /// satisfied — callers should refuse write operations in that case.
    pub fn check_version_guard(&self) -> super::error::Result<()> {
        let Some(ref req) = self.required_cli_version else {
            return Ok(());
        };

        let current = semver::Version::parse(Self::BINARY_VERSION).map_err(|e| {
            super::error::CrabError::Internal(format!(
                "failed to parse binary version \"{}\": {e}",
                Self::BINARY_VERSION,
            ))
        })?;

        if req.matches(&current) {
            Ok(())
        } else {
            Err(super::error::CrabError::IncompatibleFormat {
                required: req.to_string(),
                found: current.to_string(),
            })
        }
    }

    fn validate_resolved(&self) -> super::error::Result<()> {
        self.staging.validate()?;
        self.validate_push()?;
        self.validate_cache()?;
        Ok(())
    }

    fn validate_push(&self) -> super::error::Result<()> {
        if self.push_head_check_concurrency == 0 {
            return Err(super::error::CrabError::Configuration {
                key: "head_check_concurrency must be > 0".into(),
                origin: "push".into(),
            });
        }
        if self.push_min_concurrency == 0 {
            return Err(super::error::CrabError::Configuration {
                key: "min_concurrency must be > 0".into(),
                origin: "push".into(),
            });
        }
        if self.push_max_concurrency < self.push_min_concurrency {
            return Err(super::error::CrabError::Configuration {
                key: "max_concurrency must be >= min_concurrency".into(),
                origin: "push".into(),
            });
        }
        if self.min_xorb_size == 0 {
            return Err(super::error::CrabError::Configuration {
                key: "min_xorb_size must be > 0".into(),
                origin: "push".into(),
            });
        }
        if self.max_xorb_size < self.min_xorb_size {
            return Err(super::error::CrabError::Configuration {
                key: "max_xorb_size must be >= min_xorb_size".into(),
                origin: "push".into(),
            });
        }
        if self.max_xorb_size > MAX_XORB_LAYOUT_U32 {
            return Err(super::error::CrabError::Configuration {
                key: format!("max_xorb_size must be <= {MAX_XORB_LAYOUT_U32}"),
                origin: "push".into(),
            });
        }
        if self.xorb_target_size < self.min_xorb_size || self.xorb_target_size > self.max_xorb_size
        {
            return Err(super::error::CrabError::Configuration {
                key: format!(
                    "xorb_target_size must be in {}..={}",
                    self.min_xorb_size, self.max_xorb_size
                ),
                origin: "push".into(),
            });
        }
        if !self.max_xorb_overshoot_percent.is_finite() || self.max_xorb_overshoot_percent < 0.0 {
            return Err(super::error::CrabError::Configuration {
                key: "max_xorb_overshoot_percent must be finite and >= 0".into(),
                origin: "push".into(),
            });
        }
        let max_overshoot = (self.xorb_target_size as f64 * self.max_xorb_overshoot_percent).ceil();
        if max_overshoot > (MAX_XORB_LAYOUT_U32 - self.xorb_target_size) as f64 {
            return Err(super::error::CrabError::Configuration {
                key: format!(
                    "max_xorb_overshoot_percent lets xorb_target_size exceed {MAX_XORB_LAYOUT_U32}"
                ),
                origin: "push".into(),
            });
        }
        if self.defrag_window_size == 0 {
            return Err(super::error::CrabError::Configuration {
                key: "defrag_window_size must be > 0".into(),
                origin: "push".into(),
            });
        }
        if !self.min_chunks_per_range.is_finite() || self.min_chunks_per_range <= 0.0 {
            return Err(super::error::CrabError::Configuration {
                key: "min_chunks_per_range must be finite and > 0".into(),
                origin: "push".into(),
            });
        }
        Ok(())
    }

    fn validate_cache(&self) -> super::error::Result<()> {
        if matches!(self.cache.service_auth, ServiceAuth::Mtls)
            && (self.cache.service_client_cert.is_none() || self.cache.service_client_key.is_none())
        {
            return Err(super::error::CrabError::Configuration {
                key: "cache.service_auth = \"mtls\" requires service_client_cert and service_client_key".into(),
                origin: "cache".into(),
            });
        }
        if self.cache.service_client_cert.is_some() != self.cache.service_client_key.is_some() {
            return Err(super::error::CrabError::Configuration {
                key: "cache.service_client_cert and cache.service_client_key must be set together"
                    .into(),
                origin: "cache".into(),
            });
        }
        Ok(())
    }

    /// Merge an overlay onto this config. Fields that are `Some` in the
    /// overlay override the corresponding config field.
    fn apply_overlay(
        &mut self,
        overlay: ConfigOverlay,
        origin: String,
    ) -> super::error::Result<()> {
        if let Some(v) = overlay.chunk_threshold_bytes {
            self.chunk_threshold_bytes = v;
        }
        if let Some(v) = overlay.xet_enabled {
            self.xet_enabled = v;
        }
        if let Some(v) = overlay.xet_prefix {
            self.xet_prefix = v;
        }
        if let Some(ref v) = overlay.compression {
            self.compression = parse_compression(v, &origin)?;
        }
        if let Some(v) = overlay.compression_adaptive {
            self.compression_adaptive = v;
        }
        if let Some(v) = overlay.default_branch {
            self.default_branch = v;
        }
        if let Some(ref v) = overlay.required_cli_version {
            let req = semver::VersionReq::parse(v).map_err(|e| {
                super::error::CrabError::Configuration {
                    key: format!("required_cli_version: {e}"),
                    origin: origin.clone(),
                }
            })?;
            self.required_cli_version = Some(req);
        }
        if let Some(v) = overlay.upload_concurrency {
            self.upload_concurrency = v;
        }
        if let Some(v) = overlay.download_concurrency {
            self.download_concurrency = v;
        }
        if let Some(v) = overlay.operation_timeout_secs {
            self.operation_timeout = Duration::from_secs(v);
        }
        if let Some(v) = overlay.max_retries {
            self.max_retries = v;
        }
        if let Some(v) = overlay.chunk_cache_bytes {
            self.chunk_cache_bytes = v;
        }
        if let Some(v) = overlay.shard_cache_bytes {
            self.shard_cache_bytes = Some(v);
        }
        if let Some(v) = overlay.gc_grace_period_secs {
            self.gc_grace_period = Duration::from_secs(v);
        }
        if let Some(v) = overlay.gc_delete_concurrency {
            self.gc_delete_concurrency = v;
        }
        if let Some(v) = overlay.gc_list_concurrency {
            self.gc_list_concurrency = v;
        }

        if let Some(staging) = overlay.staging {
            self.apply_staging_overlay(staging);
        }
        if let Some(perf) = overlay.perf {
            self.apply_perf_overlay(perf);
        }
        if let Some(checkout) = overlay.checkout {
            self.apply_checkout_overlay(checkout);
        }
        if let Some(hydrate) = overlay.hydrate {
            self.apply_hydrate_overlay(hydrate);
        }

        if let Some(repack) = overlay.repack {
            self.apply_repack_overlay(repack);
        }
        if let Some(push) = overlay.push {
            self.apply_push_overlay(push);
        }
        if let Some(receive) = overlay.receive {
            self.apply_receive_overlay(receive);
        }
        if let Some(uploadpack) = overlay.uploadpack {
            self.apply_uploadpack_overlay(uploadpack);
        }
        if let Some(transfer) = overlay.transfer {
            self.apply_transfer_overlay(transfer);
        }
        if let Some(fetch) = overlay.fetch {
            self.apply_fetch_overlay(fetch);
        }
        if let Some(remote) = overlay.remote {
            self.apply_remote_overlay(remote);
        }
        if let Some(replication) = overlay.replication {
            self.replication = Some(replication);
        }
        if let Some(cache) = overlay.cache {
            self.apply_cache_overlay(cache);
        }
        if let Some(shard) = overlay.shard {
            self.apply_shard_overlay(shard);
        }
        if let Some(auth) = overlay.auth {
            self.apply_auth_overlay(auth);
        }
        if let Some(workflow) = overlay.workflow {
            self.apply_workflow_overlay(workflow);
        }
        if let Some(tier) = overlay.tier {
            self.apply_tier_overlay(tier);
        }
        if let Some(cost) = overlay.cost {
            self.apply_cost_overlay(cost);
        }
        if let Some(restripe) = overlay.restripe {
            self.apply_restripe_overlay(restripe);
        }
        if let Some(gc) = overlay.gc {
            self.apply_gc_overlay(gc);
        }
        if let Some(metadb) = overlay.metadb {
            self.metadb = metadb;
        }

        Ok(())
    }

    fn apply_staging_overlay(&mut self, overlay: StagingOverlay) {
        if let Some(v) = overlay.segment_target_bytes.or(overlay.segment_target_size) {
            self.staging.segment_target_bytes = v;
        }
        if let Some(v) = overlay.segment_hard_cap_bytes {
            self.staging.segment_hard_cap_bytes = v;
        }
        if let Some(v) = overlay.fd_pool_size {
            self.staging.fd_pool_size = v;
        }
        if let Some(v) = overlay.batch_read_size {
            self.staging.batch_read_size = v;
        }
        if let Some(v) = overlay.auto_compact {
            self.staging.auto_compact = v;
        }
        if let Some(v) = overlay.compact_dead_ratio.or(overlay.compaction_dead_ratio) {
            self.staging.compact_dead_ratio = v;
        }
        if let Some(v) = overlay.durable_register {
            self.staging.durable_register = v;
        }
        if let Some(v) = overlay.retention_hours {
            self.staging.retention_hours = v;
        }
    }

    fn apply_perf_overlay(&mut self, overlay: PerfOverlay) {
        if let Some(v) = overlay.enabled {
            self.perf.enabled = v;
        }
        if let Some(v) = overlay.shard_bloom {
            self.perf.shard_bloom = v;
        }
        if let Some(v) = overlay.pointer_shard_hint {
            self.perf.pointer_shard_hint = v;
        }
        if let Some(v) = overlay.compress_staging {
            self.perf.compress_staging = v;
        }
        if let Some(v) = overlay.adaptive_threshold {
            self.perf.adaptive_threshold = v;
        }
        if let Some(v) = overlay.fastpath_min_size {
            self.perf.fastpath_min_size = v;
        }
        if let Some(v) = overlay.persist {
            self.perf_persist = v;
        }
        if let Some(v) = overlay.path {
            self.perf_path = v;
        }
    }

    fn apply_checkout_overlay(&mut self, overlay: CheckoutOverlay) {
        if let Some(v) = overlay.lazy {
            self.checkout.lazy = v;
        }
    }

    fn apply_hydrate_overlay(&mut self, overlay: HydrateOverlay) {
        if let Some(v) = overlay.include {
            self.hydrate.include = v;
        }
        if let Some(v) = overlay.exclude {
            self.hydrate.exclude = v;
        }
        if let Some(v) = overlay.auto {
            self.hydrate.auto = v;
        }
        if let Some(v) = overlay.download_concurrency {
            self.hydrate.download_concurrency = v;
        }
        if let Some(v) = overlay.prefetch_budget {
            self.hydrate.prefetch_budget = v;
        }
        if let Some(v) = overlay.auto_restore {
            self.hydrate.auto_restore = v;
        }
        if let Some(v) = overlay.auto_prefetch {
            self.hydrate.auto_prefetch = v;
        }
        if let Some(v) = overlay.speculative {
            self.hydrate.speculative = v;
        }
        if let Some(v) = overlay.speculative_concurrency {
            self.hydrate.speculative_concurrency = v;
        }
    }

    fn apply_repack_overlay(&mut self, overlay: RepackOverlay) {
        if let Some(v) = overlay.auto_threshold {
            self.repack_auto_threshold = v;
        }
    }

    fn apply_push_overlay(&mut self, overlay: PushOverlay) {
        if let Some(v) = overlay.thin_packs {
            self.push_thin_packs = v;
        }
        if let Some(v) = overlay.lock_heartbeat_interval {
            self.push_lock_heartbeat_interval = v;
        }
        if let Some(v) = overlay.lock_ttl_secs {
            self.push_lock_ttl_secs = v;
        }
        if let Some(v) = overlay.lock_wait_secs {
            self.push_lock_wait_secs = v;
        }
        if let Some(v) = overlay.max_cas_retries {
            self.push_max_cas_retries = v;
        }
        if let Some(v) = overlay.upload_concurrency {
            self.upload_concurrency = v;
        }
        if let Some(v) = overlay.head_check_concurrency {
            self.push_head_check_concurrency = v;
        }
        if let Some(v) = overlay.adaptive_concurrency {
            self.push_adaptive_concurrency = v;
        }
        if let Some(v) = overlay.min_concurrency {
            self.push_min_concurrency = v;
        }
        if let Some(v) = overlay.max_concurrency {
            self.push_max_concurrency = v;
        }
        if let Some(v) = overlay.xorb_target_size {
            self.xorb_target_size = v;
        }
        if let Some(v) = overlay.adaptive_xorb_size {
            self.adaptive_xorb_size = v;
        }
        if let Some(v) = overlay.min_xorb_size {
            self.min_xorb_size = v;
        }
        if let Some(v) = overlay.max_xorb_size {
            self.max_xorb_size = v;
        }
        if let Some(v) = overlay.min_run_tail {
            self.min_run_tail = v;
        }
        if let Some(v) = overlay.max_xorb_overshoot_percent {
            self.max_xorb_overshoot_percent = v;
        }
        if let Some(v) = overlay.split_prefer_boundary {
            self.split_prefer_boundary = v;
        }
        if let Some(v) = overlay.defrag_window_size {
            self.defrag_window_size = v;
        }
        if let Some(v) = overlay.min_chunks_per_range {
            self.min_chunks_per_range = v;
        }
    }

    fn apply_receive_overlay(&mut self, overlay: ReceiveOverlay) {
        if let Some(v) = overlay.deny_deletes {
            self.receive_deny_deletes = v;
        }
        if let Some(v) = overlay.deny_non_fast_forwards {
            self.receive_deny_non_fast_forwards = v;
        }
        if let Some(v) = overlay.deny_current_branch {
            self.receive_deny_current_branch = v;
        }
        if let Some(v) = overlay.max_input_size {
            self.receive_max_input_size = v;
        }
        if let Some(v) = overlay.ff_summary_window_commits {
            self.receive_ff_summary_window_commits = v;
        }
    }

    fn apply_uploadpack_overlay(&mut self, overlay: UploadpackOverlay) {
        if let Some(v) = overlay.allow_any_sha_in_want {
            self.uploadpack_allow_any_sha_in_want = v;
        }
        if let Some(v) = overlay.allow_tip_sha_in_want {
            self.uploadpack_allow_tip_sha_in_want = v;
        }
        if let Some(v) = overlay.allow_reachable_sha_in_want {
            self.uploadpack_allow_reachable_sha_in_want = v;
        }
        if let Some(v) = overlay.max_egress_bytes {
            self.uploadpack_max_egress_bytes = v;
        }
    }

    fn apply_transfer_overlay(&mut self, overlay: TransferOverlay) {
        if let Some(v) = overlay.hide_refs {
            self.transfer_hide_refs = v;
        }
    }

    fn apply_fetch_overlay(&mut self, overlay: FetchOverlay) {
        if let Some(v) = overlay.ref_filtering {
            self.fetch_ref_filtering = v;
        }
        if let Some(v) = overlay.object_level_filtering {
            self.fetch_object_level_filtering = v;
        }
    }

    fn apply_remote_overlay(&mut self, overlay: RemoteOverlay) {
        if let Some(v) = overlay.url {
            self.remote_url = Some(v);
        }
        if let Some(v) = overlay.region {
            self.remote_region = Some(v);
        }
    }

    fn apply_cache_overlay(&mut self, overlay: CacheOverlay) {
        if let Some(url) = overlay.service_url {
            self.cache.service_url = Some(url);
        }
        if let Some(mode_str) = overlay.service_mode
            && let Ok(mode) = mode_str.parse()
        {
            self.cache.service_mode = mode;
        }
        if let Some(pw) = overlay.push_warming {
            self.cache.push_warming = pw;
        }
        if let Some(dir) = overlay.chunk_cache_dir {
            self.cache.chunk_cache_dir = Some(PathBuf::from(dir));
        }
        if let Some(ca) = overlay.service_ca_cert {
            self.cache.service_ca_cert = Some(PathBuf::from(ca));
        }
        if let Some(cert) = overlay.service_client_cert {
            self.cache.service_client_cert = Some(PathBuf::from(cert));
        }
        if let Some(key) = overlay.service_client_key {
            self.cache.service_client_key = Some(PathBuf::from(key));
        }

        // Resolve service_auth from TOML fields. Env var overrides happen
        // later in apply_env_overrides() which takes final priority.
        let auth_mode = overlay.service_auth.as_deref().unwrap_or("none");
        match auth_mode {
            "psk" => {
                if let Some(key) = overlay.service_psk {
                    self.cache.service_auth = ServiceAuth::Psk(key);
                }
            }
            "bearer" => {
                // Read token from file at config resolution time.
                let token =
                    overlay.service_token_path.and_then(|path| {
                        match std::fs::read_to_string(&path) {
                            Ok(contents) => {
                                let trimmed = contents.trim().to_string();
                                if trimmed.is_empty() {
                                    tracing::warn!(
                                        path = %path,
                                        "cache service token file is empty"
                                    );
                                    None
                                } else {
                                    Some(trimmed)
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %path,
                                    error = %e,
                                    "failed to read cache service token file"
                                );
                                None
                            }
                        }
                    });
                if let Some(t) = token {
                    self.cache.service_auth = ServiceAuth::Bearer(t);
                }
            }
            "mtls" => {
                self.cache.service_auth = ServiceAuth::Mtls;
            }
            "none" => {
                self.cache.service_auth = ServiceAuth::None;
            }
            other => {
                tracing::warn!(
                    value = %other,
                    "unrecognized cache service_auth mode, defaulting to none"
                );
            }
        }
    }

    fn apply_shard_overlay(&mut self, overlay: ShardOverlay) {
        if let Some(v) = overlay.chunk_index_table_max_size {
            self.shard_chunk_index_table_max_size = v;
        }
    }

    fn apply_auth_overlay(&mut self, overlay: AuthOverlay) {
        if let Some(v) = overlay.provider {
            self.auth.provider = v;
        }
        if let Some(v) = overlay.storage_provider {
            self.auth.storage_provider = v;
        }
        if let Some(v) = overlay.issuer_url {
            self.auth.issuer_url = Some(v);
        }
        if let Some(v) = overlay.client_id {
            self.auth.client_id = Some(v);
        }
        if let Some(v) = overlay.auth_endpoint {
            self.auth.auth_endpoint = Some(v);
        }
        if let Some(v) = overlay.scopes {
            self.auth.scopes = v;
        }
        if let Some(v) = overlay.token_cache_path {
            self.auth.token_cache_path = v;
        }
        if let Some(aws) = overlay.aws {
            if let Some(v) = aws.role_arn {
                self.auth.aws.role_arn = Some(v);
            }
            if let Some(v) = aws.region {
                self.auth.aws.region = Some(v);
            }
            if let Some(v) = aws.session_duration_secs {
                self.auth.aws.session_duration_secs = v;
            }
            self.auth.aws.clamp_session_duration();
        }
        if let Some(gcp) = overlay.gcp {
            if let Some(v) = gcp.workload_identity_pool {
                self.auth.gcp.workload_identity_pool = Some(v);
            }
            if let Some(v) = gcp.service_account {
                self.auth.gcp.service_account = Some(v);
            }
            if let Some(v) = gcp.project_id {
                self.auth.gcp.project_id = Some(v);
            }
        }
        if let Some(azure) = overlay.azure {
            if let Some(v) = azure.tenant_id {
                self.auth.azure.tenant_id = Some(v);
            }
            if let Some(v) = azure.subscription_id {
                self.auth.azure.subscription_id = Some(v);
            }
            if let Some(v) = azure.storage_account {
                self.auth.azure.storage_account = Some(v);
            }
        }
    }

    fn apply_workflow_overlay(&mut self, overlay: WorkflowOverlay) {
        if let Some(v) = overlay.enabled {
            self.workflow.enabled = v;
        }
        if let Some(v) = overlay.discover {
            self.workflow.discover = v;
        }
        if let Some(v) = overlay.lockfile {
            self.workflow.lockfile = v;
        }
        if let Some(v) = overlay.parallelism {
            self.workflow.parallelism = v;
        }
        if let Some(v) = overlay.graceful_shutdown_timeout_secs {
            self.workflow.graceful_shutdown_timeout_secs = v;
        }
        if let Some(v) = overlay.max_outs_per_stage {
            self.workflow.max_outs_per_stage = v;
        }
        if let Some(v) = overlay.max_out_bytes {
            self.workflow.max_out_bytes = v;
        }
        if let Some(v) = overlay.lock_timeout_secs {
            self.workflow.lock_timeout_secs = v;
        }
        if let Some(v) = overlay.remote_cache_readonly {
            self.workflow.remote_cache_readonly = v;
        }
        if let Some(v) = overlay.remotes {
            self.workflow.remotes = v;
        }
    }

    fn apply_tier_overlay(&mut self, overlay: TierOverlay) {
        if let Some(v) = overlay.enabled {
            self.tier.enabled = v;
        }
        if let Some(v) = overlay.to_ia_days {
            self.tier.to_ia_days = v;
        }
        if let Some(v) = overlay.to_deep_days {
            self.tier.to_deep_days = v;
        }
        if let Some(v) = overlay.noncurrent_days {
            self.tier.noncurrent_days = v;
        }
        if let Some(v) = overlay.restore_tier {
            self.tier.restore_tier = v;
        }
        if let Some(v) = overlay.restore_duration_days {
            self.tier.restore_duration_days = v;
        }
        if let Some(v) = overlay.restore_max_concurrency {
            self.tier.restore_max_concurrency = v;
        }
        if let Some(v) = overlay.restore_timeout_secs {
            self.tier.restore_timeout_secs = v;
        }
        if let Some(v) = overlay.restripe_output_class {
            self.tier.restripe_output_class = v;
        }
    }

    fn apply_cost_overlay(&mut self, overlay: CostOverlay) {
        if let Some(v) = overlay.inventory_source {
            self.cost.inventory_source = v;
        }
        if let Some(v) = overlay.list_concurrency {
            self.cost.list_concurrency = v;
        }
        if let Some(v) = overlay.sample_ratio {
            self.cost.sample_ratio = v;
        }
        if let Some(v) = overlay.pricing_file {
            self.cost.pricing_file = v;
        }
        if let Some(v) = overlay.price_table_version {
            self.cost.price_table_version = v;
        }
        if let Some(v) = overlay.access_window_days {
            self.cost.access_window_days = v;
        }
        if let Some(v) = overlay.apply_free_tier {
            self.cost.apply_free_tier = v;
        }
        if let Some(v) = overlay.report_max_staleness_hours {
            self.cost.report_max_staleness_hours = v;
        }
    }

    fn apply_restripe_overlay(&mut self, overlay: RestripeOverlay) {
        if let Some(profiles) = overlay.profiles {
            self.restripe.profiles = profiles;
        }
    }

    fn apply_gc_overlay(&mut self, overlay: GcOverlay) {
        if let Some(v) = overlay.class_aware {
            self.gc.class_aware = v;
        }
    }

    /// Resolve the effective directory for xet-core's chunk `DiskCache`.
    ///
    /// Uses [`CacheConfig::chunk_cache_dir`] when set, otherwise falls
    /// back to `{default_cache_root}/chunks/`. Called by the hydrate,
    /// smudge, and prefetch paths so every consumer of the xet-core
    /// cache agrees on the directory.
    #[must_use]
    pub fn effective_chunk_cache_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.cache.chunk_cache_dir {
            return dir.clone();
        }
        crate::cache::default_cache_root().join("chunks")
    }

    /// Build a [`crate::metadata::MetaDbConfig`] for a session
    /// anchored at `repo_prefix`, layering the `[metadb]` TOML
    /// section on top of the derived defaults.
    ///
    /// Paths fall through to [`crate::metadata::MetaDbConfig::for_repo`]
    /// when the TOML does not override them. The three
    /// single-valued SlateDB tunables (`compaction_threshold`,
    /// `wal_flush_size`, `bloom_bits_per_key`) currently live on the
    /// flat [`crate::metadata::MetaDbConfig`] and are applied from
    /// `metadb.file_index` first, falling back to
    /// `metadb.chunk_index` — the `metadb.chunk_index.*` tunables in
    /// the TOML are reserved for a future split of these fields into
    /// per-database form and are accepted today so operators can
    /// stage their config ahead of the split. The `local_path`,
    /// `in_memory_ceiling_bytes`, and `cache_gc_grace` knobs under
    /// `metadb.chunk_index` are wired through directly.
    #[must_use]
    pub fn build_metadb_config(&self, repo_prefix: &str) -> crate::metadata::MetaDbConfig {
        build_metadb_config_from(repo_prefix, &self.metadb)
    }

    /// Return the heartbeat interval clamped to `10..=(ttl_secs - 10)`.
    ///
    /// If `ttl_secs` is too small for a valid range (≤ 20), returns `None`
    /// because no heartbeat interval can satisfy the constraint.
    #[must_use]
    pub fn clamped_heartbeat_interval(&self, ttl_secs: u64) -> Option<Duration> {
        if ttl_secs <= 20 {
            return None;
        }
        let max = ttl_secs.saturating_sub(10);
        let clamped = self.push_lock_heartbeat_interval.clamp(10, max);
        Some(Duration::from_secs(clamped))
    }

    /// Build the compression policy determined by the config.
    ///
    /// When `compression_adaptive` is `true`, returns an [`AdaptiveCompression`]
    /// policy that uses BG4 byte-grouping and entropy probing. When `false`,
    /// returns a [`FixedCompression`] policy using the configured algorithm.
    #[must_use]
    pub fn compression_policy(&self) -> Box<dyn crab_xet::xorb::builder::CompressionPolicy> {
        use crab_xet::xorb::builder::{AdaptiveCompression, FixedCompression};
        use crab_xet::xorb::format::CompressionScheme;

        if self.compression_adaptive {
            Box::new(AdaptiveCompression::default())
        } else {
            let scheme = match self.compression {
                CompressionConfig::None => CompressionScheme::None,
                CompressionConfig::Zstd { .. } | CompressionConfig::Lz4 => CompressionScheme::LZ4,
            };
            Box::new(FixedCompression::new(scheme))
        }
    }
}

#[cfg(test)]
fn project_config_path_from_repo_config(repo_path: &Path) -> Option<PathBuf> {
    let crab_dir = repo_path.parent()?;
    let repo_root = crab_dir.parent()?;
    Some(repo_root.join(".crab.toml"))
}

/// Build a [`crate::metadata::MetaDbConfig`] for a session anchored
/// at `repo_prefix` by layering `metadb_toml` on top of the
/// [`crate::metadata::MetaDbConfig::for_repo`] defaults.
///
/// Exposed so call sites that only hold the `[metadb]` section (e.g.
/// the push pipeline's `PushConfig::metadb`) can reach the same
/// merging logic as [`Config::build_metadb_config`] without building
/// a throwaway full [`Config`].
pub fn build_metadb_config_from(
    repo_prefix: &str,
    metadb_toml: &MetaDbTomlConfig,
) -> crate::metadata::MetaDbConfig {
    let mut cfg = crate::metadata::MetaDbConfig::for_repo(repo_prefix);

    // Per-DB path overrides
    if let Some(ref p) = metadb_toml.file_index.path {
        cfg.file_index_path.clone_from(p);
    }
    if let Some(ref p) = metadb_toml.chunk_index.db.path {
        cfg.chunk_index_path.clone_from(p);
    }

    // Local chunk-index cache knobs
    if let Some(ref p) = metadb_toml.chunk_index.local_path {
        cfg.local_chunk_index_path.clone_from(p);
    }
    if let Some(v) = metadb_toml.chunk_index.in_memory_ceiling_bytes {
        cfg.in_memory_ceiling_bytes = v;
    }
    if let Some(v) = metadb_toml.chunk_index.cache_gc_grace {
        cfg.cache_gc_grace = v;
    }

    // Shared single-valued SlateDB tunables. file_index wins;
    // chunk_index fills in any gap. When both are absent the struct
    // default stands. Keeping the chunk_index values in the TOML
    // surface lets operators declare them now so the future per-DB
    // split is a no-op migration for the config file.
    if let Some(v) = metadb_toml
        .file_index
        .compaction_threshold
        .or(metadb_toml.chunk_index.db.compaction_threshold)
    {
        cfg.compaction_threshold = v;
    }
    if let Some(v) = metadb_toml
        .file_index
        .wal_flush_size
        .or(metadb_toml.chunk_index.db.wal_flush_size)
    {
        cfg.wal_flush_size = v;
    }
    if let Some(v) = metadb_toml
        .file_index
        .bloom_bits_per_key
        .or(metadb_toml.chunk_index.db.bloom_bits_per_key)
    {
        cfg.bloom_bits_per_key = v;
    }

    cfg
}

/// Read a TOML file and deserialize into a [`ConfigOverlay`].
fn read_toml_overlay(path: &Path) -> super::error::Result<ConfigOverlay> {
    let content =
        std::fs::read_to_string(path).map_err(|e| super::error::CrabError::Configuration {
            key: format!("failed to read file: {e}"),
            origin: path.display().to_string(),
        })?;
    toml::from_str(&content).map_err(|e| super::error::CrabError::Configuration {
        key: e.to_string(),
        origin: path.display().to_string(),
    })
}

/// Parse a compression string like `"none"` or `"zstd(3)"` into a
/// [`CompressionConfig`].
fn parse_compression(s: &str, origin: &str) -> super::error::Result<CompressionConfig> {
    match s {
        "none" => Ok(CompressionConfig::None),
        "lz4" => Ok(CompressionConfig::Lz4),
        other => {
            // Accept "zstd" (default level) or "zstd(N)".
            if other == "zstd" {
                return Ok(CompressionConfig::default());
            }
            if let Some(inner) = other
                .strip_prefix("zstd(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let level: i32 =
                    inner
                        .parse()
                        .map_err(|_| super::error::CrabError::Configuration {
                            key: format!("compression: invalid zstd level \"{inner}\""),
                            origin: origin.to_string(),
                        })?;
                return Ok(CompressionConfig::Zstd { level });
            }
            Err(super::error::CrabError::Configuration {
                key: format!("compression: unrecognized value \"{other}\""),
                origin: origin.to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Engine (perf) configuration
// ---------------------------------------------------------------------------

/// 1 MiB — minimum file size for the fast-path optimization.
const DEFAULT_FASTPATH_MIN_SIZE: u64 = 1024 * 1024;

/// Engine tuning knobs from the `[perf]` configuration section.
///
/// When `enabled` is `false`, all optimizations are disabled regardless of
/// individual field values — the system falls back to v1 behavior.
#[expect(
    clippy::struct_excessive_bools,
    reason = "config struct mirrors the TOML section; each bool is an independent feature gate"
)]
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Master switch for all perf optimizations.
    pub enabled: bool,
    /// Attach bloom filters to shard footers for faster lookups.
    pub shard_bloom: bool,
    /// Include shard-hint in pointer blobs so smudge can skip shard scans.
    pub pointer_shard_hint: bool,
    /// Compress chunks in the staging area (zstd) before xorb packing.
    pub compress_staging: bool,
    /// Use EWMA-based adaptive dedup threshold instead of the fixed 0.25.
    pub adaptive_threshold: bool,
    /// Minimum file size (bytes) eligible for the zero-copy fast path.
    pub fastpath_min_size: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shard_bloom: true,
            pointer_shard_hint: true,
            compress_staging: true,
            adaptive_threshold: true,
            fastpath_min_size: DEFAULT_FASTPATH_MIN_SIZE,
        }
    }
}

impl EngineConfig {
    /// Returns `true` when shard bloom filters should be used.
    #[must_use]
    pub fn shard_bloom_active(&self) -> bool {
        self.enabled && self.shard_bloom
    }

    /// Whether staging compression is active.
    #[must_use]
    pub fn compress_staging_active(&self) -> bool {
        self.enabled && self.compress_staging
    }

    /// Whether the adaptive dedup threshold is active.
    #[must_use]
    pub fn adaptive_threshold_active(&self) -> bool {
        self.enabled && self.adaptive_threshold
    }

    /// Whether the pointer shard hint optimization is active.
    #[must_use]
    pub fn pointer_shard_hint_active(&self) -> bool {
        self.enabled && self.pointer_shard_hint
    }

    /// Effective fast-path minimum size, or `u64::MAX` when perf is disabled
    /// (effectively disabling the fast path).
    #[must_use]
    pub fn effective_fastpath_min_size(&self) -> u64 {
        if self.enabled {
            self.fastpath_min_size
        } else {
            u64::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git<I, S>(cwd: &Path, args: I) -> Option<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
    }

    #[test]
    fn defaults_enable_all_optimizations() {
        let cfg = EngineConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.shard_bloom_active());
        assert!(cfg.compress_staging_active());
        assert!(cfg.adaptive_threshold_active());
        assert!(cfg.pointer_shard_hint_active());
        assert_eq!(cfg.fastpath_min_size, 1024 * 1024);
    }

    #[test]
    fn retired_git_fsck_switches_are_rejected() {
        for input in [
            "[receive]\nfsckObjects = false\n",
            "[receive]\nfsck_objects = false\n",
            "[uploadpack]\nfsckObjects = true\n",
            "[uploadpack]\nfsck_objects = true\n",
        ] {
            let error = toml::from_str::<ConfigOverlay>(input)
                .expect_err("retired semantic-validation switches must fail closed");
            assert!(error.to_string().contains("unknown field"));
        }
    }

    #[test]
    fn master_switch_disables_all() {
        let cfg = EngineConfig {
            enabled: false,
            ..EngineConfig::default()
        };
        assert!(!cfg.shard_bloom_active());
        assert!(!cfg.compress_staging_active());
        assert!(!cfg.adaptive_threshold_active());
        assert!(!cfg.pointer_shard_hint_active());
        assert_eq!(cfg.effective_fastpath_min_size(), u64::MAX);
    }

    #[test]
    fn individual_flags_respected_when_enabled() {
        let cfg = EngineConfig {
            enabled: true,
            shard_bloom: false,
            ..EngineConfig::default()
        };
        assert!(!cfg.shard_bloom_active());
        assert!(cfg.compress_staging_active());
    }

    // --- Config defaults ---

    #[test]
    fn config_default_has_sensible_values() {
        let cfg = Config::default();
        assert_eq!(cfg.chunk_threshold_bytes, 64 * 1024);
        assert!(cfg.xet_enabled);
        assert_eq!(cfg.xet_prefix, "xet");
        assert_eq!(cfg.compression, CompressionConfig::Zstd { level: 3 });
        assert!(cfg.compression_adaptive);
        assert_eq!(cfg.default_branch, "main");
        assert!(cfg.required_cli_version.is_none());
        assert_eq!(cfg.upload_concurrency, 8);
        assert_eq!(cfg.download_concurrency, 8);
        assert_eq!(cfg.operation_timeout, Duration::from_secs(300));
        assert_eq!(cfg.max_retries, 5);
        assert_eq!(cfg.chunk_cache_bytes, 256 * 1024 * 1024);
        assert!(cfg.shard_cache_bytes.is_none());
        assert_eq!(cfg.shard_chunk_index_table_max_size, 64 * 1024 * 1024);
        assert_eq!(cfg.gc_grace_period, Duration::from_secs(24 * 60 * 60));
        assert_eq!(cfg.gc_delete_concurrency, 64);
        assert_eq!(cfg.gc_list_concurrency, 32);
        // Transport-gaps config defaults
        assert_eq!(cfg.repack_auto_threshold, 50);
        assert!(!cfg.push_thin_packs);
        assert_eq!(cfg.push_lock_heartbeat_interval, 100);
        assert_eq!(cfg.push_lock_ttl_secs, 300);
        assert_eq!(cfg.push_lock_wait_secs, 0);
        assert_eq!(cfg.push_max_cas_retries, 64);
        assert_eq!(cfg.xorb_target_size, 64 * 1024 * 1024);
        assert!(!cfg.adaptive_xorb_size);
        assert_eq!(cfg.min_xorb_size, 16 * 1024 * 1024);
        assert_eq!(cfg.max_xorb_size, 256 * 1024 * 1024);
        assert!(!cfg.fetch_ref_filtering);
        assert!(!cfg.fetch_object_level_filtering);
        // Split parameter defaults
        assert_eq!(cfg.min_run_tail, 1024 * 1024);
        assert!((cfg.max_xorb_overshoot_percent - 0.10).abs() < f64::EPSILON);
        assert_eq!(cfg.split_prefer_boundary, 2 * 1024 * 1024);
        assert_eq!(cfg.defrag_window_size, 10);
        assert!((cfg.min_chunks_per_range - 16.0).abs() < f64::EPSILON);
        // Remote, perf persistence defaults
        assert!(cfg.remote_url.is_none());
        assert!(cfg.remote_region.is_none());
        assert!(cfg.perf_persist);
        assert_eq!(cfg.perf_path, ".crab/perf-state.json");
    }

    #[test]
    fn config_embeds_staging_and_perf_defaults() {
        let cfg = Config::default();
        let staging = StagingConfig::default();
        let perf = EngineConfig::default();
        assert_eq!(
            cfg.staging.segment_target_bytes,
            staging.segment_target_bytes
        );
        assert_eq!(cfg.perf.enabled, perf.enabled);
    }

    // --- CompressionConfig ---

    #[test]
    fn compression_config_default_is_zstd_3() {
        assert_eq!(
            CompressionConfig::default(),
            CompressionConfig::Zstd { level: 3 }
        );
    }

    #[test]
    fn compression_config_display() {
        assert_eq!(CompressionConfig::None.to_string(), "none");
        assert_eq!(CompressionConfig::Zstd { level: 3 }.to_string(), "zstd(3)");
        assert_eq!(
            CompressionConfig::Zstd { level: 19 }.to_string(),
            "zstd(19)"
        );
        assert_eq!(CompressionConfig::Lz4.to_string(), "lz4");
    }

    // --- Config overlay and resolve ---

    #[test]
    fn resolve_local_with_no_files_returns_defaults() {
        let cfg = Config::resolve_local_from(None, PathBuf::from("/nonexistent"))
            .expect("should succeed with no config files");
        assert_eq!(cfg.chunk_threshold_bytes, DEFAULT_CHUNK_THRESHOLD_BYTES);
        assert!(cfg.xet_enabled);
        assert_eq!(cfg.upload_concurrency, DEFAULT_UPLOAD_CONCURRENCY);
    }

    #[test]
    fn resolve_local_applies_user_toml() {
        let dir = tempfile::tempdir().unwrap();
        let user_toml = dir.path().join("config.toml");
        std::fs::write(
            &user_toml,
            "upload_concurrency = 16\ndefault_branch = \"develop\"\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(user_toml), PathBuf::from("/nonexistent"))
            .expect("should parse user TOML");
        assert_eq!(cfg.upload_concurrency, 16);
        assert_eq!(cfg.default_branch, "develop");
        assert_eq!(cfg.download_concurrency, DEFAULT_DOWNLOAD_CONCURRENCY);
    }

    #[test]
    fn resolve_local_repo_overrides_user() {
        let dir = tempfile::tempdir().unwrap();
        let user_toml = dir.path().join("user.toml");
        std::fs::write(&user_toml, "upload_concurrency = 16\nmax_retries = 10\n").unwrap();

        let repo_toml = dir.path().join("repo.toml");
        std::fs::write(&repo_toml, "upload_concurrency = 4\n").unwrap();

        let cfg =
            Config::resolve_local_from(Some(user_toml), repo_toml).expect("should merge layers");
        assert_eq!(cfg.upload_concurrency, 4);
        assert_eq!(cfg.max_retries, 10);
    }

    #[test]
    fn resolve_remote_merges_json_layer() {
        let cfg = Config::default();
        let json = br#"{"chunk_threshold_bytes":128000,"xet_enabled":false,"required_cli_version":">=0.2.0"}"#;
        let resolved = cfg.resolve_remote(json).expect("should parse remote JSON");
        assert_eq!(resolved.chunk_threshold_bytes, 128_000);
        assert!(!resolved.xet_enabled);
        assert!(resolved.required_cli_version.is_some());
    }

    #[test]
    fn resolve_remote_empty_is_noop() {
        let cfg = Config::default();
        let resolved = cfg.resolve_remote(b"").expect("empty should succeed");
        assert_eq!(
            resolved.chunk_threshold_bytes,
            DEFAULT_CHUNK_THRESHOLD_BYTES
        );
    }

    #[test]
    fn overlay_staging_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[staging]\nsegment_target_bytes = 1048576\nauto_compact = true\nbatch_read_size = 512\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse staging section");
        assert_eq!(cfg.staging.segment_target_bytes, 1_048_576);
        assert!(cfg.staging.auto_compact);
        assert_eq!(
            cfg.staging.fd_pool_size,
            StagingConfig::default().fd_pool_size
        );
        assert_eq!(cfg.staging.batch_read_size, 512);
    }

    #[test]
    fn overlay_perf_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[perf]\nenabled = false\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse perf section");
        assert!(!cfg.perf.enabled);
        assert!(cfg.perf.shard_bloom);
    }

    #[test]
    fn overlay_checkout_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[checkout]\nlazy = true\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse checkout section");
        assert!(cfg.checkout.lazy);
    }

    #[test]
    fn overlay_hydrate_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[hydrate]\ninclude = [\"models/**\", \"data/**\"]\nexclude = [\"data/archive/**\"]\nauto = true\ndownload_concurrency = 32\nprefetch_budget = 2147483648\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse hydrate section");
        assert_eq!(cfg.hydrate.include, vec!["models/**", "data/**"]);
        assert_eq!(cfg.hydrate.exclude, vec!["data/archive/**"]);
        assert!(cfg.hydrate.auto);
        assert_eq!(cfg.hydrate.download_concurrency, 32);
        assert_eq!(cfg.hydrate.prefetch_budget, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn overlay_repack_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[repack]\nauto_threshold = 100\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse repack section");
        assert_eq!(cfg.repack_auto_threshold, 100);
    }

    #[test]
    fn overlay_push_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[push]\nthin_packs = true\nlock_heartbeat_interval = 60\nlock_ttl_secs = 600\nlock_wait_secs = 30\nmax_cas_retries = 128\nupload_concurrency = 32\nxorb_target_size = 134217728\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse push section");
        assert!(cfg.push_thin_packs);
        assert_eq!(cfg.push_lock_heartbeat_interval, 60);
        assert_eq!(cfg.push_lock_ttl_secs, 600);
        assert_eq!(cfg.push_lock_wait_secs, 30);
        assert_eq!(cfg.push_max_cas_retries, 128);
        assert_eq!(cfg.upload_concurrency, 32);
        assert_eq!(cfg.xorb_target_size, 134_217_728);
    }

    #[test]
    fn overlay_push_split_parameters() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[push]\nmin_run_tail = 2097152\nmax_xorb_overshoot_percent = 0.15\nsplit_prefer_boundary = 4194304\ndefrag_window_size = 20\nmin_chunks_per_range = 8.0\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse push split parameters");
        assert_eq!(cfg.min_run_tail, 2_097_152);
        assert!((cfg.max_xorb_overshoot_percent - 0.15).abs() < f64::EPSILON);
        assert_eq!(cfg.split_prefer_boundary, 4_194_304);
        assert_eq!(cfg.defrag_window_size, 20);
        assert!((cfg.min_chunks_per_range - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn overlay_push_adaptive_xorb_sizing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[push]\nadaptive_xorb_size = true\nmin_xorb_size = 33554432\nmax_xorb_size = 134217728\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse push adaptive xorb sizing");
        assert!(cfg.adaptive_xorb_size);
        assert_eq!(cfg.min_xorb_size, 32 * 1024 * 1024);
        assert_eq!(cfg.max_xorb_size, 128 * 1024 * 1024);
    }

    #[test]
    fn invalid_push_xorb_sizing_is_rejected() {
        let cases = [
            ("xorb_target_size = 0", "xorb_target_size"),
            (
                "xorb_target_size = 8388608\nmin_xorb_size = 16777216\nmax_xorb_size = 268435456",
                "xorb_target_size",
            ),
            (
                "xorb_target_size = 67108864\nmin_xorb_size = 134217728\nmax_xorb_size = 67108864",
                "max_xorb_size",
            ),
            (
                "xorb_target_size = 67108864\nmin_xorb_size = 0\nmax_xorb_size = 268435456",
                "min_xorb_size",
            ),
            (
                "xorb_target_size = 67108864\nmin_xorb_size = 16777216\nmax_xorb_size = 4294967296",
                "max_xorb_size",
            ),
            (
                "xorb_target_size = 4294967295\nmin_xorb_size = 16777216\nmax_xorb_size = 4294967295\nmax_xorb_overshoot_percent = 0.01",
                "max_xorb_overshoot_percent",
            ),
        ];

        for (toml, expected_key) in cases {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, format!("[push]\n{toml}\n")).unwrap();

            let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
                .expect_err("invalid push xorb sizing should fail");
            match err {
                crate::core::CrabError::Configuration { key, origin } => {
                    assert_eq!(origin, "push");
                    assert!(
                        key.contains(expected_key),
                        "expected key containing {expected_key}, got {key}"
                    );
                }
                other => panic!("expected Configuration, got {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_push_split_tuning_is_rejected_from_remote_config() {
        let cases = [
            (
                r#"{"push":{"head_check_concurrency":0}}"#,
                "head_check_concurrency",
            ),
            (r#"{"push":{"min_concurrency":0}}"#, "min_concurrency"),
            (
                r#"{"push":{"min_concurrency":8,"max_concurrency":4}}"#,
                "max_concurrency",
            ),
            (
                r#"{"push":{"max_xorb_overshoot_percent":-0.1}}"#,
                "max_xorb_overshoot_percent",
            ),
            (r#"{"push":{"defrag_window_size":0}}"#, "defrag_window_size"),
            (
                r#"{"push":{"min_chunks_per_range":0.0}}"#,
                "min_chunks_per_range",
            ),
        ];

        for (json, expected_key) in cases {
            let err = Config::default()
                .resolve_remote(json.as_bytes())
                .expect_err("invalid remote push tuning should fail");
            match err {
                crate::core::CrabError::Configuration { key, origin } => {
                    assert_eq!(origin, "push");
                    assert!(
                        key.contains(expected_key),
                        "expected key containing {expected_key}, got {key}"
                    );
                }
                other => panic!("expected Configuration, got {other:?}"),
            }
        }
    }

    #[test]
    fn overlay_fetch_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[fetch]\nref_filtering = true\nobject_level_filtering = true\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse fetch section");
        assert!(cfg.fetch_ref_filtering);
        assert!(cfg.fetch_object_level_filtering);
    }

    #[test]
    fn clamped_heartbeat_interval_within_range() {
        let mut cfg = Config::default();
        cfg.push_lock_heartbeat_interval = 100;
        // TTL = 300s → valid range 10..=290
        let interval = cfg.clamped_heartbeat_interval(300).unwrap();
        assert_eq!(interval, Duration::from_secs(100));
    }

    #[test]
    fn clamped_heartbeat_interval_clamps_low() {
        let mut cfg = Config::default();
        cfg.push_lock_heartbeat_interval = 3;
        let interval = cfg.clamped_heartbeat_interval(300).unwrap();
        assert_eq!(interval, Duration::from_secs(10));
    }

    #[test]
    fn clamped_heartbeat_interval_clamps_high() {
        let mut cfg = Config::default();
        cfg.push_lock_heartbeat_interval = 500;
        // TTL = 300s → max = 290
        let interval = cfg.clamped_heartbeat_interval(300).unwrap();
        assert_eq!(interval, Duration::from_secs(290));
    }

    #[test]
    fn clamped_heartbeat_interval_tiny_ttl_returns_none() {
        let cfg = Config::default();
        // TTL = 20s → no valid range
        assert!(cfg.clamped_heartbeat_interval(20).is_none());
        assert!(cfg.clamped_heartbeat_interval(15).is_none());
    }

    #[test]
    fn checkout_and_hydrate_default_values() {
        let cfg = Config::default();
        assert!(!cfg.checkout.lazy);
        assert!(cfg.hydrate.include.is_empty());
        assert!(cfg.hydrate.exclude.is_empty());
        assert!(!cfg.hydrate.auto);
        assert_eq!(cfg.hydrate.download_concurrency, 4);
        assert_eq!(cfg.hydrate.prefetch_budget, 384 * 1024 * 1024);
    }

    #[test]
    fn malformed_toml_returns_configuration_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "not valid toml [[[").unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("should fail on malformed TOML");
        assert!(matches!(err, crate::core::CrabError::Configuration { .. }));
    }

    #[test]
    fn malformed_remote_json_returns_configuration_error() {
        let err = Config::default()
            .resolve_remote(b"{bad json")
            .expect_err("should fail on malformed JSON");
        assert!(matches!(err, crate::core::CrabError::Configuration { .. }));
    }

    #[test]
    fn unknown_toml_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "bogus_key = 42\n").unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("unknown keys should be rejected");
        assert!(matches!(err, crate::core::CrabError::Configuration { .. }));
    }

    #[test]
    fn invalid_compression_string_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "compression = \"brotli\"\n").unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("invalid compression should fail");
        assert!(matches!(err, crate::core::CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_compression_variants() {
        assert_eq!(
            parse_compression("none", "t").unwrap(),
            CompressionConfig::None
        );
        assert_eq!(
            parse_compression("zstd", "t").unwrap(),
            CompressionConfig::Zstd { level: 3 }
        );
        assert_eq!(
            parse_compression("zstd(19)", "t").unwrap(),
            CompressionConfig::Zstd { level: 19 }
        );
        assert_eq!(
            parse_compression("lz4", "t").unwrap(),
            CompressionConfig::Lz4
        );
        assert!(parse_compression("brotli", "t").is_err());
        assert!(parse_compression("zstd(abc)", "t").is_err());
    }

    #[test]
    fn compression_adaptive_defaults_to_true() {
        let cfg = Config::default();
        assert!(cfg.compression_adaptive);
    }

    #[test]
    fn compression_adaptive_overlay_disables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "compression_adaptive = false\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse compression_adaptive");
        assert!(!cfg.compression_adaptive);
    }

    #[test]
    fn compression_policy_adaptive_returns_adaptive() {
        let cfg = Config {
            compression_adaptive: true,
            ..Config::default()
        };
        // Adaptive policy should handle BG4 + entropy probing.
        // We verify it doesn't panic and returns a valid policy.
        let _policy = cfg.compression_policy();
    }

    #[test]
    fn compression_policy_fixed_returns_fixed() {
        let cfg = Config {
            compression_adaptive: false,
            ..Config::default()
        };
        let _policy = cfg.compression_policy();
    }

    #[test]
    fn operation_timeout_from_secs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "operation_timeout_secs = 60\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse timeout");
        assert_eq!(cfg.operation_timeout, Duration::from_secs(60));
    }

    #[test]
    fn full_four_layer_precedence() {
        let dir = tempfile::tempdir().unwrap();

        let user = dir.path().join("user.toml");
        std::fs::write(
            &user,
            "max_retries = 10\nupload_concurrency = 16\ndownload_concurrency = 32\n",
        )
        .unwrap();

        let repo = dir.path().join("repo.toml");
        std::fs::write(&repo, "upload_concurrency = 2\n").unwrap();

        let local = Config::resolve_local_from(Some(user), repo).expect("should merge");
        assert_eq!(local.upload_concurrency, 2);
        assert_eq!(local.max_retries, 10);
        assert_eq!(local.download_concurrency, 32);
        assert_eq!(local.chunk_threshold_bytes, DEFAULT_CHUNK_THRESHOLD_BYTES);

        let final_cfg = local
            .resolve_remote(br#"{"max_retries":3,"chunk_threshold_bytes":999}"#)
            .expect("should merge remote");
        assert_eq!(final_cfg.max_retries, 3);
        assert_eq!(final_cfg.chunk_threshold_bytes, 999);
        assert_eq!(final_cfg.upload_concurrency, 2);
        assert_eq!(final_cfg.download_concurrency, 32);
    }

    // --- Version guard ---

    #[test]
    fn version_guard_passes_when_no_requirement() {
        let cfg = Config::default();
        assert!(cfg.required_cli_version.is_none());
        cfg.check_version_guard()
            .expect("no requirement should pass");
    }

    #[test]
    fn version_guard_passes_when_binary_satisfies_requirement() {
        let mut cfg = Config::default();
        // The binary version is CARGO_PKG_VERSION (0.1.0). A requirement
        // that accepts any 0.1.x should pass.
        cfg.required_cli_version = Some(semver::VersionReq::parse(">=0.1.0").unwrap());
        cfg.check_version_guard().expect("0.1.0 satisfies >=0.1.0");
    }

    #[test]
    fn version_guard_rejects_when_binary_too_old() {
        let mut cfg = Config::default();
        // Require a version higher than the current binary.
        cfg.required_cli_version = Some(semver::VersionReq::parse(">=99.0.0").unwrap());
        let err = cfg
            .check_version_guard()
            .expect_err("should reject old binary");
        assert!(
            matches!(err, crate::core::CrabError::IncompatibleFormat { .. }),
            "expected IncompatibleFormat, got: {err:?}"
        );
    }

    #[test]
    fn version_guard_exact_match() {
        let mut cfg = Config::default();
        let current = Config::BINARY_VERSION;
        let req_str = format!("={current}");
        cfg.required_cli_version = Some(semver::VersionReq::parse(&req_str).unwrap());
        cfg.check_version_guard().expect("exact match should pass");
    }

    #[test]
    fn overlay_remote_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[remote]\nurl = \"crab://my-bucket/my-repo\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse remote section");
        assert_eq!(cfg.remote_url.as_deref(), Some("crab://my-bucket/my-repo"));
    }

    #[test]
    fn resolve_for_repo_uses_shared_config_for_no_checkout_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let linked = dir.path().join("linked");

        let init = std::process::Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .output();
        if init.is_err() || !init.unwrap().status.success() {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        run_git(&repo, ["config", "user.email", "worktree@crab.dev"]);
        run_git(&repo, ["config", "user.name", "crab-worktree"]);
        std::fs::write(repo.join("README.md"), b"initial\n").unwrap();
        run_git(&repo, ["add", "README.md"]);
        let commit = run_git(&repo, ["commit", "-qm", "init"]);
        if commit.is_none_or(|output| !output.status.success()) {
            eprintln!("SKIP: git commit failed");
            return;
        }

        std::fs::create_dir_all(repo.join(".crab")).unwrap();
        std::fs::write(
            repo.join(".crab").join("config.toml"),
            "[remote]\nurl = \"crab://bucket/shared-config\"\n",
        )
        .unwrap();

        let add = run_git(
            &repo,
            [
                "worktree",
                "add",
                "-q",
                "--detach",
                "--no-checkout",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        if add.is_none_or(|output| !output.status.success()) {
            eprintln!("SKIP: git worktree add --no-checkout failed");
            return;
        }

        assert!(!linked.join(".crab.toml").exists());
        let cfg = Config::resolve_for_repo(&linked).expect("linked config");
        assert_eq!(
            cfg.remote_url.as_deref(),
            Some("crab://bucket/shared-config")
        );
    }

    #[test]
    fn project_config_replication_augments_local_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let git_config = git_dir.join("config");
        std::fs::write(&git_config, "").unwrap();
        std::fs::write(
            repo.join(".crab.toml"),
            r#"[remote]
url = "crab://primary-bucket/org/repo"

[replication]
primary = "crab://primary-bucket/org/repo"

[[replication.replicas]]
name = "west"
provider = "s3"
url = "s3://replica-bucket/org/repo"
region = "us-west-2"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(None, git_config)
            .expect("should merge project replication config");
        assert_eq!(
            cfg.remote_url.as_deref(),
            Some("crab://primary-bucket/org/repo")
        );
        let replication = cfg.replication.expect("replication config");
        assert_eq!(
            replication.primary.as_deref(),
            Some("crab://primary-bucket/org/repo")
        );
        assert_eq!(replication.replicas[0].name, "west");
    }

    #[test]
    fn project_config_auth_storage_provider_augments_local_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let git_config = git_dir.join("config");
        std::fs::write(&git_config, "").unwrap();
        std::fs::write(
            repo.join(".crab.toml"),
            r#"[remote]
url = "crab://gcs-bucket/org/repo"

[auth]
storage_provider = "gcs"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(None, git_config)
            .expect("should merge project auth storage provider");
        assert_eq!(cfg.auth.storage_provider, StorageProvider::Gcs);
    }

    #[test]
    fn overlay_perf_persist_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[perf]\npersist = false\npath = \"/tmp/perf.json\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse perf persist/path");
        assert!(!cfg.perf_persist);
        assert_eq!(cfg.perf_path, "/tmp/perf.json");
    }

    #[test]
    fn staging_alias_segment_target_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[staging]\nsegment_target_size = 67108864\ncompaction_dead_ratio = 0.3\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse staging aliases");
        assert_eq!(cfg.staging.segment_target_bytes, 67_108_864);
        assert!((cfg.staging.compact_dead_ratio - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn lz4_compression_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "compression = \"lz4\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse lz4 compression");
        assert_eq!(cfg.compression, CompressionConfig::Lz4);
    }

    #[test]
    fn overlay_cache_chunk_cache_dir_sets_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[cache]\nchunk_cache_dir = \"/var/tmp/crab-chunks\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse cache.chunk_cache_dir");
        assert_eq!(
            cfg.cache.chunk_cache_dir.as_deref(),
            Some(Path::new("/var/tmp/crab-chunks"))
        );
        assert_eq!(
            cfg.effective_chunk_cache_dir(),
            PathBuf::from("/var/tmp/crab-chunks")
        );
    }

    #[test]
    fn effective_chunk_cache_dir_defaults_to_cache_root_chunks() {
        let cfg = Config::default();
        assert!(cfg.cache.chunk_cache_dir.is_none());
        let dir = cfg.effective_chunk_cache_dir();
        assert!(
            dir.ends_with("chunks"),
            "expected default dir to end in 'chunks', got {dir:?}"
        );
    }

    #[test]
    fn overlay_shard_chunk_index_table_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[shard]\nchunk_index_table_max_size = 134217728\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse shard.chunk_index_table_max_size");
        assert_eq!(cfg.shard_chunk_index_table_max_size, 128 * 1024 * 1024);
    }

    // --- Auth config parsing ---

    #[test]
    fn auth_config_defaults_to_static_auto() {
        let cfg = Config::default();
        assert_eq!(cfg.auth.provider, AuthProvider::Static);
        assert_eq!(cfg.auth.storage_provider, StorageProvider::Auto);
        assert!(cfg.auth.issuer_url.is_none());
        assert!(cfg.auth.client_id.is_none());
        assert!(cfg.auth.auth_endpoint.is_none());
        assert_eq!(cfg.auth.scopes, "openid email profile");
        assert_eq!(cfg.auth.token_cache_path, "~/.config/crab/tokens/");
        assert!(cfg.auth.aws.role_arn.is_none());
        assert!(cfg.auth.aws.region.is_none());
        assert_eq!(cfg.auth.aws.session_duration_secs, 3600);
        assert!(cfg.auth.gcp.workload_identity_pool.is_none());
        assert!(cfg.auth.azure.tenant_id.is_none());
    }

    #[test]
    fn auth_overlay_parses_all_provider_variants() {
        let variants = [
            ("aws-oidc", AuthProvider::AwsOidc),
            ("gcp-workload-identity", AuthProvider::GcpWorkloadIdentity),
            ("azure-entra", AuthProvider::AzureEntra),
            ("crab-auth", AuthProvider::CrabAuth),
            ("static", AuthProvider::Static),
            ("none", AuthProvider::None),
        ];
        for (toml_val, expected) in variants {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, format!("[auth]\nprovider = \"{toml_val}\"\n")).unwrap();

            let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
                .unwrap_or_else(|e| panic!("provider '{toml_val}' should parse: {e:?}"));
            assert_eq!(cfg.auth.provider, expected, "mismatch for '{toml_val}'");
        }
    }

    #[test]
    fn auth_overlay_parses_all_storage_provider_variants() {
        let variants = [
            ("s3", StorageProvider::S3),
            ("gcs", StorageProvider::Gcs),
            ("azure", StorageProvider::Azure),
            ("auto", StorageProvider::Auto),
        ];
        for (toml_val, expected) in variants {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("config.toml");
            std::fs::write(&p, format!("[auth]\nstorage_provider = \"{toml_val}\"\n")).unwrap();

            let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
                .unwrap_or_else(|e| panic!("storage_provider '{toml_val}' should parse: {e:?}"));
            assert_eq!(
                cfg.auth.storage_provider, expected,
                "mismatch for '{toml_val}'"
            );
        }
    }

    #[test]
    fn storage_provider_maps_to_concrete_provider_kind() {
        assert_eq!(
            StorageProvider::parse_config_value("s3"),
            Some(StorageProvider::S3)
        );
        assert_eq!(
            StorageProvider::parse_config_value("gcs"),
            Some(StorageProvider::Gcs)
        );
        assert_eq!(
            StorageProvider::parse_config_value("azure"),
            Some(StorageProvider::Azure)
        );
        assert_eq!(
            StorageProvider::parse_config_value("auto"),
            Some(StorageProvider::Auto)
        );
        assert_eq!(StorageProvider::parse_config_value("local"), None);

        assert_eq!(StorageProvider::S3.toml_value(), "s3");
        assert_eq!(StorageProvider::Gcs.toml_value(), "gcs");
        assert_eq!(StorageProvider::Azure.toml_value(), "azure");
        assert_eq!(StorageProvider::Auto.toml_value(), "auto");

        assert_eq!(
            StorageProvider::S3.label(),
            "s3 (Amazon S3 or S3-compatible)"
        );
        assert_eq!(StorageProvider::Gcs.label(), "gcs (Google Cloud Storage)");
        assert_eq!(StorageProvider::Azure.label(), "azure (Azure Blob Storage)");
        assert_eq!(
            StorageProvider::Auto.label(),
            "auto (CRAB_STORAGE_PROVIDER or s3)"
        );

        assert_eq!(StorageProvider::S3.credential_discovery_scheme(), None);
        assert_eq!(
            StorageProvider::Gcs.credential_discovery_scheme(),
            Some("gs")
        );
        assert_eq!(
            StorageProvider::Azure.credential_discovery_scheme(),
            Some("az")
        );
        assert_eq!(StorageProvider::Auto.credential_discovery_scheme(), None);

        assert_eq!(
            StorageProvider::S3.storage_provider_kind(),
            Some(StorageProviderKind::S3)
        );
        assert_eq!(
            StorageProvider::Gcs.storage_provider_kind(),
            Some(StorageProviderKind::Gcs)
        );
        assert_eq!(
            StorageProvider::Azure.storage_provider_kind(),
            Some(StorageProviderKind::Azure)
        );
        assert_eq!(StorageProvider::Auto.storage_provider_kind(), None);

        assert_eq!(
            StorageProvider::from_storage_provider_kind(StorageProviderKind::S3),
            Some(StorageProvider::S3)
        );
        assert_eq!(
            StorageProvider::from_storage_provider_kind(StorageProviderKind::Gcs),
            Some(StorageProvider::Gcs)
        );
        assert_eq!(
            StorageProvider::from_storage_provider_kind(StorageProviderKind::Azure),
            Some(StorageProvider::Azure)
        );
        assert_eq!(
            StorageProvider::from_storage_provider_kind(StorageProviderKind::Local),
            None
        );
    }

    #[test]
    fn auth_overlay_invalid_provider_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[auth]\nprovider = \"bogus-provider\"\n").unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("invalid provider should be rejected");
        assert!(matches!(err, crate::core::CrabError::Configuration { .. }));
    }

    #[test]
    fn auth_overlay_missing_optional_fields_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        // Only set provider — everything else should keep defaults.
        std::fs::write(&p, "[auth]\nprovider = \"aws-oidc\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse with missing optional fields");
        assert_eq!(cfg.auth.provider, AuthProvider::AwsOidc);
        assert_eq!(cfg.auth.storage_provider, StorageProvider::Auto);
        assert!(cfg.auth.issuer_url.is_none());
        assert!(cfg.auth.client_id.is_none());
        assert_eq!(cfg.auth.scopes, "openid email profile");
        assert_eq!(cfg.auth.aws.session_duration_secs, 3600);
    }

    #[test]
    fn auth_overlay_full_aws_oidc_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[auth]
provider = "aws-oidc"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli-prod"
scopes = "openid email"
token_cache_path = "/tmp/tokens"

[auth.aws]
role_arn = "arn:aws:iam::123456789012:role/crab-dev"
region = "us-west-2"
session_duration_secs = 7200
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse full AWS OIDC config");
        assert_eq!(cfg.auth.provider, AuthProvider::AwsOidc);
        assert_eq!(
            cfg.auth.issuer_url.as_deref(),
            Some("https://login.corp.example.com")
        );
        assert_eq!(cfg.auth.client_id.as_deref(), Some("crab-cli-prod"));
        assert_eq!(cfg.auth.scopes, "openid email");
        assert_eq!(cfg.auth.token_cache_path, "/tmp/tokens");
        assert_eq!(
            cfg.auth.aws.role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/crab-dev")
        );
        assert_eq!(cfg.auth.aws.region.as_deref(), Some("us-west-2"));
        assert_eq!(cfg.auth.aws.session_duration_secs, 7200);
    }

    #[test]
    fn auth_overlay_full_gcp_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[auth]
provider = "gcp-workload-identity"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli"

[auth.gcp]
workload_identity_pool = "projects/123/locations/global/workloadIdentityPools/pool/providers/idp"
service_account = "crab@proj.iam.gserviceaccount.com"
project_id = "my-project"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse full GCP config");
        assert_eq!(cfg.auth.provider, AuthProvider::GcpWorkloadIdentity);
        assert!(cfg.auth.gcp.workload_identity_pool.is_some());
        assert_eq!(
            cfg.auth.gcp.service_account.as_deref(),
            Some("crab@proj.iam.gserviceaccount.com")
        );
        assert_eq!(cfg.auth.gcp.project_id.as_deref(), Some("my-project"));
    }

    #[test]
    fn auth_overlay_full_azure_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[auth]
provider = "azure-entra"
issuer_url = "https://login.microsoftonline.com/tenant/v2.0"
client_id = "app-id"

[auth.azure]
tenant_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
subscription_id = "sub-123"
storage_account = "mlmodels"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse full Azure config");
        assert_eq!(cfg.auth.provider, AuthProvider::AzureEntra);
        assert_eq!(
            cfg.auth.azure.tenant_id.as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(cfg.auth.azure.subscription_id.as_deref(), Some("sub-123"));
        assert_eq!(cfg.auth.azure.storage_account.as_deref(), Some("mlmodels"));
    }

    #[test]
    fn auth_overlay_crab_auth_canonical_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[auth]
provider = "crab-auth"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli"
auth_endpoint = "https://crab-auth.corp.example.com/v1/credentials"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse crab-auth config");
        assert_eq!(cfg.auth.provider, AuthProvider::CrabAuth);
        assert_eq!(cfg.auth.provider.as_str(), "crab-auth");
        assert_eq!(
            cfg.auth.auth_endpoint.as_deref(),
            Some("https://crab-auth.corp.example.com/v1/credentials")
        );
    }

    #[test]
    fn aws_session_duration_clamped_to_valid_range() {
        // Below minimum (900)
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[auth]\nprovider = \"aws-oidc\"\n\n[auth.aws]\nsession_duration_secs = 100\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse and clamp low duration");
        assert_eq!(cfg.auth.aws.session_duration_secs, 900);

        // Above maximum (43200)
        let p2 = dir.path().join("config2.toml");
        std::fs::write(
            &p2,
            "[auth]\nprovider = \"aws-oidc\"\n\n[auth.aws]\nsession_duration_secs = 99999\n",
        )
        .unwrap();

        let cfg2 = Config::resolve_local_from(Some(p2), PathBuf::from("/nonexistent"))
            .expect("should parse and clamp high duration");
        assert_eq!(cfg2.auth.aws.session_duration_secs, 43200);

        // Within range — no clamping
        let p3 = dir.path().join("config3.toml");
        std::fs::write(
            &p3,
            "[auth]\nprovider = \"aws-oidc\"\n\n[auth.aws]\nsession_duration_secs = 7200\n",
        )
        .unwrap();

        let cfg3 = Config::resolve_local_from(Some(p3), PathBuf::from("/nonexistent"))
            .expect("should parse valid duration without clamping");
        assert_eq!(cfg3.auth.aws.session_duration_secs, 7200);
    }

    #[test]
    fn auth_overlay_merges_across_layers() {
        let dir = tempfile::tempdir().unwrap();

        // User TOML: sets provider, issuer_url, client_id, aws role
        let user = dir.path().join("user.toml");
        std::fs::write(
            &user,
            r#"[auth]
provider = "aws-oidc"
issuer_url = "https://login.corp.example.com"
client_id = "crab-cli"
scopes = "openid email"

[auth.aws]
role_arn = "arn:aws:iam::111111111111:role/user-role"
region = "us-east-1"
session_duration_secs = 3600
"#,
        )
        .unwrap();

        // Repo TOML: overrides region and session duration
        let repo = dir.path().join("repo.toml");
        std::fs::write(
            &repo,
            r#"[auth.aws]
region = "eu-west-1"
session_duration_secs = 7200
"#,
        )
        .unwrap();

        let local = Config::resolve_local_from(Some(user), repo).expect("should merge user + repo");

        // Provider, issuer, client_id from user layer
        assert_eq!(local.auth.provider, AuthProvider::AwsOidc);
        assert_eq!(
            local.auth.issuer_url.as_deref(),
            Some("https://login.corp.example.com")
        );
        assert_eq!(local.auth.client_id.as_deref(), Some("crab-cli"));
        assert_eq!(local.auth.scopes, "openid email");
        // role_arn from user layer (repo didn't override)
        assert_eq!(
            local.auth.aws.role_arn.as_deref(),
            Some("arn:aws:iam::111111111111:role/user-role")
        );
        // region and session_duration overridden by repo layer
        assert_eq!(local.auth.aws.region.as_deref(), Some("eu-west-1"));
        assert_eq!(local.auth.aws.session_duration_secs, 7200);

        // Remote JSON layer: override provider to crab-auth, add endpoint
        let remote_json = br#"{
            "auth": {
                "provider": "crab-auth",
                "auth_endpoint": "https://crab-auth.corp.example.com/v1/creds"
            }
        }"#;
        let final_cfg = local
            .resolve_remote(remote_json)
            .expect("should merge remote JSON");

        // Remote overrode provider
        assert_eq!(final_cfg.auth.provider, AuthProvider::CrabAuth);
        assert_eq!(
            final_cfg.auth.auth_endpoint.as_deref(),
            Some("https://crab-auth.corp.example.com/v1/creds")
        );
        // Earlier layers preserved for non-overridden fields
        assert_eq!(
            final_cfg.auth.issuer_url.as_deref(),
            Some("https://login.corp.example.com")
        );
        assert_eq!(final_cfg.auth.aws.region.as_deref(), Some("eu-west-1"));
    }

    // --- Tier config ---

    #[test]
    fn tier_config_defaults() {
        let cfg = TierConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.to_ia_days, 30);
        assert_eq!(cfg.to_deep_days, 180);
        assert_eq!(cfg.noncurrent_days, 30);
        assert_eq!(cfg.restore_tier, "standard");
        assert_eq!(cfg.restore_duration_days, 7);
        assert_eq!(cfg.restore_max_concurrency, 16);
        assert_eq!(cfg.restore_timeout_secs, 21600);
        assert_eq!(cfg.restripe_output_class, "standard");
    }

    #[test]
    fn config_default_embeds_tier_defaults() {
        let cfg = Config::default();
        assert!(!cfg.tier.enabled);
        assert_eq!(cfg.tier.to_ia_days, 30);
        assert_eq!(cfg.tier.restore_tier, "standard");
    }

    #[test]
    fn overlay_tier_section_parses_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[tier]
enabled = true
to_ia_days = 60
to_deep_days = 365
noncurrent_days = 90
restore_tier = "expedited"
restore_duration_days = 14
restore_max_concurrency = 32
restore_timeout_secs = 43200
restripe_output_class = "standard-ia"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse tier section");
        assert!(cfg.tier.enabled);
        assert_eq!(cfg.tier.to_ia_days, 60);
        assert_eq!(cfg.tier.to_deep_days, 365);
        assert_eq!(cfg.tier.noncurrent_days, 90);
        assert_eq!(cfg.tier.restore_tier, "expedited");
        assert_eq!(cfg.tier.restore_duration_days, 14);
        assert_eq!(cfg.tier.restore_max_concurrency, 32);
        assert_eq!(cfg.tier.restore_timeout_secs, 43200);
        assert_eq!(cfg.tier.restripe_output_class, "standard-ia");
    }

    #[test]
    fn overlay_tier_partial_preserves_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[tier]\nenabled = true\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse partial tier section");
        assert!(cfg.tier.enabled);
        assert_eq!(cfg.tier.to_ia_days, 30);
        assert_eq!(cfg.tier.restore_tier, "standard");
    }

    // --- Cost config ---

    #[test]
    fn cost_config_defaults() {
        let cfg = CostConfig::default();
        assert_eq!(cfg.inventory_source, "auto");
        assert_eq!(cfg.list_concurrency, 32);
        assert!((cfg.sample_ratio - 1.0).abs() < f64::EPSILON);
        assert!(cfg.pricing_file.is_empty());
        assert!(cfg.price_table_version.is_empty());
        assert_eq!(cfg.access_window_days, 90);
        assert!(!cfg.apply_free_tier);
        assert_eq!(cfg.report_max_staleness_hours, 48);
    }

    #[test]
    fn config_default_embeds_cost_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.cost.inventory_source, "auto");
        assert_eq!(cfg.cost.list_concurrency, 32);
    }

    #[test]
    fn overlay_cost_section_parses_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[cost]
inventory_source = "live"
list_concurrency = 64
sample_ratio = 0.5
pricing_file = "/etc/crab/prices.yaml"
price_table_version = "2026-03-01"
access_window_days = 180
apply_free_tier = true
report_max_staleness_hours = 24
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse cost section");
        assert_eq!(cfg.cost.inventory_source, "live");
        assert_eq!(cfg.cost.list_concurrency, 64);
        assert!((cfg.cost.sample_ratio - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.cost.pricing_file, "/etc/crab/prices.yaml");
        assert_eq!(cfg.cost.price_table_version, "2026-03-01");
        assert_eq!(cfg.cost.access_window_days, 180);
        assert!(cfg.cost.apply_free_tier);
        assert_eq!(cfg.cost.report_max_staleness_hours, 24);
    }

    #[test]
    fn overlay_cost_partial_preserves_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[cost]\ninventory_source = \"report\"\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse partial cost section");
        assert_eq!(cfg.cost.inventory_source, "report");
        assert_eq!(cfg.cost.list_concurrency, 32);
        assert!((cfg.cost.sample_ratio - 1.0).abs() < f64::EPSILON);
    }

    // --- Restripe config ---

    #[test]
    fn restripe_config_defaults() {
        let cfg = RestripeConfig::default();
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn config_default_embeds_restripe_defaults() {
        let cfg = Config::default();
        assert!(cfg.restripe.profiles.is_empty());
    }

    #[test]
    fn overlay_restripe_section_parses_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"[restripe.profiles.ml]
target_xorb_bytes = 268435456
max_xorbs_per_file = 4
group_by = "file"
compression = "zstd:3"

[restripe.profiles.code]
target_xorb_bytes = 16777216
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse restripe profiles");
        assert_eq!(cfg.restripe.profiles.len(), 2);

        let ml = cfg.restripe.profiles.get("ml").expect("ml profile");
        assert_eq!(ml.target_xorb_bytes, Some(268_435_456));
        assert_eq!(ml.max_xorbs_per_file, Some(4));
        assert_eq!(ml.group_by.as_deref(), Some("file"));
        assert_eq!(ml.compression.as_deref(), Some("zstd:3"));

        let code = cfg.restripe.profiles.get("code").expect("code profile");
        assert_eq!(code.target_xorb_bytes, Some(16_777_216));
        assert!(code.max_xorbs_per_file.is_none());
    }

    // --- GC config ---

    #[test]
    fn gc_config_defaults() {
        let cfg = GcConfig::default();
        assert!(!cfg.class_aware);
    }

    #[test]
    fn config_default_embeds_gc_defaults() {
        let cfg = Config::default();
        assert!(!cfg.gc.class_aware);
    }

    #[test]
    fn overlay_gc_section_parses_class_aware() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[gc]\nclass_aware = true\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse gc section");
        assert!(cfg.gc.class_aware);
    }

    // --- Hydrate auto_restore ---

    #[test]
    fn hydrate_auto_restore_defaults_to_true() {
        let cfg = Config::default();
        assert!(cfg.hydrate.auto_restore);
    }

    #[test]
    fn overlay_hydrate_auto_restore() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[hydrate]\nauto_restore = false\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse hydrate.auto_restore");
        assert!(!cfg.hydrate.auto_restore);
    }

    // --- Hydrate auto_prefetch ---

    #[test]
    fn hydrate_auto_prefetch_defaults_to_true() {
        let cfg = Config::default();
        assert!(cfg.hydrate.auto_prefetch);
    }

    #[test]
    fn overlay_hydrate_auto_prefetch_false() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[hydrate]\nauto_prefetch = false\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse hydrate.auto_prefetch");
        assert!(!cfg.hydrate.auto_prefetch);
    }

    #[test]
    fn overlay_hydrate_auto_prefetch_explicit_true() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[hydrate]\nauto_prefetch = true\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse hydrate.auto_prefetch = true");
        assert!(cfg.hydrate.auto_prefetch);
    }

    // --- Tier/Cost/GC env overrides ---

    /// Env vars used by tier/cost/gc tests.
    const STORAGE_ECONOMY_ENV_VARS: &[&str] = &[
        "CRAB_TIER_ENABLED",
        "CRAB_TIER_TO_IA_DAYS",
        "CRAB_TIER_TO_DEEP_DAYS",
        "CRAB_TIER_NONCURRENT_DAYS",
        "CRAB_TIER_RESTORE_TIER",
        "CRAB_TIER_RESTORE_DURATION_DAYS",
        "CRAB_TIER_RESTORE_MAX_CONCURRENCY",
        "CRAB_TIER_RESTORE_TIMEOUT_SECS",
        "CRAB_TIER_RESTRIPE_OUTPUT_CLASS",
        "CRAB_COST_INVENTORY_SOURCE",
        "CRAB_COST_LIST_CONCURRENCY",
        "CRAB_COST_SAMPLE_RATIO",
        "CRAB_COST_PRICING_FILE",
        "CRAB_COST_PRICE_TABLE_VERSION",
        "CRAB_COST_ACCESS_WINDOW_DAYS",
        "CRAB_COST_APPLY_FREE_TIER",
        "CRAB_COST_REPORT_MAX_STALENESS_HOURS",
        "CRAB_GC_CLASS_AWARE",
    ];

    fn clear_storage_economy_env() {
        for var in STORAGE_ECONOMY_ENV_VARS {
            // SAFETY: tests run in a shared process but never in parallel for
            // the same env var — the mutex below serializes access.
            unsafe {
                std::env::remove_var(var);
            }
        }
    }

    static STORAGE_ECONOMY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tier_env_overrides_apply() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();
        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_TIER_ENABLED", "true");
            std::env::set_var("CRAB_TIER_TO_IA_DAYS", "60");
            std::env::set_var("CRAB_TIER_TO_DEEP_DAYS", "365");
            std::env::set_var("CRAB_TIER_NONCURRENT_DAYS", "90");
            std::env::set_var("CRAB_TIER_RESTORE_TIER", "expedited");
            std::env::set_var("CRAB_TIER_RESTORE_DURATION_DAYS", "14");
            std::env::set_var("CRAB_TIER_RESTORE_MAX_CONCURRENCY", "32");
            std::env::set_var("CRAB_TIER_RESTORE_TIMEOUT_SECS", "43200");
            std::env::set_var("CRAB_TIER_RESTRIPE_OUTPUT_CLASS", "standard-ia");
        }

        let mut cfg = TierConfig::default();
        cfg.apply_env_overrides();

        assert!(cfg.enabled);
        assert_eq!(cfg.to_ia_days, 60);
        assert_eq!(cfg.to_deep_days, 365);
        assert_eq!(cfg.noncurrent_days, 90);
        assert_eq!(cfg.restore_tier, "expedited");
        assert_eq!(cfg.restore_duration_days, 14);
        assert_eq!(cfg.restore_max_concurrency, 32);
        assert_eq!(cfg.restore_timeout_secs, 43200);
        assert_eq!(cfg.restripe_output_class, "standard-ia");

        clear_storage_economy_env();
    }

    #[test]
    fn cost_env_overrides_apply() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();
        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_COST_INVENTORY_SOURCE", "live");
            std::env::set_var("CRAB_COST_LIST_CONCURRENCY", "64");
            std::env::set_var("CRAB_COST_SAMPLE_RATIO", "0.25");
            std::env::set_var("CRAB_COST_PRICING_FILE", "/tmp/prices.yaml");
            std::env::set_var("CRAB_COST_PRICE_TABLE_VERSION", "2026-06-01");
            std::env::set_var("CRAB_COST_ACCESS_WINDOW_DAYS", "180");
            std::env::set_var("CRAB_COST_APPLY_FREE_TIER", "true");
            std::env::set_var("CRAB_COST_REPORT_MAX_STALENESS_HOURS", "12");
        }

        let mut cfg = CostConfig::default();
        cfg.apply_env_overrides();

        assert_eq!(cfg.inventory_source, "live");
        assert_eq!(cfg.list_concurrency, 64);
        assert!((cfg.sample_ratio - 0.25).abs() < f64::EPSILON);
        assert_eq!(cfg.pricing_file, "/tmp/prices.yaml");
        assert_eq!(cfg.price_table_version, "2026-06-01");
        assert_eq!(cfg.access_window_days, 180);
        assert!(cfg.apply_free_tier);
        assert_eq!(cfg.report_max_staleness_hours, 12);

        clear_storage_economy_env();
    }

    #[test]
    fn gc_env_overrides_apply() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();
        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_GC_CLASS_AWARE", "true");
        }

        let mut cfg = GcConfig::default();
        cfg.apply_env_overrides();

        assert!(cfg.class_aware);

        clear_storage_economy_env();
    }

    #[test]
    fn tier_env_overrides_precedence_over_toml() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[tier]\nenabled = false\nto_ia_days = 60\nrestore_tier = \"bulk\"\n",
        )
        .unwrap();

        // Env overrides TOML for enabled and restore_tier.
        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_TIER_ENABLED", "true");
            std::env::set_var("CRAB_TIER_RESTORE_TIER", "expedited");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse with env overrides");

        // Env wins over TOML.
        assert!(cfg.tier.enabled);
        assert_eq!(cfg.tier.restore_tier, "expedited");
        // TOML value preserved where env is absent.
        assert_eq!(cfg.tier.to_ia_days, 60);

        clear_storage_economy_env();
    }

    #[test]
    fn cost_env_overrides_precedence_over_toml() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[cost]\ninventory_source = \"report\"\nlist_concurrency = 16\n",
        )
        .unwrap();

        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_COST_INVENTORY_SOURCE", "live");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse with env overrides");

        // Env wins.
        assert_eq!(cfg.cost.inventory_source, "live");
        // TOML preserved where env is absent.
        assert_eq!(cfg.cost.list_concurrency, 16);

        clear_storage_economy_env();
    }

    #[test]
    fn tier_env_malformed_values_are_ignored() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();
        // SAFETY: env mutations serialized by STORAGE_ECONOMY_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_TIER_ENABLED", "maybe");
            std::env::set_var("CRAB_TIER_TO_IA_DAYS", "not-a-number");
        }

        let mut cfg = TierConfig::default();
        cfg.apply_env_overrides();

        // Malformed values ignored; defaults preserved.
        assert!(!cfg.enabled);
        assert_eq!(cfg.to_ia_days, 30);

        clear_storage_economy_env();
    }

    #[test]
    fn tier_env_absent_vars_leave_baseline_untouched() {
        let _guard = STORAGE_ECONOMY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_storage_economy_env();

        let mut cfg = TierConfig {
            enabled: true,
            to_ia_days: 90,
            ..TierConfig::default()
        };
        cfg.apply_env_overrides();

        assert!(cfg.enabled);
        assert_eq!(cfg.to_ia_days, 90);
    }

    // --- Workflow config ---

    /// Env vars used by workflow tests, cleared before each test to keep
    /// the global env stable across `cargo test`'s threads.
    const WORKFLOW_ENV_VARS: &[&str] = &[
        "CRAB_WORKFLOW_ENABLED",
        "CRAB_WORKFLOW_DISCOVER",
        "CRAB_WORKFLOW_PARALLELISM",
        "CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
        "CRAB_WORKFLOW_MAX_OUTS_PER_STAGE",
        "CRAB_WORKFLOW_MAX_OUT_BYTES",
        "CRAB_WORKFLOW_LOCK_TIMEOUT_SECS",
    ];

    fn clear_workflow_env() {
        for var in WORKFLOW_ENV_VARS {
            // SAFETY: tests run in a shared process but never in parallel for
            // the same env var — the mutex below serializes access.
            unsafe {
                std::env::remove_var(var);
            }
        }
    }

    /// Env-mutating tests serialize on this mutex because `std::env` is
    /// process-global and cargo runs tests multi-threaded by default.
    static WORKFLOW_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn workflow_config_defaults() {
        let cfg = WorkflowConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.discover, WorkflowDiscover::Root);
        assert_eq!(cfg.parallelism, DEFAULT_WORKFLOW_PARALLELISM);
        assert_eq!(cfg.graceful_shutdown_timeout_secs, 10);
        assert_eq!(cfg.max_outs_per_stage, 10_000);
        assert_eq!(cfg.max_out_bytes, 1024 * 1024 * 1024 * 1024);
        assert_eq!(cfg.lock_timeout_secs, 600);
        assert!(cfg.remotes.is_empty());
    }

    #[test]
    fn config_default_embeds_workflow_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.workflow, WorkflowConfig::default());
    }

    #[test]
    fn overlay_workflow_section_parses_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[workflow]\nenabled = true\ndiscover = \"recursive\"\n\
             parallelism = 4\ngraceful_shutdown_timeout_secs = 30\n\
             max_outs_per_stage = 500\nmax_out_bytes = 1048576\n\
             lock_timeout_secs = 120\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse workflow section");
        assert!(cfg.workflow.enabled);
        assert_eq!(cfg.workflow.discover, WorkflowDiscover::Recursive);
        assert_eq!(cfg.workflow.parallelism, 4);
        assert_eq!(cfg.workflow.graceful_shutdown_timeout_secs, 30);
        assert_eq!(cfg.workflow.max_outs_per_stage, 500);
        assert_eq!(cfg.workflow.max_out_bytes, 1_048_576);
        assert_eq!(cfg.workflow.lock_timeout_secs, 120);
        assert!(cfg.workflow.remotes.is_empty());
    }

    #[test]
    fn overlay_workflow_named_remotes_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[workflow]\nenabled = true\n\n\
             [workflow.remotes.models]\nurl = \"crab://ml-cache/models\"\n\n\
             [workflow.remotes.metrics]\nurl = \"crab://ml-cache/metrics\"\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse workflow remotes");

        assert_eq!(cfg.workflow.remotes["models"].url, "crab://ml-cache/models");
        assert_eq!(
            cfg.workflow.remotes["metrics"].url,
            "crab://ml-cache/metrics"
        );
    }

    #[test]
    fn overlay_workflow_partial_preserves_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[workflow]\nenabled = true\n").unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse partial workflow section");
        assert!(cfg.workflow.enabled);
        // Non-overridden fields keep their defaults
        assert_eq!(cfg.workflow.parallelism, DEFAULT_WORKFLOW_PARALLELISM);
        assert_eq!(cfg.workflow.discover, WorkflowDiscover::Root);
        assert_eq!(cfg.workflow.max_outs_per_stage, 10_000);
    }

    #[test]
    fn overlay_accepts_hydra_section_without_disabling_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[workflow]\nenabled = true\n\n[hydra]\nenabled = true\nconfig_dir = \"conf\"\nconfig_name = \"config.yaml\"\n",
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should parse hydra section");

        assert!(cfg.workflow.enabled);
    }

    #[test]
    fn workflow_env_overrides_apply() {
        let _guard = WORKFLOW_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_workflow_env();
        // SAFETY: env mutations serialized by WORKFLOW_ENV_LOCK; cleared at end.
        unsafe {
            std::env::set_var("CRAB_WORKFLOW_ENABLED", "true");
            std::env::set_var("CRAB_WORKFLOW_DISCOVER", "recursive");
            std::env::set_var("CRAB_WORKFLOW_PARALLELISM", "8");
            std::env::set_var("CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS", "25");
            std::env::set_var("CRAB_WORKFLOW_MAX_OUTS_PER_STAGE", "99");
            std::env::set_var("CRAB_WORKFLOW_MAX_OUT_BYTES", "2048");
            std::env::set_var("CRAB_WORKFLOW_LOCK_TIMEOUT_SECS", "300");
        }

        let mut cfg = WorkflowConfig::default();
        cfg.apply_env_overrides();

        assert!(cfg.enabled);
        assert_eq!(cfg.discover, WorkflowDiscover::Recursive);
        assert_eq!(cfg.parallelism, 8);
        assert_eq!(cfg.graceful_shutdown_timeout_secs, 25);
        assert_eq!(cfg.max_outs_per_stage, 99);
        assert_eq!(cfg.max_out_bytes, 2048);
        assert_eq!(cfg.lock_timeout_secs, 300);

        clear_workflow_env();
    }

    #[test]
    fn workflow_env_bool_accepts_numeric_and_word_forms() {
        let _guard = WORKFLOW_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_workflow_env();

        for (raw, expected) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("OFF", false),
        ] {
            // SAFETY: serialized on WORKFLOW_ENV_LOCK.
            unsafe {
                std::env::set_var("CRAB_WORKFLOW_ENABLED", raw);
            }
            let mut cfg = WorkflowConfig::default();
            cfg.apply_env_overrides();
            assert_eq!(cfg.enabled, expected, "input {raw:?}");
        }

        clear_workflow_env();
    }

    #[test]
    fn workflow_env_malformed_values_are_ignored() {
        let _guard = WORKFLOW_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_workflow_env();
        // SAFETY: serialized on WORKFLOW_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_WORKFLOW_ENABLED", "maybe");
            std::env::set_var("CRAB_WORKFLOW_DISCOVER", "somewhere");
            std::env::set_var("CRAB_WORKFLOW_PARALLELISM", "not-a-number");
        }

        let mut cfg = WorkflowConfig::default();
        cfg.apply_env_overrides();

        // All three values were malformed; defaults preserved.
        assert!(!cfg.enabled);
        assert_eq!(cfg.discover, WorkflowDiscover::Root);
        assert_eq!(cfg.parallelism, DEFAULT_WORKFLOW_PARALLELISM);

        clear_workflow_env();
    }

    #[test]
    fn workflow_env_absent_vars_leave_baseline_untouched() {
        let _guard = WORKFLOW_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_workflow_env();

        let mut cfg = WorkflowConfig {
            enabled: true,
            discover: WorkflowDiscover::Recursive,
            parallelism: 12,
            ..WorkflowConfig::default()
        };
        cfg.apply_env_overrides();

        assert!(cfg.enabled);
        assert_eq!(cfg.discover, WorkflowDiscover::Recursive);
        assert_eq!(cfg.parallelism, 12);
    }

    // --- MetaDB config ---

    /// Env vars used by the metadb config tests. Cleared before each
    /// test and serialized on `METADB_ENV_LOCK` because `std::env` is
    /// process-global.
    const METADB_ENV_VARS: &[&str] = &[
        "CRAB_METADB_FILE_INDEX_PATH",
        "CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD",
        "CRAB_METADB_FILE_INDEX_WAL_FLUSH_SIZE",
        "CRAB_METADB_FILE_INDEX_BLOOM_BITS_PER_KEY",
        "CRAB_METADB_CHUNK_INDEX_PATH",
        "CRAB_METADB_CHUNK_INDEX_COMPACTION_THRESHOLD",
        "CRAB_METADB_CHUNK_INDEX_WAL_FLUSH_SIZE",
        "CRAB_METADB_CHUNK_INDEX_BLOOM_BITS_PER_KEY",
        "CRAB_METADB_CHUNK_INDEX_LOCAL_PATH",
        "CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES",
        "CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE",
    ];

    fn clear_metadb_env() {
        for var in METADB_ENV_VARS {
            // SAFETY: env mutations serialized by METADB_ENV_LOCK.
            unsafe {
                std::env::remove_var(var);
            }
        }
    }

    static METADB_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn metadb_toml_default_is_empty() {
        let toml = MetaDbTomlConfig::default();
        assert!(toml.file_index.path.is_none());
        assert!(toml.file_index.compaction_threshold.is_none());
        assert!(toml.file_index.wal_flush_size.is_none());
        assert!(toml.file_index.bloom_bits_per_key.is_none());
        assert!(toml.chunk_index.db.path.is_none());
        assert!(toml.chunk_index.db.compaction_threshold.is_none());
        assert!(toml.chunk_index.db.wal_flush_size.is_none());
        assert!(toml.chunk_index.db.bloom_bits_per_key.is_none());
        assert!(toml.chunk_index.local_path.is_none());
        assert!(toml.chunk_index.in_memory_ceiling_bytes.is_none());
        assert!(toml.chunk_index.cache_gc_grace.is_none());
    }

    #[test]
    fn metadb_section_absent_deserializes_cleanly_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        // Intentionally no `[metadb]` section.
        std::fs::write(&p, "upload_concurrency = 16\n").unwrap();

        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("missing [metadb] should resolve with defaults");
        assert!(cfg.metadb.file_index.path.is_none());
        assert!(cfg.metadb.chunk_index.db.path.is_none());
        assert!(cfg.metadb.chunk_index.in_memory_ceiling_bytes.is_none());
    }

    #[test]
    fn metadb_toml_parses_full_section() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[metadb.file_index]
path = "custom/file_index_db/"
compaction_threshold = 8
wal_flush_size = 8388608
bloom_bits_per_key = 12

[metadb.chunk_index]
path = "custom/chunk_index_db/"
compaction_threshold = 6
wal_flush_size = 6291456
bloom_bits_per_key = 14
local_path = "/var/cache/crab/chunk-index.sqlite"
in_memory_ceiling_bytes = 2147483648
cache_gc_grace = 5
"#,
        )
        .unwrap();

        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("full [metadb] section should parse");

        assert_eq!(
            cfg.metadb.file_index.path.as_deref(),
            Some("custom/file_index_db/")
        );
        assert_eq!(cfg.metadb.file_index.compaction_threshold, Some(8));
        assert_eq!(cfg.metadb.file_index.wal_flush_size, Some(8 * 1024 * 1024));
        assert_eq!(cfg.metadb.file_index.bloom_bits_per_key, Some(12));

        assert_eq!(
            cfg.metadb.chunk_index.db.path.as_deref(),
            Some("custom/chunk_index_db/")
        );
        assert_eq!(cfg.metadb.chunk_index.db.compaction_threshold, Some(6));
        assert_eq!(
            cfg.metadb.chunk_index.db.wal_flush_size,
            Some(6 * 1024 * 1024)
        );
        assert_eq!(cfg.metadb.chunk_index.db.bloom_bits_per_key, Some(14));
        assert_eq!(
            cfg.metadb.chunk_index.local_path.as_deref(),
            Some(std::path::Path::new("/var/cache/crab/chunk-index.sqlite"))
        );
        assert_eq!(
            cfg.metadb.chunk_index.in_memory_ceiling_bytes,
            Some(2 * 1024 * 1024 * 1024)
        );
        assert_eq!(cfg.metadb.chunk_index.cache_gc_grace, Some(5));
    }

    #[test]
    fn metadb_env_overrides_apply_all_vars() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();
        // SAFETY: env mutations serialized by METADB_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_METADB_FILE_INDEX_PATH", "env/file_index_db/");
            std::env::set_var("CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD", "9");
            std::env::set_var("CRAB_METADB_FILE_INDEX_WAL_FLUSH_SIZE", "1048576");
            std::env::set_var("CRAB_METADB_FILE_INDEX_BLOOM_BITS_PER_KEY", "11");
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_PATH", "env/chunk_index_db/");
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_COMPACTION_THRESHOLD", "7");
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_WAL_FLUSH_SIZE", "2097152");
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_BLOOM_BITS_PER_KEY", "13");
            std::env::set_var(
                "CRAB_METADB_CHUNK_INDEX_LOCAL_PATH",
                "/tmp/env-chunk-index.sqlite",
            );
            std::env::set_var(
                "CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES",
                "536870912",
            );
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE", "7");
        }

        let mut toml = MetaDbTomlConfig::default();
        toml.apply_env_overrides();

        assert_eq!(toml.file_index.path.as_deref(), Some("env/file_index_db/"));
        assert_eq!(toml.file_index.compaction_threshold, Some(9));
        assert_eq!(toml.file_index.wal_flush_size, Some(1024 * 1024));
        assert_eq!(toml.file_index.bloom_bits_per_key, Some(11));

        assert_eq!(
            toml.chunk_index.db.path.as_deref(),
            Some("env/chunk_index_db/")
        );
        assert_eq!(toml.chunk_index.db.compaction_threshold, Some(7));
        assert_eq!(toml.chunk_index.db.wal_flush_size, Some(2 * 1024 * 1024));
        assert_eq!(toml.chunk_index.db.bloom_bits_per_key, Some(13));
        assert_eq!(
            toml.chunk_index.local_path.as_deref(),
            Some(std::path::Path::new("/tmp/env-chunk-index.sqlite"))
        );
        assert_eq!(
            toml.chunk_index.in_memory_ceiling_bytes,
            Some(512 * 1024 * 1024)
        );
        assert_eq!(toml.chunk_index.cache_gc_grace, Some(7));

        clear_metadb_env();
    }

    #[test]
    fn metadb_env_malformed_values_are_ignored() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();
        // SAFETY: env mutations serialized by METADB_ENV_LOCK.
        unsafe {
            std::env::set_var(
                "CRAB_METADB_FILE_INDEX_COMPACTION_THRESHOLD",
                "not-a-number",
            );
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE", "negative? nope");
        }

        let mut toml = MetaDbTomlConfig::default();
        toml.apply_env_overrides();

        // Malformed values dropped; baseline untouched.
        assert!(toml.file_index.compaction_threshold.is_none());
        assert!(toml.chunk_index.cache_gc_grace.is_none());

        clear_metadb_env();
    }

    #[test]
    fn metadb_env_absent_vars_leave_baseline_untouched() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let mut toml = MetaDbTomlConfig::default();
        toml.file_index.compaction_threshold = Some(42);
        toml.chunk_index.cache_gc_grace = Some(99);
        toml.apply_env_overrides();

        assert_eq!(toml.file_index.compaction_threshold, Some(42));
        assert_eq!(toml.chunk_index.cache_gc_grace, Some(99));
    }

    #[test]
    fn metadb_env_overrides_precedence_over_toml() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[metadb.file_index]
path = "toml/file_index_db/"
compaction_threshold = 4

[metadb.chunk_index]
cache_gc_grace = 2
"#,
        )
        .unwrap();

        // SAFETY: env mutations serialized by METADB_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_METADB_FILE_INDEX_PATH", "env/file_index_db/");
            std::env::set_var("CRAB_METADB_CHUNK_INDEX_CACHE_GC_GRACE", "10");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("env+TOML merge should parse");

        // Env wins where set.
        assert_eq!(
            cfg.metadb.file_index.path.as_deref(),
            Some("env/file_index_db/")
        );
        assert_eq!(cfg.metadb.chunk_index.cache_gc_grace, Some(10));
        // TOML preserved where env is absent.
        assert_eq!(cfg.metadb.file_index.compaction_threshold, Some(4));

        clear_metadb_env();
    }

    #[test]
    fn build_metadb_config_defaults_without_toml() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let cfg = Config::default();
        let meta = cfg.build_metadb_config("org/my-repo");
        assert_eq!(meta.file_index_path, "org/my-repo/file_index_db/");
        assert_eq!(meta.chunk_index_path, ".crab/chunk_index_db/");
        // Default compaction threshold lives on `MetaDbConfig::default`;
        // TOML absent means the field is untouched.
        assert_eq!(meta.compaction_threshold, 4);
        assert_eq!(meta.wal_flush_size, 4 * 1024 * 1024);
        assert_eq!(meta.bloom_bits_per_key, 10);
        assert_eq!(meta.cache_gc_grace, 3);
        assert_eq!(meta.in_memory_ceiling_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn build_metadb_config_applies_toml_and_env() {
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[metadb.file_index]
path = "toml/file_index_db/"
compaction_threshold = 8
wal_flush_size = 8388608
bloom_bits_per_key = 12

[metadb.chunk_index]
path = "toml/chunk_index_db/"
in_memory_ceiling_bytes = 536870912
cache_gc_grace = 5
"#,
        )
        .unwrap();

        // Env wins over TOML: override the in-memory ceiling.
        // SAFETY: env mutations serialized by METADB_ENV_LOCK.
        unsafe {
            std::env::set_var(
                "CRAB_METADB_CHUNK_INDEX_IN_MEMORY_CEILING_BYTES",
                "268435456",
            );
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect("should resolve");
        let meta = cfg.build_metadb_config("org/my-repo");

        // Paths — TOML wins over derived defaults.
        assert_eq!(meta.file_index_path, "toml/file_index_db/");
        assert_eq!(meta.chunk_index_path, "toml/chunk_index_db/");

        // Shared-field tunables — file_index wins.
        assert_eq!(meta.compaction_threshold, 8);
        assert_eq!(meta.wal_flush_size, 8 * 1024 * 1024);
        assert_eq!(meta.bloom_bits_per_key, 12);

        // chunk_index-only — env overrides TOML.
        assert_eq!(meta.in_memory_ceiling_bytes, 256 * 1024 * 1024);
        // cache_gc_grace came from TOML (env absent for it).
        assert_eq!(meta.cache_gc_grace, 5);

        clear_metadb_env();
    }

    #[test]
    fn build_metadb_config_chunk_index_fallback_for_shared_tunables() {
        // When file_index omits a shared tunable, chunk_index's value
        // is used. Documents the merging strategy today so a future
        // split of the flat MetaDbConfig keeps known-good behavior.
        let _guard = METADB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_metadb_env();

        let mut cfg = Config::default();
        cfg.metadb.chunk_index.db.compaction_threshold = Some(6);
        cfg.metadb.chunk_index.db.wal_flush_size = Some(2 * 1024 * 1024);
        cfg.metadb.chunk_index.db.bloom_bits_per_key = Some(15);

        let meta = cfg.build_metadb_config("org/my-repo");
        assert_eq!(meta.compaction_threshold, 6);
        assert_eq!(meta.wal_flush_size, 2 * 1024 * 1024);
        assert_eq!(meta.bloom_bits_per_key, 15);
    }

    #[test]
    fn build_metadb_config_trims_trailing_slash_on_repo_prefix() {
        let cfg = Config::default();
        let meta = cfg.build_metadb_config("org/my-repo/");
        assert_eq!(meta.file_index_path, "org/my-repo/file_index_db/");
    }

    #[test]
    fn metadb_toml_deny_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[metadb.file_index]
unknown_key = "should fail"
"#,
        )
        .unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("unknown key under [metadb.file_index] should fail");
        assert!(matches!(
            err,
            super::super::error::CrabError::Configuration { .. }
        ));
    }

    // --- Cache service auth config ---

    /// Env vars used by cache auth tests. Serialized on `CACHE_ENV_LOCK`.
    const CACHE_ENV_VARS: &[&str] = &[CACHE_SERVICE_URL_ENV, "CRAB_CACHE_PSK", "CRAB_CACHE_TOKEN"];

    static CACHE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_cache_env() {
        for var in CACHE_ENV_VARS {
            // SAFETY: serialized on CACHE_ENV_LOCK.
            unsafe {
                std::env::remove_var(var);
            }
        }
    }

    #[test]
    fn cache_auth_defaults_to_none() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let cfg = Config::resolve_local_from(None, PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(cfg.cache.service_auth, ServiceAuth::None);
        assert!(cfg.cache.service_ca_cert.is_none());
        assert!(cfg.cache.service_client_cert.is_none());
        assert!(cfg.cache.service_client_key.is_none());
    }

    #[test]
    fn cache_auth_psk_from_toml() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_url = "https://cache.example.com:8443"
service_auth = "psk"
service_psk = "my-secret-key"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_auth,
            ServiceAuth::Psk("my-secret-key".into())
        );
        assert_eq!(
            cfg.cache.service_url.as_deref(),
            Some("https://cache.example.com:8443")
        );
    }

    #[test]
    fn cache_auth_psk_env_overrides_toml() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_auth = "psk"
service_psk = "toml-key"
"#,
        )
        .unwrap();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_CACHE_PSK", "env-key");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(cfg.cache.service_auth, ServiceAuth::Psk("env-key".into()));

        clear_cache_env();
    }

    #[test]
    fn cache_service_url_env_overrides_toml() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_url = "https://toml-cache.example.com:8443"
"#,
        )
        .unwrap();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var(
                CACHE_SERVICE_URL_ENV,
                " https://env-cache.example.com:9443 ",
            );
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_url.as_deref(),
            Some("https://env-cache.example.com:9443")
        );

        clear_cache_env();
    }

    #[test]
    fn empty_cache_service_url_env_is_ignored() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_url = "https://toml-cache.example.com:8443"
"#,
        )
        .unwrap();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var(CACHE_SERVICE_URL_ENV, "  ");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_url.as_deref(),
            Some("https://toml-cache.example.com:8443")
        );

        clear_cache_env();
    }

    #[test]
    fn cache_auth_bearer_from_file() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("cache-token");
        std::fs::write(&token_file, "  file-token-value  \n").unwrap();

        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            format!(
                r#"
[cache]
service_auth = "bearer"
service_token_path = "{}"
"#,
                token_file.display()
            ),
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_auth,
            ServiceAuth::Bearer("file-token-value".into())
        );
    }

    #[test]
    fn cache_auth_bearer_env_overrides_file() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("cache-token");
        std::fs::write(&token_file, "file-token").unwrap();

        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            format!(
                r#"
[cache]
service_auth = "bearer"
service_token_path = "{}"
"#,
                token_file.display()
            ),
        )
        .unwrap();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_CACHE_TOKEN", "env-token");
        }

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_auth,
            ServiceAuth::Bearer("env-token".into())
        );

        clear_cache_env();
    }

    #[test]
    fn cache_auth_env_psk_without_toml_section() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_CACHE_PSK", "env-only-psk");
        }

        let cfg = Config::resolve_local_from(None, PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_auth,
            ServiceAuth::Psk("env-only-psk".into())
        );

        clear_cache_env();
    }

    #[test]
    fn cache_auth_env_token_without_toml_section() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_CACHE_TOKEN", "env-only-token");
        }

        let cfg = Config::resolve_local_from(None, PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_auth,
            ServiceAuth::Bearer("env-only-token".into())
        );

        clear_cache_env();
    }

    #[test]
    fn cache_auth_psk_env_wins_over_token_env() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        // SAFETY: serialized on CACHE_ENV_LOCK.
        unsafe {
            std::env::set_var("CRAB_CACHE_PSK", "psk-value");
            std::env::set_var("CRAB_CACHE_TOKEN", "token-value");
        }

        let cfg = Config::resolve_local_from(None, PathBuf::from("/nonexistent")).unwrap();
        // PSK takes priority when both are set.
        assert_eq!(cfg.cache.service_auth, ServiceAuth::Psk("psk-value".into()));

        clear_cache_env();
    }

    #[test]
    fn cache_service_ca_cert_from_toml() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_ca_cert = "/etc/crab/cache-ca.pem"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(
            cfg.cache.service_ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/crab/cache-ca.pem"))
        );
    }

    #[test]
    fn cache_service_mtls_from_toml() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_auth = "mtls"
service_ca_cert = "/etc/crab/cache-ca.pem"
service_client_cert = "/etc/crab/cache-client.pem"
service_client_key = "/etc/crab/cache-client-key.pem"
"#,
        )
        .unwrap();

        let cfg = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent")).unwrap();
        assert_eq!(cfg.cache.service_auth, ServiceAuth::Mtls);
        assert_eq!(
            cfg.cache.service_ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/crab/cache-ca.pem"))
        );
        assert_eq!(
            cfg.cache.service_client_cert.as_deref(),
            Some(std::path::Path::new("/etc/crab/cache-client.pem"))
        );
        assert_eq!(
            cfg.cache.service_client_key.as_deref(),
            Some(std::path::Path::new("/etc/crab/cache-client-key.pem"))
        );
    }

    #[test]
    fn cache_service_mtls_rejects_half_configured_identity() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cache_env();

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[cache]
service_auth = "mtls"
service_client_cert = "/etc/crab/cache-client.pem"
"#,
        )
        .unwrap();

        let err = Config::resolve_local_from(Some(p), PathBuf::from("/nonexistent"))
            .expect_err("half-configured mTLS identity should fail");
        match err {
            crate::core::CrabError::Configuration { key, origin } => {
                assert_eq!(origin, "cache");
                assert!(key.contains("service_client_cert"));
            }
            other => panic!("expected Configuration, got {other:?}"),
        }
    }
}

pub use crab_staging::StagingConfig;

// ---------------------------------------------------------------------------
// Workflow configuration
// ---------------------------------------------------------------------------

/// Default workflow feature flag. When `false`, the workflow layer is inert
/// and no `.crab/workflow/` artifacts are created.
const DEFAULT_WORKFLOW_ENABLED: bool = false;

/// Default stage-discovery mode. `Root` considers only `crab.yaml` at the
/// repo root; nested yaml files are rejected unless `Recursive` is selected.
const DEFAULT_WORKFLOW_DISCOVER: WorkflowDiscover = WorkflowDiscover::Root;

/// Default lockfile storage mode. `Single` preserves the classic monolithic
/// `crab.lock` at the repo root; `Split` writes one `*.workflow.lock` per
/// `*.workflow.yaml` alongside its yaml.
const DEFAULT_WORKFLOW_LOCKFILE: WorkflowLockfile = WorkflowLockfile::Single;

/// Default number of stages executed in parallel across independent DAG
/// branches. Capped at 8 to avoid overwhelming the machine; uses the
/// number of logical CPUs when available.
pub const DEFAULT_WORKFLOW_PARALLELISM: u32 = {
    // Compile-time constant: we pick 4 as a safe default. The runtime
    // resolver in `Config::resolve_local` may override this with
    // `min(available_parallelism, 8)` via env or TOML.
    4
};

/// Default graceful-shutdown window: child gets SIGTERM, then 10s, then SIGKILL.
const DEFAULT_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// Default ceiling on declared outs per stage. Guards against pathological
/// YAML from codegen or templated pipelines.
const DEFAULT_WORKFLOW_MAX_OUTS_PER_STAGE: usize = 10_000;

/// Default per-out byte ceiling: 1 TiB. Stages producing larger files
/// raise `StageOutTooLarge`.
const DEFAULT_WORKFLOW_MAX_OUT_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

/// Default scheduler-lock timeout: 10 minutes. A second `crab run` on the
/// same repo waits up to this long for the first to finish.
const DEFAULT_WORKFLOW_LOCK_TIMEOUT_SECS: u64 = 600;

/// Stage-discovery mode from `[workflow] discover`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowDiscover {
    /// Only `crab.yaml` at the repo root is parsed. Nested yaml files
    /// are rejected with `WorkflowDiscoveryAmbiguous`.
    Root,
    /// All `crab.yaml` files under the repo root participate, each
    /// rooted at its containing directory.
    Recursive,
}

impl Default for WorkflowDiscover {
    fn default() -> Self {
        DEFAULT_WORKFLOW_DISCOVER
    }
}

/// Lockfile storage mode. Mirrors
/// [`crate::workflow::lockfile_split::LockfileMode`] so callers can
/// thread it through from config without a second conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowLockfile {
    /// One monolithic `crab.lock` at the repo root. Default.
    Single,
    /// Per-workflow-file lockfiles: `<name>.workflow.lock` next to
    /// each `<name>.workflow.yaml`, plus `crab.lock` for stages
    /// declared in the root `crab.yaml`.
    Split,
}

impl Default for WorkflowLockfile {
    fn default() -> Self {
        DEFAULT_WORKFLOW_LOCKFILE
    }
}

/// Named workflow remote.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRemoteConfig {
    /// URL used for outputs declaring `remote: <name>` when this is
    /// a Crab URL, and for DVC-style `remote://name/path` external
    /// dependency/output aliases when it names a supported external
    /// backend.
    pub url: String,
}

/// Workflow-layer configuration from the `[workflow]` TOML section.
///
/// The whole subsystem is feature-flagged behind `enabled = false` so
/// partial rollouts don't destabilize commands that never touch a
/// `crab.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkflowConfig {
    /// Master switch for the workflow layer.
    #[serde(default = "default_workflow_enabled")]
    pub enabled: bool,
    /// How `crab run` locates `crab.yaml` files.
    #[serde(default)]
    pub discover: WorkflowDiscover,
    /// Lockfile storage layout. `Single` keeps the monolithic
    /// `crab.lock`; `Split` emits one lockfile per workflow YAML.
    #[serde(default)]
    pub lockfile: WorkflowLockfile,
    /// Maximum stages executed concurrently across a DAG.
    #[serde(default = "default_workflow_parallelism")]
    pub parallelism: u32,
    /// Seconds between SIGTERM and SIGKILL when a stage exceeds its timeout.
    #[serde(default = "default_workflow_graceful_shutdown_timeout_secs")]
    pub graceful_shutdown_timeout_secs: u64,
    /// Maximum declared outs per stage.
    #[serde(default = "default_workflow_max_outs_per_stage")]
    pub max_outs_per_stage: usize,
    /// Maximum bytes per declared out.
    #[serde(default = "default_workflow_max_out_bytes")]
    pub max_out_bytes: u64,
    /// Seconds a second `crab run` will wait for the scheduler lock.
    #[serde(default = "default_workflow_lock_timeout_secs")]
    pub lock_timeout_secs: u64,
    /// When true, `--cache-push` is rejected. Pull operations still
    /// work. Intended for CI consumers that only read from the shared
    /// cache while designated builders push.
    #[serde(default)]
    pub remote_cache_readonly: bool,
    /// Named remotes selected by DVC-compatible `outs.remote` and
    /// `remote://name/path` external aliases.
    #[serde(default)]
    pub remotes: BTreeMap<String, WorkflowRemoteConfig>,
}

fn default_workflow_enabled() -> bool {
    DEFAULT_WORKFLOW_ENABLED
}
fn default_workflow_parallelism() -> u32 {
    DEFAULT_WORKFLOW_PARALLELISM
}
fn default_workflow_graceful_shutdown_timeout_secs() -> u64 {
    DEFAULT_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS
}
fn default_workflow_max_outs_per_stage() -> usize {
    DEFAULT_WORKFLOW_MAX_OUTS_PER_STAGE
}
fn default_workflow_max_out_bytes() -> u64 {
    DEFAULT_WORKFLOW_MAX_OUT_BYTES
}
fn default_workflow_lock_timeout_secs() -> u64 {
    DEFAULT_WORKFLOW_LOCK_TIMEOUT_SECS
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: default_workflow_enabled(),
            discover: WorkflowDiscover::default(),
            lockfile: WorkflowLockfile::default(),
            parallelism: default_workflow_parallelism(),
            graceful_shutdown_timeout_secs: default_workflow_graceful_shutdown_timeout_secs(),
            max_outs_per_stage: default_workflow_max_outs_per_stage(),
            max_out_bytes: default_workflow_max_out_bytes(),
            lock_timeout_secs: default_workflow_lock_timeout_secs(),
            remote_cache_readonly: false,
            remotes: BTreeMap::new(),
        }
    }
}

impl WorkflowConfig {
    /// Apply `CRAB_WORKFLOW_*` environment-variable overrides in place.
    ///
    /// Unknown or malformed values are logged and skipped; malformed
    /// overrides never poison a valid baseline.
    ///
    /// Recognized variables:
    ///
    /// - `CRAB_WORKFLOW_ENABLED` — `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`
    /// - `CRAB_WORKFLOW_DISCOVER` — `root` or `recursive`
    /// - `CRAB_WORKFLOW_LOCKFILE` — `single` or `split`
    /// - `CRAB_WORKFLOW_PARALLELISM` — unsigned integer
    /// - `CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS` — unsigned integer
    /// - `CRAB_WORKFLOW_MAX_OUTS_PER_STAGE` — unsigned integer
    /// - `CRAB_WORKFLOW_MAX_OUT_BYTES` — unsigned integer
    /// - `CRAB_WORKFLOW_LOCK_TIMEOUT_SECS` — unsigned integer
    /// - `CRAB_WORKFLOW_REMOTE_CACHE_READONLY` — `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`
    pub fn apply_env_overrides(&mut self) {
        if let Some(v) = env_bool("CRAB_WORKFLOW_ENABLED") {
            self.enabled = v;
        }
        if let Ok(raw) = std::env::var("CRAB_WORKFLOW_DISCOVER") {
            match raw.to_ascii_lowercase().as_str() {
                "root" => self.discover = WorkflowDiscover::Root,
                "recursive" => self.discover = WorkflowDiscover::Recursive,
                other => tracing::warn!(
                    value = %other,
                    "CRAB_WORKFLOW_DISCOVER must be \"root\" or \"recursive\"; ignoring"
                ),
            }
        }
        if let Ok(raw) = std::env::var("CRAB_WORKFLOW_LOCKFILE") {
            match raw.to_ascii_lowercase().as_str() {
                "single" => self.lockfile = WorkflowLockfile::Single,
                "split" => self.lockfile = WorkflowLockfile::Split,
                other => tracing::warn!(
                    value = %other,
                    "CRAB_WORKFLOW_LOCKFILE must be \"single\" or \"split\"; ignoring"
                ),
            }
        }
        if let Some(v) = env_parse::<u32>("CRAB_WORKFLOW_PARALLELISM") {
            self.parallelism = v;
        }
        if let Some(v) = env_parse::<u64>("CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS") {
            self.graceful_shutdown_timeout_secs = v;
        }
        if let Some(v) = env_parse::<usize>("CRAB_WORKFLOW_MAX_OUTS_PER_STAGE") {
            self.max_outs_per_stage = v;
        }
        if let Some(v) = env_parse::<u64>("CRAB_WORKFLOW_MAX_OUT_BYTES") {
            self.max_out_bytes = v;
        }
        if let Some(v) = env_parse::<u64>("CRAB_WORKFLOW_LOCK_TIMEOUT_SECS") {
            self.lock_timeout_secs = v;
        }
        if let Some(v) = env_bool("CRAB_WORKFLOW_REMOTE_CACHE_READONLY") {
            self.remote_cache_readonly = v;
        }
    }

    /// Minimum free disk space (bytes) before skipping cache writes.
    /// Returns the default of 100 MB. Configurable via
    /// `CRAB_WORKFLOW_MIN_CACHE_HEADROOM` env var (bytes).
    pub fn min_cache_headroom_bytes(&self) -> u64 {
        if let Some(v) = env_parse::<u64>("CRAB_WORKFLOW_MIN_CACHE_HEADROOM") {
            return v;
        }
        crate::workflow::cache::DEFAULT_MIN_CACHE_HEADROOM_BYTES
    }
}

/// Parse a boolean env var. Accepts common truthy/falsy spellings;
/// unrecognized values are logged and return `None` (caller keeps baseline).
fn env_bool(var: &str) -> Option<bool> {
    let raw = std::env::var(var).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            tracing::warn!(var = var, value = %other, "ignoring non-boolean env value");
            None
        }
    }
}

/// Parse a numeric env var. Malformed values are logged and return `None`.
fn env_parse<T>(var: &str) -> Option<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(var).ok()?;
    match raw.trim().parse::<T>() {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(var = var, value = %raw, error = %e, "ignoring unparseable env value");
            None
        }
    }
}
