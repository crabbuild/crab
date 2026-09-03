//! `crab doctor` — run a comprehensive health check.
//!
//! Validates the local crab setup in one pass: git version, filter
//! driver registration, remote reachability, credentials, config
//! integrity, cache health, and staging area state. Prints a
//! green/red checklist so users can quickly identify what's broken.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crab_auth::token_cache::{TokenCache, expand_token_cache_path};
use crab_cache::active_probe::{self, ActiveProbeAuth, ActiveProbeObject};
use crab_cache::build_cache_service_http_client;
use crab_cache::health::{CacheHealthReport, CacheIssueKind, CacheRootState, inspect_cache};
use crab_cache::path_class::{CacheRouteContract, cache_route_contract_matches_current};

use crate::core::config::{Config, ServiceAuth, ServiceMode};
use crate::core::credential_discovery::{CredentialSource, discover_credentials};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::core::project_config::ProjectConfig;
use crate::core::style::CliStyle;
use tokio_util::sync::CancellationToken;

const MAX_CACHE_SERVICE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Outcome of a single diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

/// A single diagnostic result.
#[derive(Serialize, schemars::JsonSchema)]
pub struct CheckResult {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

/// Summary counts for the doctor report.
#[derive(Serialize, schemars::JsonSchema)]
pub struct DoctorSummary {
    ok: u32,
    warn: u32,
    fail: u32,
}

/// Payload emitted by `crab doctor --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct DoctorPayload {
    checks: Vec<CheckResult>,
    summary: DoctorSummary,
}

#[derive(Serialize)]
struct CacheServiceSupportBundle {
    collected_at_unix_ms: u64,
    redacted: bool,
    service: CacheServiceSupportConfig,
    checks: Vec<CheckResult>,
    probes: CacheServiceSupportProbes,
    signals: CacheServiceSupportSignals,
    runbooks: Vec<CacheServiceRunbookLink>,
    docs: CacheServiceSupportDocs,
    recommended_commands: Vec<&'static str>,
}

#[derive(Serialize)]
struct CacheServiceSupportConfig {
    configured: bool,
    service_url: &'static str,
    scheme: Option<String>,
    mode: Option<&'static str>,
    push_warming: Option<bool>,
    auth: Option<&'static str>,
    ca: Option<&'static str>,
    client_cert: Option<&'static str>,
    config_error: Option<String>,
}

#[derive(Default, Serialize)]
struct CacheServiceSupportProbes {
    health: Option<CacheServiceHttpProbe>,
    auth: Option<CacheServiceHttpProbe>,
    capabilities: Option<CacheServiceHttpProbe>,
    capabilities_snapshot: Option<CacheServiceCapabilitiesProbe>,
    authz: Option<CacheServiceHttpProbe>,
    authz_snapshot: Option<CacheServiceAuthzProbe>,
    admin_stats: Option<CacheServiceHttpProbe>,
    metrics: Option<CacheServiceHttpProbe>,
    admin_snapshot: Option<CacheServiceAdminProbe>,
    metrics_totals: BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct CacheServiceHttpProbe {
    endpoint: &'static str,
    ok: bool,
    http_status: Option<u16>,
    error: Option<String>,
}

#[derive(Default, Serialize)]
struct CacheServiceSupportSignals {
    cache_hit_rate: Option<f64>,
    origin_fallback_rate: Option<f64>,
    bytes_from_cache_rate: Option<f64>,
    integrity_repairs: Option<u64>,
    mutable_proxy_reads: Option<u64>,
    push_warming_writes: Option<u64>,
    evicted_objects: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
struct CacheServiceRunbookLink {
    alert: &'static str,
    url: &'static str,
}

#[derive(Serialize)]
struct CacheServiceSupportDocs {
    monitoring: &'static str,
    runbooks: &'static str,
    troubleshooting: &'static str,
}

const CACHE_SERVICE_AUTH_PROBE_PATH: &str = "/v1/capabilities";
const CACHE_SERVICE_CAPABILITIES_SCHEMA: &str = "crab-cache-service.capabilities.v1";
const CACHE_SERVICE_AUTHZ_SCHEMA: &str = "crab-cache-service.authz-check.v1";
const CACHE_SERVICE_SUPPORT_SCHEMA: &str = "cache-service.support-bundle";
// Must exceed the cache server readiness origin probe budget so doctor can
// preserve HTTP 503 degradation instead of reporting a client-side timeout.
const CACHE_SERVICE_DOCTOR_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_SERVICE_DOCS_MONITORING: &str = "https://crab.build/docs/cli/cache-service/monitoring";
const CACHE_SERVICE_DOCS_RUNBOOKS: &str = "https://crab.build/docs/cli/cache-service/runbooks";
const CACHE_SERVICE_DOCS_TROUBLESHOOTING: &str =
    "https://crab.build/docs/cli/cache-service/troubleshooting";
const CACHE_SERVICE_RUNBOOKS: [CacheServiceRunbookLink; 5] = [
    CacheServiceRunbookLink {
        alert: "CrabCacheRuntimeIntegrityRepair",
        url: "https://crab.build/docs/cli/cache-service/runbooks#crab-cache-runtime-integrity-repair",
    },
    CacheServiceRunbookLink {
        alert: "CrabCacheStartupIntegrityRepair",
        url: "https://crab.build/docs/cli/cache-service/runbooks#crab-cache-startup-integrity-repair",
    },
    CacheServiceRunbookLink {
        alert: "CrabCacheOriginFallbackHigh",
        url: "https://crab.build/docs/cli/cache-service/runbooks#crab-cache-origin-fallback-high",
    },
    CacheServiceRunbookLink {
        alert: "CrabCacheHitRateLow",
        url: "https://crab.build/docs/cli/cache-service/runbooks#crab-cache-hit-rate-low",
    },
    CacheServiceRunbookLink {
        alert: "CrabCacheMutableProxyActive",
        url: "https://crab.build/docs/cli/cache-service/runbooks#crab-cache-mutable-proxy-active",
    },
];

impl CheckResult {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Ok,
            detail: detail.into(),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Warn,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
}

/// Run `crab doctor` in the current working directory.
pub async fn run_doctor(mode: OutputMode, cache_service_active_probe: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_doctor_in(&cwd, mode, cache_service_active_probe).await
}

/// Collect a redacted cache-service support bundle for incidents.
pub async fn run_cache_service_support_bundle(
    mode: OutputMode,
    output: Option<PathBuf>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let bundle = collect_cache_service_support_bundle(&cwd).await;

    if let Some(path) = output.as_deref() {
        write_json_bundle(path, &bundle)?;
    }

    match (mode, output.as_deref()) {
        (OutputMode::Json, _) => emit_json(CACHE_SERVICE_SUPPORT_SCHEMA, "1.0", &bundle),
        (OutputMode::Text, Some(path)) => {
            println!("Wrote cache-service support bundle to {}", path.display());
            println!("Redacted: {}", bundle.redacted);
            println!("Configured: {}", bundle.service.configured);
        }
        (OutputMode::Text | OutputMode::Jsonl, None) => print_json_bundle(&bundle)?,
        (OutputMode::Jsonl, Some(path)) => {
            println!("Wrote cache-service support bundle to {}", path.display());
        }
    }

    Ok(())
}

/// Run the metadb-focused health report (`crab doctor --metadb`).
///
/// Opens a short-lived [`MetaDbGuard`] against the current repo and
/// prints a two-section summary: one per SlateDB instance plus a
/// shard-count line derived from S3 LIST on `.crab/shards/`.
///
/// This is a read-only snapshot — no compaction trigger, no WAL
/// replay. Failures during open map to a `Fail` line in the report
/// rather than aborting, so operators running this in an incident
/// still see what they can see.
pub async fn run_doctor_metadb(mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    crate::cmd::metadb::run_doctor_metadb_in(&cwd, mode).await
}

/// Run the cost optimizer report (`crab doctor --cost`).
///
/// Collects inventory, applies pricing, generates recommendations,
/// and renders the report in human or JSON format.
pub async fn run_cost_report(
    mode: OutputMode,
    pricing_file: Option<String>,
    inventory_source: Option<String>,
    sample: Option<f64>,
    top_k: Option<usize>,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    let cwd = std::env::current_dir()?;
    let remote_url = ProjectConfig::remote_url(&cwd)?;
    let remote = crate::git::url::CrabUrl::parse(&remote_url)?;
    let store =
        crate::auth::build_repository_url_store(config, &remote, "doctor.cost", cancel).await?;
    let report = crate::cost::engine::build_report(
        config,
        &store,
        &crate::cost::engine::ReportOptions {
            pricing_file,
            inventory_source,
            sample_ratio: sample,
            top_k,
        },
        cancel,
    )
    .await?;
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    if mode == OutputMode::Json {
        emit_json("cost", "1.0", &report);
    } else {
        let output = crate::cost::report::render_human(&report);
        print!("{output}");
    }

    Ok(())
}

/// Run all diagnostic checks rooted at `root`.
pub async fn run_doctor_in(
    root: &Path,
    mode: OutputMode,
    cache_service_active_probe: bool,
) -> Result<()> {
    #[cfg(feature = "otlp")]
    let _span = tracing::info_span!(
        "doctor.cost",
        command = "doctor",
        root = %root.display(),
    )
    .entered();

    let mut results = Vec::new();

    results.push(check_git_version());
    results.push(check_crab_binary());
    results.push(check_git_repo(root));
    results.push(check_filter_driver(root));
    results.push(check_gitattributes(root));
    results.push(check_crab_config(root));
    results.push(check_remote_url(root));
    results.push(check_auth());
    results.push(check_remote_access(root).await);
    results.push(check_credential_discovery(root).await);
    results.push(check_staging(root).await);
    results.extend(check_cache(root).await);
    results.extend(check_cache_service(root, cache_service_active_probe).await);
    results.push(check_version_guard(root));

    if mode == OutputMode::Json {
        let mut ok: u32 = 0;
        let mut warn: u32 = 0;
        let mut fail: u32 = 0;
        for r in &results {
            match r.status {
                CheckStatus::Ok => ok += 1,
                CheckStatus::Warn => warn += 1,
                CheckStatus::Fail => fail += 1,
            }
        }
        let payload = DoctorPayload {
            checks: results,
            summary: DoctorSummary { ok, warn, fail },
        };
        emit_json("doctor", "1.0", payload);
        return Ok(());
    }

    // Print results.
    let style = CliStyle::resolve(mode);
    println!("crab doctor\n");

    let mut fail_count = 0;
    let mut warn_count = 0;

    for r in &results {
        let icon = match r.status {
            CheckStatus::Ok => format!("{}", style.success.apply_to("✓")),
            CheckStatus::Warn => format!("{}", style.warning.apply_to("!")),
            CheckStatus::Fail => format!("{}", style.error.apply_to("✗")),
        };
        println!("  {icon} {:<24} {}", r.name, r.detail);
        match r.status {
            CheckStatus::Fail => fail_count += 1,
            CheckStatus::Warn => warn_count += 1,
            CheckStatus::Ok => {}
        }
    }

    println!();
    if fail_count > 0 {
        println!(
            "{fail_count} problem(s) found, {warn_count} warning(s). \
             Run `crab env` for full diagnostics."
        );
    } else if warn_count > 0 {
        println!("All checks passed with {warn_count} warning(s).");
    } else {
        println!("All checks passed. Everything looks good.");
    }

    Ok(())
}

// --- Individual checks ---

/// Verify git is installed and meets the minimum version requirement.
fn check_git_version() -> CheckResult {
    let Ok(output) = Command::new("git").arg("--version").output() else {
        return CheckResult::fail("git", "git not found on PATH");
    };

    if !output.status.success() {
        return CheckResult::fail("git", "git --version failed");
    }

    let version_str = String::from_utf8_lossy(&output.stdout);
    let version_str = version_str.trim();

    // Git 2.27+ is required for the long-running filter-process protocol.
    let parts: Vec<&str> = version_str.split_whitespace().collect();
    let ver = parts.get(2).unwrap_or(&"unknown");

    let segments: Vec<u32> = ver.split('.').filter_map(|s| s.parse().ok()).collect();

    if segments.len() >= 2 && (segments[0] > 2 || (segments[0] == 2 && segments[1] >= 27)) {
        CheckResult::ok("git", version_str.to_owned())
    } else {
        CheckResult::warn(
            "git",
            format!("{version_str} (2.27+ recommended for filter-process protocol)"),
        )
    }
}

/// Verify the crab binary is accessible.
fn check_crab_binary() -> CheckResult {
    let bin = crate::cmd::init::crab_binary_path();
    let path = std::path::Path::new(&bin);

    if bin == "crab" {
        // Bare name — check if it's on PATH.
        match Command::new("which").arg("crab").output() {
            Ok(o) if o.status.success() => {
                CheckResult::ok("crab binary", format!("{bin} (on PATH)"))
            }
            _ => CheckResult::warn("crab binary", "crab not found on PATH (using current exe)"),
        }
    } else if path.exists() {
        CheckResult::ok("crab binary", bin)
    } else {
        CheckResult::warn("crab binary", format!("{bin} (file not found)"))
    }
}

/// Check that we're inside a git repository.
fn check_git_repo(root: &Path) -> CheckResult {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output();

    match output {
        Ok(o) if o.status.success() => CheckResult::ok("git repository", "detected"),
        _ => CheckResult::warn("git repository", "not inside a git repository"),
    }
}

/// Check that crab's git drivers are registered in git config.
fn check_filter_driver(root: &Path) -> CheckResult {
    let keys = [
        "filter.crab.process",
        "filter.crab.required",
        "diff.crab.command",
    ];

    for key in &keys {
        let output = Command::new("git")
            .args(["config", key])
            .current_dir(root)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let val = String::from_utf8_lossy(&o.stdout);
                if val.trim().is_empty() {
                    return CheckResult::fail(
                        "git drivers",
                        format!("{key} is empty — run `crab install`"),
                    );
                }
            }
            _ => {
                return CheckResult::fail(
                    "git drivers",
                    format!("{key} not set — run `crab install`"),
                );
            }
        }
    }

    CheckResult::ok(
        "git drivers",
        "filter.crab.* and diff.crab.command configured",
    )
}

/// Check that .gitattributes exists and has crab patterns.
fn check_gitattributes(root: &Path) -> CheckResult {
    let ga_path = root.join(".gitattributes");
    match std::fs::read_to_string(&ga_path) {
        Ok(content) => {
            let crab_lines: Vec<&str> = content
                .lines()
                .filter(|l| l.contains("filter=crab"))
                .collect();

            if crab_lines.is_empty() {
                CheckResult::warn(
                    ".gitattributes",
                    "exists but no filter=crab patterns — run `crab track '*.ext'`",
                )
            } else {
                CheckResult::ok(
                    ".gitattributes",
                    format!("{} crab pattern(s)", crab_lines.len()),
                )
            }
        }
        Err(_) => CheckResult::warn(
            ".gitattributes",
            "not found — run `crab track '*.ext'` to start tracking files",
        ),
    }
}

/// Check that the committed and local configuration files parse.
fn check_crab_config(root: &Path) -> CheckResult {
    if root.join(".crab.toml").is_file() && !root.join("crab.toml").is_file() {
        return CheckResult::fail(
            "crab config",
            "Crab no longer reads .crab.toml — rename it to crab.toml and commit the rename",
        );
    }
    match ProjectConfig::load_for_repo(root) {
        Ok(Some(config)) if config.remote.url.trim().is_empty() => {
            return CheckResult::fail("crab config", "crab.toml has an empty [remote].url");
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            return CheckResult::warn(
                "crab config",
                "crab.toml not found — run `crab configure <REMOTE>`",
            );
        }
        Err(error) => return CheckResult::fail("crab config", error.to_string()),
    }
    let config_path = root.join(".crab/local.toml");
    if !config_path.exists() {
        return CheckResult::warn(
            "crab config",
            "crab.toml valid; .crab/local.toml not found (using defaults)",
        );
    }

    match Config::resolve_for_repo(root) {
        Ok(_) => CheckResult::ok("crab config", "crab.toml and .crab/local.toml valid"),
        Err(e) => CheckResult::fail("crab config", format!("parse error: {e}")),
    }
}

/// Check that `crab.toml` contains a valid remote URL.
fn check_remote_url(root: &Path) -> CheckResult {
    match ProjectConfig::load_for_repo(root) {
        Ok(Some(config)) => match crate::git::url::CrabUrl::parse(&config.remote.url) {
            Ok(parsed) => CheckResult::ok(
                "remote URL",
                format!("crab://{}/{}", parsed.bucket, parsed.repo_path),
            ),
            Err(e) => CheckResult::fail("remote URL", format!("invalid: {e}")),
        },
        Ok(None) => CheckResult::warn(
            "remote URL",
            "crab.toml not found — run `crab configure <REMOTE>`",
        ),
        Err(error) => CheckResult::fail("remote URL", error.to_string()),
    }
}

fn cache_service_repo_path(root: &Path) -> std::result::Result<String, String> {
    let remote = ProjectConfig::remote_url(root).map_err(|error| error.to_string())?;
    crate::git::url::CrabUrl::parse(&remote)
        .map(|parsed| parsed.repo_path)
        .map_err(|e| format!("cache-service authz requires a valid crab remote: {e}"))
}

/// Check the local auth state: config validity, token presence, token expiry.
///
/// For `Static` and `None` providers this is a quick OK — no crab-managed
/// tokens to inspect. For OIDC providers we verify the config can construct
/// a provider and that cached tokens exist and are not expired.
fn check_auth() -> CheckResult {
    let config = match Config::resolve_local() {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::warn("auth", format!("could not load config: {e}"));
        }
    };

    if !config.auth.provider.uses_token_cache() {
        // Verify the config can construct a provider (catches bad storage_provider, etc.).
        if let Err(e) = crate::auth::create_provider(&config) {
            return CheckResult::fail("auth", format!("config error: {e}"));
        }
        return CheckResult::ok(
            "auth",
            format!("{} (no crab-managed auth)", config.auth.provider),
        );
    }

    // OIDC provider — verify config can construct a provider.
    if let Err(e) = crate::auth::create_provider(&config) {
        return CheckResult::fail(
            "auth",
            format!("{} config error: {e}", config.auth.provider),
        );
    }

    // Check for cached tokens.
    let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
    let cache = match TokenCache::new(cache_dir) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::warn(
                "auth",
                format!("{} — cannot open token cache: {e}", config.auth.provider),
            );
        }
    };

    let provider_kind = config.auth.provider;
    let provider_name = provider_kind.as_str();
    match cache.load_any(provider_kind.token_cache_keys()) {
        Ok(Some(tokens)) => {
            let (expiry, is_expired) = parse_token_expiry(&tokens.id_token);
            let identity = tokens
                .identity
                .email
                .as_deref()
                .unwrap_or(&tokens.identity.subject);

            if is_expired {
                CheckResult::warn(
                    "auth",
                    format!(
                        "{} — tokens expired{} — run `crab login`",
                        provider_name,
                        expiry.map(|e| format!(" ({e})")).unwrap_or_default(),
                    ),
                )
            } else {
                CheckResult::ok(
                    "auth",
                    format!(
                        "{provider_name} — {identity}{}",
                        expiry.map(|e| format!(", expires {e}")).unwrap_or_default(),
                    ),
                )
            }
        }
        Ok(None) => CheckResult::warn(
            "auth",
            format!("{provider_name} — no cached tokens — run `crab login`"),
        ),
        Err(e) => CheckResult::warn(
            "auth",
            format!("{provider_name} — failed to read token cache: {e}"),
        ),
    }
}

/// Parse the `exp` claim from a JWT and return (ISO 8601 string, is_expired).
fn parse_token_expiry(id_token: &str) -> (Option<String>, bool) {
    let parts: Vec<&str> = id_token.splitn(3, '.').collect();
    if parts.len() < 2 {
        return (None, true);
    }

    let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(parts[1]) else {
        return (None, true);
    };

    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) else {
        return (None, true);
    };

    let Some(exp) = claims.get("exp").and_then(serde_json::Value::as_u64) else {
        return (None, false);
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let is_expired = now >= exp;

    // Format as ISO 8601 UTC (manual to avoid chrono dependency).
    let secs_per_min = 60u64;
    let secs_per_hour = 3600u64;
    let secs_per_day = 86400u64;

    // Days since epoch → year/month/day via a simple leap-year-aware loop.
    let mut remaining = exp;
    let mut year = 1970u64;
    loop {
        let days_in_year =
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                366
            } else {
                365
            };
        let secs_in_year = days_in_year * secs_per_day;
        if remaining < secs_in_year {
            break;
        }
        remaining -= secs_in_year;
        year += 1;
    }

    let is_leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_months: [u64; 12] = [
        31,
        if is_leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];

    let mut day_of_year = remaining / secs_per_day;
    remaining %= secs_per_day;
    let mut month = 0u64;
    for &dim in &days_in_months {
        if day_of_year < dim {
            break;
        }
        day_of_year -= dim;
        month += 1;
    }

    let hour = remaining / secs_per_hour;
    remaining %= secs_per_hour;
    let minute = remaining / secs_per_min;
    let second = remaining % secs_per_min;

    let expiry_str = format!(
        "{year:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}Z",
        month + 1,
        day_of_year + 1,
    );

    (Some(expiry_str), is_expired)
}

/// Check that the bucket is reachable and the Crab repository exists.
async fn check_remote_access(root: &Path) -> CheckResult {
    let url = match ProjectConfig::remote_url(root) {
        Ok(url) => url,
        Err(error) => return CheckResult::fail("remote access", error.to_string()),
    };

    let Ok(parsed) = crate::git::url::CrabUrl::parse(&url) else {
        return CheckResult::warn("remote access", "skipped (invalid remote URL)");
    };

    let config = match Config::resolve_for_repo(root) {
        Ok(config) => config,
        Err(error) => {
            return CheckResult::fail(
                "remote access",
                format!("cannot load repository config: {error}; run `crab configure`"),
            );
        }
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let store =
        match crate::auth::build_repository_url_store(&config, &parsed, "doctor", &cancel).await {
            Ok(s) => s,
            Err(error) => {
                return remote_access_failure(&parsed.bucket, Some(&parsed.repo_path), &error);
            }
        };

    let prefix = object_store::path::Path::from(parsed.repo_path.as_str());

    // Listing distinguishes a missing bucket from a missing repository object.
    let mut stream = store.inner().list(Some(&prefix));
    if let Err(error) = stream.try_next().await {
        let error = CrabError::from(crab_storage::map_object_store_error(error, prefix.as_ref()));
        return remote_access_failure(&parsed.bucket, None, &error);
    }

    let layout = crate::storage::StoreLayout::new(store.clone(), parsed.repo_path.clone());
    match store.head(&layout.layout_descriptor_path()).await {
        Ok(_) => CheckResult::ok(
            "remote access",
            format!(
                "bucket '{}' and repository '{}' reachable",
                parsed.bucket, parsed.repo_path
            ),
        ),
        Err(error) => remote_access_failure(&parsed.bucket, Some(&parsed.repo_path), &error),
    }
}

fn remote_access_failure(bucket: &str, repo: Option<&str>, error: &CrabError) -> CheckResult {
    let scope = repo.map_or_else(
        || format!("bucket '{bucket}'"),
        |repo| format!("repository '{repo}' in bucket '{bucket}'"),
    );

    match error {
        CrabError::NotFound { .. } if repo.is_some() => CheckResult::fail(
            "remote access",
            format!("{scope} is not initialized — run `crab configure <REMOTE>` to create it"),
        ),
        CrabError::NotFound { .. } => CheckResult::fail(
            "remote access",
            format!("bucket '{bucket}' not found — create it or correct the remote URL"),
        ),
        CrabError::NoCredentials => CheckResult::fail(
            "remote access",
            "no cloud credentials found — configure provider credentials, then rerun `crab doctor`",
        ),
        CrabError::Forbidden { .. }
        | CrabError::AuthFailed { .. }
        | CrabError::AuthExpired { .. } => CheckResult::fail(
            "remote access",
            format!(
                "access denied to {scope} — grant the active identity the required bucket and repository-prefix permissions"
            ),
        ),
        CrabError::Configuration { .. } => CheckResult::fail(
            "remote access",
            format!("storage configuration is invalid: {error}; run `crab configure`"),
        ),
        _ => CheckResult::warn(
            "remote access",
            format!("could not verify {scope}: {error}"),
        ),
    }
}

/// Check credential discovery chain for the configured remote.
///
/// Reads the remote URL from `crab.toml` and runs
/// the credential discovery chain. Reports which credential source was
/// found (or warns if none).
async fn check_credential_discovery(root: &Path) -> CheckResult {
    let url = match ProjectConfig::remote_url(root) {
        Ok(url) => url,
        Err(error) => return CheckResult::fail("credential discovery", error.to_string()),
    };

    let aws_profile = Config::resolve_for_repo(root)
        .ok()
        .and_then(|config| config.auth.aws.profile);
    let result = discover_credentials(&url, aws_profile.as_deref()).await;

    match result.source {
        CredentialSource::None => CheckResult::warn("credential discovery", result.description),
        _ if !result.valid => CheckResult::warn(
            "credential discovery",
            format!("{} (invalid)", result.description),
        ),
        _ => CheckResult::ok("credential discovery", result.description),
    }
}

/// Check the staging area for corruption or lock issues.
async fn check_staging(root: &Path) -> CheckResult {
    let staging_dir = root.join(".crab/staging");
    if !staging_dir.exists() {
        return CheckResult::ok("staging area", "clean (no staging directory)");
    }

    let staging = match crab_staging::StagingAreaReadOnly::open(staging_dir.clone()).await {
        Ok(staging) => staging,
        Err(crab_staging::StagingError::StagingLocked { holder_pid }) => {
            return CheckResult::warn(
                "staging area",
                holder_pid.map_or_else(
                    || "active writer; lifecycle check deferred".to_owned(),
                    |pid| format!("active writer PID {pid}; lifecycle check deferred"),
                ),
            );
        }
        Err(error) => {
            return CheckResult::fail(
                "staging area",
                format!(
                    "cannot open canonical staging lifecycle metadata: {error}; recreate staging and re-add affected paths"
                ),
            );
        }
    };
    let health = match staging.lifecycle_health() {
        Ok(health) => health,
        Err(error) => {
            return CheckResult::fail(
                "staging area",
                format!("lifecycle metadata unreadable: {error}; run `crab staging verify`"),
            );
        }
    };
    if health.quarantined_entries > 0 {
        return CheckResult::fail(
            "staging area",
            format!(
                "layout v{} has {} quarantined entr{}; payload bytes were preserved — inspect with `crab staging verify`",
                health.layout_version,
                health.quarantined_entries,
                if health.quarantined_entries == 1 {
                    "y"
                } else {
                    "ies"
                }
            ),
        );
    }
    if health.unresolved_publications > 0 {
        let intent_ids = staging
            .unresolved_publication_intents()
            .map(|intents| {
                intents
                    .into_iter()
                    .map(|intent| intent.intent_id.as_str().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|error| format!("unreadable intent ids: {error}"));
        return CheckResult::fail(
            "staging area",
            format!(
                "layout v{} has {} unresolved Git-index publication(s) ({intent_ids}); rerun `crab add` to reconcile the exact index state",
                health.layout_version, health.unresolved_publications
            ),
        );
    }
    if health.reclaimable_superseded_leases > 0 || health.reclaimable_files > 0 {
        return CheckResult::fail(
            "staging area",
            format!(
                "layout v{} has {} published superseded lease(s) and {} unowned file(s) without a canonical path head; run `crab staging clean --prune-abandoned`",
                health.layout_version,
                health.reclaimable_superseded_leases,
                health.reclaimable_files
            ),
        );
    }
    if health.open_push_snapshots > 0 || health.committed_push_snapshots > 0 {
        return CheckResult::warn(
            "staging area",
            format!(
                "layout v{} has {} open and {} committed push snapshot(s), with {} superseded lease pin(s) retaining {} bytes safely; retry push or run `crab staging clean --prune-abandoned`",
                health.layout_version,
                health.open_push_snapshots,
                health.committed_push_snapshots,
                health.snapshot_pinned_superseded_leases,
                health
                    .snapshot_pinned_segment_bytes
                    .saturating_add(health.snapshot_pinned_prepared_bytes)
            ),
        );
    }
    if health.open_batches_without_publication > 0 {
        return CheckResult::warn(
            "staging area",
            format!(
                "layout v{} has {} unpublished staging batch(es) without a Git-index publication intent; complete `git add`/`crab add` or run `crab staging clean --prune-abandoned`",
                health.layout_version, health.open_batches_without_publication
            ),
        );
    }

    // Count segment files to give a rough health indicator.
    let segment_count = std::fs::read_dir(&staging_dir)
        .map(|rd| {
            rd.filter_map(std::result::Result::ok)
                .filter(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|ext| ext == "segment" || ext == "seg")
                })
                .count()
        })
        .unwrap_or(0);

    if segment_count > 100 {
        CheckResult::warn(
            "staging area",
            format!("{segment_count} segments — consider `crab staging clean`"),
        )
    } else {
        CheckResult::ok(
            "staging area",
            format!(
                "layout v{}, {segment_count} segment(s), {} recipe(s), {} payload(s)",
                health.layout_version, health.recipes, health.payloads
            ),
        )
    }
}

async fn check_cache(repo_root: &Path) -> Vec<CheckResult> {
    let config = match Config::resolve_for_repo(repo_root) {
        Ok(config) => config,
        Err(error) => {
            return vec![CheckResult::fail(
                "local cache",
                format!("cannot resolve cache budget: {error}"),
            )];
        }
    };
    let root = crate::cache::default_cache_root();
    match inspect_cache(&root, config.cache.max_bytes, &CancellationToken::new()).await {
        Ok(report) => cache_checks(&report),
        Err(error) => vec![CheckResult::warn(
            "local cache",
            format!("inspection unavailable: {error}"),
        )],
    }
}

fn cache_checks(report: &CacheHealthReport) -> Vec<CheckResult> {
    if report.root_state == CacheRootState::Missing {
        return vec![CheckResult::ok(
            "local cache",
            "not yet created; inspection did not initialize it",
        )];
    }
    let summary = format!(
        "{}: {} allocated bytes, {} logical bytes, {} byte budget; {} (not full integrity verification)",
        report.root.display(),
        report.observed.allocated_bytes,
        report.observed.logical_bytes,
        report.budget_bytes,
        if report.scan_complete {
            "complete scan"
        } else {
            "partial scan; lower bounds"
        }
    );
    let mut checks = vec![if report.is_available() {
        CheckResult::ok("local cache", summary)
    } else {
        CheckResult::warn("local cache", summary)
    }];
    if report.over_budget == Some(true) {
        checks.push(CheckResult::warn("cache budget", "allocated usage exceeds the effective budget; inspect `crab cache stats --json`. Prune removes only eligible payloads; retained state needs its owner's maintenance"));
    }
    for issue in &report.issues {
        let action = match issue.kind {
            CacheIssueKind::UnsafePath => {
                "check ownership, links and owner-only permissions on this exact path; no automatic repair was attempted"
            }
            CacheIssueKind::Busy => {
                "retry after cache activity stops; do not remove database side files"
            }
            CacheIssueKind::Corrupt => {
                "stop cache writers and preserve the affected database and side files for diagnosis; inspection does not rebuild them"
            }
            CacheIssueKind::Io | CacheIssueKind::Unavailable => {
                "check this path and available disk space, then retry; inspection changed no cache state"
            }
        };
        let detail = format!(
            "{} [{}]: {}; {action}",
            report.root.join(&issue.path).display(),
            issue.family.unwrap_or("root"),
            issue.error
        );
        checks.push(if issue.kind == CacheIssueKind::UnsafePath {
            CheckResult::fail("cache family", detail)
        } else {
            CheckResult::warn("cache family", detail)
        });
    }
    if report.omitted_issues > 0 {
        checks.push(CheckResult::warn("cache family", format!("{} additional issues omitted; inspect per-family counts with `crab cache stats --json`", report.omitted_issues)));
    }
    checks
}

async fn collect_cache_service_support_bundle(root: &Path) -> CacheServiceSupportBundle {
    let collected_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let config_result = Config::resolve_for_repo(root);
    let base_url = config_result
        .as_ref()
        .ok()
        .and_then(|config| config.cache.service_url.as_deref())
        .map(|url| url.trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty());
    let repo_path = cache_service_repo_path(root).ok();
    let mut checks = check_cache_service(root, false).await;
    redact_cache_service_checks(&mut checks, base_url.as_deref());

    let mut probes = CacheServiceSupportProbes::default();
    let mut service = match &config_result {
        Ok(config) => cache_service_support_config(config, None),
        Err(err) => CacheServiceSupportConfig {
            configured: false,
            service_url: "unavailable",
            scheme: None,
            mode: None,
            push_warming: None,
            auth: None,
            ca: None,
            client_cert: None,
            config_error: Some(err.to_string()),
        },
    };

    if let (Ok(config), Some(base_url)) = (config_result.as_ref(), base_url.as_deref()) {
        service = cache_service_support_config(config, Some(base_url));
        if let Ok(client) = cache_service_http_client(
            config.cache.service_ca_cert.as_deref(),
            config.cache.service_client_cert.as_deref(),
            config.cache.service_client_key.as_deref(),
        ) {
            probes.health = Some(
                collect_cache_service_http_probe(
                    &client,
                    format!("{base_url}/v1/health"),
                    "/v1/health",
                    base_url,
                    None,
                )
                .await,
            );

            probes.auth = Some(
                collect_cache_service_http_probe(
                    &client,
                    format!("{base_url}{CACHE_SERVICE_AUTH_PROBE_PATH}"),
                    CACHE_SERVICE_AUTH_PROBE_PATH,
                    base_url,
                    Some(&config.cache.service_auth),
                )
                .await,
            );

            let (capabilities_probe, capabilities_snapshot) =
                collect_cache_service_capabilities_snapshot(
                    &client,
                    base_url,
                    &config.cache.service_auth,
                )
                .await;
            probes.capabilities = Some(capabilities_probe);
            probes.capabilities_snapshot = capabilities_snapshot;

            if let Some(repo_path) = repo_path.as_deref() {
                let (authz_probe, authz_snapshot) = collect_cache_service_authz_snapshot(
                    &client,
                    base_url,
                    &config.cache.service_auth,
                    repo_path,
                )
                .await;
                probes.authz = Some(authz_probe);
                probes.authz_snapshot = authz_snapshot;
            }

            let (admin_probe, admin_snapshot) =
                collect_cache_service_admin_snapshot(&client, base_url, &config.cache.service_auth)
                    .await;
            probes.admin_stats = Some(admin_probe);
            probes.admin_snapshot = admin_snapshot;

            let (metrics_probe, metrics_totals) =
                collect_cache_service_metrics_totals(&client, base_url).await;
            probes.metrics = Some(metrics_probe);
            probes.metrics_totals = metrics_totals;
        }
    }

    let signals = cache_service_support_signals(probes.admin_snapshot.as_ref());

    CacheServiceSupportBundle {
        collected_at_unix_ms,
        redacted: true,
        service,
        checks,
        probes,
        signals,
        runbooks: CACHE_SERVICE_RUNBOOKS.to_vec(),
        docs: CacheServiceSupportDocs {
            monitoring: CACHE_SERVICE_DOCS_MONITORING,
            runbooks: CACHE_SERVICE_DOCS_RUNBOOKS,
            troubleshooting: CACHE_SERVICE_DOCS_TROUBLESHOOTING,
        },
        recommended_commands: vec![
            "crab doctor --support-bundle --output cache-service-support.json",
            "crab doctor --json",
            "make cache-service-rustfs-smoke",
            "curl -fsS -H \"X-Cache-PSK: $CRAB_CACHE_PSK\" https://crab-cache.example.com:8443/v1/capabilities",
            "curl -fsS -H \"X-Cache-PSK: $CRAB_CACHE_PSK\" -H \"Content-Type: application/json\" -d '{\"repo_path\":\"org/repo\"}' https://crab-cache.example.com:8443/v1/authz/check",
            "curl -fsS -H \"X-Cache-PSK: $CRAB_CACHE_PSK\" https://crab-cache.example.com:8443/v1/admin/stats",
        ],
    }
}

fn cache_service_support_config(
    config: &Config,
    base_url: Option<&str>,
) -> CacheServiceSupportConfig {
    CacheServiceSupportConfig {
        configured: config.cache.service_url.is_some(),
        service_url: if config.cache.service_url.is_some() {
            "configured-redacted"
        } else {
            "not-configured"
        },
        scheme: base_url.and_then(cache_service_url_scheme),
        mode: config
            .cache
            .service_url
            .as_ref()
            .map(|_| cache_service_mode_label(config.cache.service_mode)),
        push_warming: config
            .cache
            .service_url
            .as_ref()
            .map(|_| config.cache.push_warming),
        auth: config
            .cache
            .service_url
            .as_ref()
            .map(|_| cache_service_auth_label(&config.cache.service_auth)),
        ca: config
            .cache
            .service_url
            .as_ref()
            .map(|_| cache_service_ca_label(config.cache.service_ca_cert.as_deref())),
        client_cert: config
            .cache
            .service_url
            .as_ref()
            .map(|_| cache_service_client_cert_label(config.cache.service_client_cert.as_deref())),
        config_error: None,
    }
}

fn cache_service_url_scheme(base_url: &str) -> Option<String> {
    reqwest::Url::parse(base_url)
        .ok()
        .map(|url| url.scheme().to_owned())
}

async fn read_cache_service_response_bounded(
    response: reqwest::Response,
) -> std::result::Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_CACHE_SERVICE_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "response exceeds the {MAX_CACHE_SERVICE_RESPONSE_BYTES}-byte safety limit"
        ));
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| "response body length overflow".to_owned())?;
        if next_len > MAX_CACHE_SERVICE_RESPONSE_BYTES {
            return Err(format!(
                "response exceeds the {MAX_CACHE_SERVICE_RESPONSE_BYTES}-byte safety limit"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn collect_cache_service_http_probe(
    client: &reqwest::Client,
    url: String,
    endpoint: &'static str,
    base_url: &str,
    auth: Option<&ServiceAuth>,
) -> CacheServiceHttpProbe {
    let mut request = client.get(&url);
    if let Some(auth) = auth {
        request = apply_cache_service_auth(request, auth);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            CacheServiceHttpProbe {
                endpoint,
                ok: status.is_success() || status == reqwest::StatusCode::NOT_FOUND,
                http_status: Some(status.as_u16()),
                error: None,
            }
        }
        Err(err) => CacheServiceHttpProbe {
            endpoint,
            ok: false,
            http_status: None,
            error: Some(redact_cache_service_message(base_url, &err.to_string())),
        },
    }
}

async fn collect_cache_service_capabilities_snapshot(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
) -> (CacheServiceHttpProbe, Option<CacheServiceCapabilitiesProbe>) {
    let capabilities_url = format!("{base_url}/v1/capabilities");
    let request = apply_cache_service_auth(client.get(&capabilities_url), auth);
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/capabilities",
                    ok: false,
                    http_status: None,
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return (
            CacheServiceHttpProbe {
                endpoint: "/v1/capabilities",
                ok: false,
                http_status: Some(status.as_u16()),
                error: None,
            },
            None,
        );
    }

    let body = match read_cache_service_response_bounded(response).await {
        Ok(body) => body,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/capabilities",
                    ok: false,
                    http_status: Some(status.as_u16()),
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    match serde_json::from_slice::<CacheServiceCapabilitiesProbe>(&body) {
        Ok(snapshot) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/capabilities",
                ok: cache_service_capabilities_are_valid(&snapshot),
                http_status: Some(status.as_u16()),
                error: None,
            },
            Some(snapshot),
        ),
        Err(err) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/capabilities",
                ok: false,
                http_status: Some(status.as_u16()),
                error: Some(format!(
                    "capabilities JSON did not match expected schema: {err}"
                )),
            },
            None,
        ),
    }
}

async fn collect_cache_service_authz_snapshot(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
    repo_path: &str,
) -> (CacheServiceHttpProbe, Option<CacheServiceAuthzProbe>) {
    let authz_url = format!("{base_url}/v1/authz/check");
    let request = apply_cache_service_auth(
        client
            .post(&authz_url)
            .json(&serde_json::json!({ "repo_path": repo_path })),
        auth,
    );
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/authz/check",
                    ok: false,
                    http_status: None,
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return (
            CacheServiceHttpProbe {
                endpoint: "/v1/authz/check",
                ok: false,
                http_status: Some(status.as_u16()),
                error: None,
            },
            None,
        );
    }

    let body = match read_cache_service_response_bounded(response).await {
        Ok(body) => body,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/authz/check",
                    ok: false,
                    http_status: Some(status.as_u16()),
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    match serde_json::from_slice::<CacheServiceAuthzProbe>(&body) {
        Ok(snapshot) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/authz/check",
                ok: cache_service_authz_has_schema(&snapshot),
                http_status: Some(status.as_u16()),
                error: None,
            },
            Some(snapshot),
        ),
        Err(err) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/authz/check",
                ok: false,
                http_status: Some(status.as_u16()),
                error: Some(format!(
                    "authz check JSON did not match expected schema: {err}"
                )),
            },
            None,
        ),
    }
}

async fn collect_cache_service_admin_snapshot(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
) -> (CacheServiceHttpProbe, Option<CacheServiceAdminProbe>) {
    let stats_url = format!("{base_url}/v1/admin/stats");
    let request = apply_cache_service_auth(client.get(&stats_url), auth);
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/admin/stats",
                    ok: false,
                    http_status: None,
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return (
            CacheServiceHttpProbe {
                endpoint: "/v1/admin/stats",
                ok: false,
                http_status: Some(status.as_u16()),
                error: None,
            },
            None,
        );
    }

    let body = match read_cache_service_response_bounded(response).await {
        Ok(body) => body,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/admin/stats",
                    ok: false,
                    http_status: Some(status.as_u16()),
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                None,
            );
        }
    };

    match serde_json::from_slice::<CacheServiceAdminProbe>(&body) {
        Ok(snapshot) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/admin/stats",
                ok: true,
                http_status: Some(status.as_u16()),
                error: None,
            },
            Some(snapshot),
        ),
        Err(err) => (
            CacheServiceHttpProbe {
                endpoint: "/v1/admin/stats",
                ok: false,
                http_status: Some(status.as_u16()),
                error: Some(format!(
                    "admin stats JSON did not match expected schema: {err}"
                )),
            },
            None,
        ),
    }
}

async fn collect_cache_service_metrics_totals(
    client: &reqwest::Client,
    base_url: &str,
) -> (CacheServiceHttpProbe, BTreeMap<String, f64>) {
    let metrics_url = format!("{base_url}/v1/metrics");
    let response = match client.get(&metrics_url).send().await {
        Ok(response) => response,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/metrics",
                    ok: false,
                    http_status: None,
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                BTreeMap::new(),
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return (
            CacheServiceHttpProbe {
                endpoint: "/v1/metrics",
                ok: false,
                http_status: Some(status.as_u16()),
                error: None,
            },
            BTreeMap::new(),
        );
    }

    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            return (
                CacheServiceHttpProbe {
                    endpoint: "/v1/metrics",
                    ok: false,
                    http_status: Some(status.as_u16()),
                    error: Some(redact_cache_service_message(base_url, &err.to_string())),
                },
                BTreeMap::new(),
            );
        }
    };

    (
        CacheServiceHttpProbe {
            endpoint: "/v1/metrics",
            ok: true,
            http_status: Some(status.as_u16()),
            error: None,
        },
        summarize_cache_service_metrics(&body),
    )
}

fn summarize_cache_service_metrics(body: &str) -> BTreeMap<String, f64> {
    const SELECTED: [&str; 19] = [
        "active_connections",
        "cache_bytes_served",
        "cache_bytes_stored",
        "cache_eviction_total",
        "cache_hit_total",
        "cache_inflight_misses",
        "cache_integrity_repair_total",
        "cache_max_bytes",
        "cache_max_object_bytes",
        "cache_miss_total",
        "dedup_chunks_known",
        "dedup_chunks_unknown",
        "dedup_query_total",
        "mutable_path_proxy_read_total",
        "mutable_path_proxy_stream_error_total",
        "origin_avoided_reads_total",
        "origin_fetch_bytes",
        "origin_fetch_total",
        "push_warming_total",
    ];

    let mut totals = BTreeMap::new();
    for line in body.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(series) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next() else {
            continue;
        };
        let metric = series.split_once('{').map_or(series, |(metric, _)| metric);
        if !SELECTED.contains(&metric) {
            continue;
        }
        let Ok(value) = value.parse::<f64>() else {
            continue;
        };
        *totals.entry(metric.to_owned()).or_insert(0.0) += value;
    }
    totals
}

fn cache_service_support_signals(
    stats: Option<&CacheServiceAdminProbe>,
) -> CacheServiceSupportSignals {
    let Some(stats) = stats else {
        return CacheServiceSupportSignals::default();
    };
    let traffic = stats.traffic.as_ref();
    let cache_hits = traffic.and_then(|traffic| traffic.cache_hits);
    let cache_misses = traffic.and_then(|traffic| traffic.cache_misses);
    let immutable_reads = match (cache_hits, cache_misses) {
        (Some(hits), Some(misses)) => Some(hits + misses),
        _ => None,
    };

    CacheServiceSupportSignals {
        cache_hit_rate: ratio(cache_hits, immutable_reads),
        origin_fallback_rate: ratio(
            traffic.and_then(|traffic| traffic.origin_fetches),
            immutable_reads,
        ),
        bytes_from_cache_rate: ratio(
            traffic.and_then(|traffic| traffic.bytes_served_from_cache),
            traffic.and_then(|traffic| traffic.bytes_served_total),
        ),
        integrity_repairs: Some(cache_service_integrity_repair_total(stats)),
        mutable_proxy_reads: traffic.and_then(|traffic| traffic.mutable_proxy_reads),
        push_warming_writes: traffic.and_then(|traffic| traffic.push_warming_writes),
        evicted_objects: stats.eviction.as_ref().map(|eviction| eviction.total),
    }
}

fn ratio(numerator: Option<u64>, denominator: Option<u64>) -> Option<f64> {
    let (Some(numerator), Some(denominator)) = (numerator, denominator) else {
        return None;
    };
    if denominator == 0 {
        return None;
    }
    Some(numerator as f64 / denominator as f64)
}

fn cache_service_integrity_repair_total(stats: &CacheServiceAdminProbe) -> u64 {
    stats.startup_integrity.as_ref().map_or(0, |integrity| {
        integrity.metadata_entries_removed
            + integrity.metadata_size_corrections
            + integrity.unindexed_objects_indexed
            + integrity.unindexed_paths_removed
    }) + stats.runtime_integrity.as_ref().map_or(0, |integrity| {
        integrity.missing_files_repaired
            + integrity.invalid_objects_evicted
            + integrity.metadata_entries_recreated
    })
}

fn redact_cache_service_checks(checks: &mut [CheckResult], base_url: Option<&str>) {
    let Some(base_url) = base_url else {
        return;
    };
    for check in checks {
        check.detail = redact_cache_service_message(base_url, &check.detail);
    }
}

fn redact_cache_service_message(base_url: &str, message: &str) -> String {
    message.replace(base_url, "<cache-service-url-redacted>")
}

fn print_json_bundle<T: Serialize>(payload: &T) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer_pretty(&mut lock, payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}

fn write_json_bundle<T: Serialize>(path: &Path, payload: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file(), payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct CacheServiceCapabilitiesProbe {
    schema: Option<String>,
    limits: Option<CacheServiceLimitsProbe>,
    routes: Option<CacheRouteContract>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceAuthzProbe {
    schema: Option<String>,
    repo_path: Option<String>,
    policy_configured: Option<bool>,
    actions: Option<CacheServiceAuthzActionsProbe>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceAuthzActionsProbe {
    read: Option<bool>,
    write: Option<bool>,
    dedup: Option<bool>,
    admin: Option<bool>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceAdminProbe {
    total_bytes: Option<u64>,
    max_bytes: Option<u64>,
    limits: Option<CacheServiceLimitsProbe>,
    xorb_count: Option<u64>,
    shard_count: Option<u64>,
    pack_count: Option<u64>,
    metadata_count: Option<u64>,
    eviction: Option<CacheServiceEvictionProbe>,
    startup_integrity: Option<CacheServiceStartupIntegrityProbe>,
    runtime_integrity: Option<CacheServiceRuntimeIntegrityProbe>,
    traffic: Option<CacheServiceTrafficProbe>,
    dedup_index: Option<CacheServiceDedupProbe>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceLimitsProbe {
    max_cache_bytes: Option<u64>,
    max_object_bytes: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceEvictionProbe {
    total: u64,
    xorb: u64,
    shard: u64,
    pack: u64,
    pack_index: u64,
    metadata: u64,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceStartupIntegrityProbe {
    metadata_entries_removed: u64,
    metadata_size_corrections: u64,
    unindexed_objects_indexed: u64,
    unindexed_paths_removed: u64,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceRuntimeIntegrityProbe {
    missing_files_repaired: u64,
    invalid_objects_evicted: u64,
    metadata_entries_recreated: u64,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceTrafficProbe {
    cache_hits: Option<u64>,
    cache_misses: Option<u64>,
    origin_avoided_reads: Option<u64>,
    origin_fetches: Option<u64>,
    bytes_served_from_cache: Option<u64>,
    bytes_served_total: Option<u64>,
    push_warming_writes: Option<u64>,
    mutable_proxy_reads: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceDedupProbe {
    indexed_chunks: u64,
    scope: String,
    requires_repo_context: bool,
    startup_rebuild: Option<CacheServiceDedupRebuildProbe>,
    last_ingestion_error: Option<serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct CacheServiceDedupRebuildProbe {
    status: String,
    entries: u64,
    error: Option<String>,
}

/// Check the optional organization cache service configuration and live auth.
async fn check_cache_service(root: &Path, active_probe: bool) -> Vec<CheckResult> {
    let config = match Config::resolve_for_repo(root) {
        Ok(c) => c,
        Err(e) => {
            return vec![CheckResult::warn(
                "cache service",
                format!("could not load config: {e}"),
            )];
        }
    };

    let Some(base_url) = config.cache.service_url.as_deref() else {
        return vec![CheckResult::ok("cache service", "not configured")];
    };
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return vec![CheckResult::fail(
            "cache service",
            "cache.service_url is empty",
        )];
    }

    let client = match cache_service_http_client(
        config.cache.service_ca_cert.as_deref(),
        config.cache.service_client_cert.as_deref(),
        config.cache.service_client_key.as_deref(),
    ) {
        Ok(client) => client,
        Err(e) => return vec![CheckResult::fail("cache service", e)],
    };
    let mut results = Vec::new();

    let health_url = format!("{base_url}/v1/health");
    match client.get(&health_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            results.push(CheckResult::ok(
                "cache service",
                format!(
                    "{} health ok ({}, {}, {}, {}, {})",
                    base_url,
                    cache_service_mode_label(config.cache.service_mode),
                    cache_service_push_warming_label(config.cache.push_warming),
                    cache_service_auth_label(&config.cache.service_auth),
                    cache_service_ca_label(config.cache.service_ca_cert.as_deref()),
                    cache_service_client_cert_label(config.cache.service_client_cert.as_deref())
                ),
            ));
        }
        Ok(resp) => {
            return vec![CheckResult::fail(
                "cache service",
                format!("{} unhealthy: HTTP {}", base_url, resp.status().as_u16()),
            )];
        }
        Err(e) => {
            return vec![CheckResult::fail(
                "cache service",
                format!("{base_url} unreachable: {e}"),
            )];
        }
    }

    let probe_url = format!(
        "{base_url}/v1/.crab/xorbs/00/0000000000000000000000000000000000000000000000000000000000000000"
    );
    let req = apply_cache_service_auth(client.get(&probe_url), &config.cache.service_auth);
    results.push(match req.send().await {
        Ok(resp) => cache_service_auth_probe_result(
            base_url,
            config.cache.service_mode,
            &config.cache.service_auth,
            resp.status(),
        ),
        Err(e) => CheckResult::fail(
            "cache service auth",
            format!("{base_url} auth probe failed: {e}"),
        ),
    });

    results.push(
        check_cache_service_capabilities(&client, base_url, &config.cache.service_auth).await,
    );

    results.push(
        check_cache_service_authz(
            root,
            &client,
            base_url,
            &config.cache.service_auth,
            config.cache.service_mode,
            config.cache.push_warming,
        )
        .await,
    );

    results.push(
        check_cache_service_admin(
            &client,
            base_url,
            &config.cache.service_auth,
            config.cache.service_mode,
        )
        .await,
    );
    if active_probe {
        results.push(
            check_cache_service_active_probe(root, &client, base_url, &config.cache.service_auth)
                .await,
        );
    }
    results
}

fn cache_service_auth_probe_result(
    base_url: &str,
    mode: ServiceMode,
    auth: &ServiceAuth,
    status: reqwest::StatusCode,
) -> CheckResult {
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        return CheckResult::ok(
            "cache service auth",
            format!(
                "{} object route reachable ({}, {})",
                base_url,
                cache_service_mode_label(mode),
                cache_service_auth_label(auth)
            ),
        );
    }

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return CheckResult::fail(
            "cache service auth",
            format!(
                "{} rejected cache credentials with HTTP {}; {}",
                base_url,
                status.as_u16(),
                cache_service_auth_failure_hint(auth)
            ),
        );
    }

    if status.is_server_error() {
        return CheckResult::fail(
            "cache service auth",
            format!(
                "{} object-route auth probe returned HTTP {}",
                base_url,
                status.as_u16()
            ),
        );
    }

    CheckResult::warn(
        "cache service auth",
        format!(
            "{} object-route auth probe returned HTTP {} ({}, {})",
            base_url,
            status.as_u16(),
            cache_service_mode_label(mode),
            cache_service_auth_label(auth)
        ),
    )
}

async fn check_cache_service_capabilities(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
) -> CheckResult {
    let (probe, snapshot) =
        collect_cache_service_capabilities_snapshot(client, base_url, auth).await;

    if let Some(error) = probe.error {
        return CheckResult::fail("cache service caps", error);
    }

    let Some(status) = probe.http_status else {
        return CheckResult::fail("cache service caps", "capabilities probe did not complete");
    };

    if status == reqwest::StatusCode::UNAUTHORIZED.as_u16()
        || status == reqwest::StatusCode::FORBIDDEN.as_u16()
    {
        return CheckResult::fail(
            "cache service caps",
            format!(
                "capabilities rejected cache credentials with HTTP {}; {}",
                status,
                cache_service_auth_failure_hint(auth)
            ),
        );
    }
    if status == reqwest::StatusCode::NOT_FOUND.as_u16() {
        return CheckResult::fail(
            "cache service caps",
            "capabilities endpoint missing; upgrade crab-cache-server",
        );
    }
    if !(200..300).contains(&status) {
        return CheckResult::fail(
            "cache service caps",
            format!("capabilities returned HTTP {status}"),
        );
    }

    let Some(snapshot) = snapshot else {
        return CheckResult::fail(
            "cache service caps",
            "capabilities response missing parsed body",
        );
    };
    cache_service_capabilities_result(&snapshot)
}

fn cache_service_capabilities_result(snapshot: &CacheServiceCapabilitiesProbe) -> CheckResult {
    let Some(schema) = snapshot.schema.as_deref() else {
        return CheckResult::fail("cache service caps", "capabilities schema missing");
    };
    if schema != CACHE_SERVICE_CAPABILITIES_SCHEMA {
        return CheckResult::fail(
            "cache service caps",
            format!("unexpected capabilities schema {schema}"),
        );
    }

    let Some(limits) = snapshot.limits.as_ref() else {
        return CheckResult::fail("cache service caps", "capabilities limits missing");
    };
    let Some(max_cache_bytes) = limits.max_cache_bytes else {
        return CheckResult::fail("cache service caps", "max_cache_bytes missing");
    };
    let Some(max_object_bytes) = limits.max_object_bytes else {
        return CheckResult::fail("cache service caps", "max_object_bytes missing");
    };
    if max_cache_bytes == 0 {
        return CheckResult::fail("cache service caps", "max_cache_bytes must be positive");
    }
    if max_object_bytes == 0 {
        return CheckResult::fail("cache service caps", "max_object_bytes must be positive");
    }

    let Some(routes) = snapshot.routes.as_ref() else {
        return CheckResult::fail(
            "cache service caps",
            "capabilities route contract missing; upgrade crab-cache-server",
        );
    };
    if !cache_route_contract_matches_current(routes) {
        return CheckResult::fail(
            "cache service caps",
            "capabilities route contract does not match this crab build",
        );
    }

    CheckResult::ok(
        "cache service caps",
        format!(
            "schema v1, cache limit {}, object limit {}, route contract current",
            format_bytes(max_cache_bytes),
            format_bytes(max_object_bytes)
        ),
    )
}

fn cache_service_capabilities_are_valid(snapshot: &CacheServiceCapabilitiesProbe) -> bool {
    snapshot.schema.as_deref() == Some(CACHE_SERVICE_CAPABILITIES_SCHEMA)
        && snapshot
            .limits
            .as_ref()
            .and_then(|limits| limits.max_cache_bytes)
            .is_some_and(|bytes| bytes > 0)
        && snapshot
            .limits
            .as_ref()
            .and_then(|limits| limits.max_object_bytes)
            .is_some_and(|bytes| bytes > 0)
        && snapshot
            .routes
            .as_ref()
            .is_some_and(cache_route_contract_matches_current)
}

async fn check_cache_service_authz(
    root: &Path,
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
    mode: ServiceMode,
    push_warming: bool,
) -> CheckResult {
    let repo_path = match cache_service_repo_path(root) {
        Ok(repo_path) => repo_path,
        Err(detail) => return CheckResult::warn("cache service authz", detail),
    };
    let (probe, snapshot) =
        collect_cache_service_authz_snapshot(client, base_url, auth, &repo_path).await;

    if let Some(error) = probe.error {
        return CheckResult::fail("cache service authz", error);
    }

    let Some(status) = probe.http_status else {
        return CheckResult::fail("cache service authz", "authz probe did not complete");
    };
    if status == reqwest::StatusCode::UNAUTHORIZED.as_u16()
        || status == reqwest::StatusCode::FORBIDDEN.as_u16()
    {
        return CheckResult::fail(
            "cache service authz",
            format!(
                "authz check rejected cache credentials with HTTP {}; {}",
                status,
                cache_service_auth_failure_hint(auth)
            ),
        );
    }
    if status == reqwest::StatusCode::NOT_FOUND.as_u16() {
        return CheckResult::fail(
            "cache service authz",
            "authz check endpoint missing; upgrade crab-cache-server",
        );
    }
    if !(200..300).contains(&status) {
        return CheckResult::fail(
            "cache service authz",
            format!("authz check returned HTTP {status}"),
        );
    }

    let Some(snapshot) = snapshot else {
        return CheckResult::fail("cache service authz", "authz response missing parsed body");
    };
    cache_service_authz_result(mode, push_warming, &snapshot)
}

fn cache_service_authz_result(
    mode: ServiceMode,
    push_warming: bool,
    snapshot: &CacheServiceAuthzProbe,
) -> CheckResult {
    if !cache_service_authz_has_schema(snapshot) {
        return CheckResult::fail("cache service authz", "unexpected authz check schema");
    }
    let Some(actions) = snapshot.actions.as_ref() else {
        return CheckResult::fail("cache service authz", "authz actions missing");
    };
    let read = actions.read.unwrap_or(false);
    let write = actions.write.unwrap_or(false);
    let dedup = actions.dedup.unwrap_or(false);
    let admin = actions.admin.unwrap_or(false);

    let mut missing = Vec::new();
    if mode.cache_reads_enabled() && !read {
        missing.push("read");
    }
    if push_warming && !write {
        missing.push("write");
    }
    if mode.dedup_enabled() && !dedup {
        missing.push("dedup");
    }

    let repo_path = snapshot.repo_path.as_deref().unwrap_or("unknown");
    let policy = if snapshot.policy_configured == Some(true) {
        "policy configured"
    } else {
        "no policy"
    };
    let detail = format!(
        "repo {repo_path}, {policy}; read {read}, write {write}, dedup {dedup}, admin {admin}"
    );
    if !missing.is_empty() {
        return CheckResult::fail(
            "cache service authz",
            format!(
                "{detail}; missing required action(s): {}",
                missing.join(",")
            ),
        );
    }
    if !admin {
        return CheckResult::warn(
            "cache service authz",
            format!("{detail}; admin denied, support bundle and active probe need admin"),
        );
    }
    CheckResult::ok("cache service authz", detail)
}

fn cache_service_authz_has_schema(snapshot: &CacheServiceAuthzProbe) -> bool {
    snapshot.schema.as_deref() == Some(CACHE_SERVICE_AUTHZ_SCHEMA)
}

async fn check_cache_service_active_probe(
    root: &Path,
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
) -> CheckResult {
    let probe = match build_cache_service_active_probe(root) {
        Ok(probe) => probe,
        Err(detail) => return CheckResult::fail("cache service active", detail),
    };

    let outcome = match active_probe::run_active_probe(
        client,
        base_url,
        active_probe_auth(auth),
        cache_service_auth_failure_hint(auth),
        &probe,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(detail) => return CheckResult::fail("cache service active", detail),
    };

    CheckResult::ok(
        "cache service active",
        format!(
            "write/read/range/cleanup ok ({} B probe, {} B evicted)",
            outcome.body_len, outcome.evicted_bytes
        ),
    )
}

fn build_cache_service_active_probe(root: &Path) -> std::result::Result<ActiveProbeObject, String> {
    let repo_path = cache_service_repo_path(root)?;
    Ok(active_probe::build_active_probe(
        &repo_path,
        "crab-doctor-probe",
        "crab doctor cache-service active probe",
    ))
}

fn active_probe_auth(auth: &ServiceAuth) -> ActiveProbeAuth<'_> {
    match auth {
        ServiceAuth::None => ActiveProbeAuth::None,
        ServiceAuth::Psk(psk) => ActiveProbeAuth::Psk(psk),
        ServiceAuth::Bearer(token) => ActiveProbeAuth::Bearer(token),
        ServiceAuth::Mtls => ActiveProbeAuth::Mtls,
    }
}

async fn check_cache_service_admin(
    client: &reqwest::Client,
    base_url: &str,
    auth: &ServiceAuth,
    mode: ServiceMode,
) -> CheckResult {
    let stats_url = format!("{base_url}/v1/admin/stats");
    let req = apply_cache_service_auth(client.get(&stats_url), auth);
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return CheckResult::warn(
                "cache service admin",
                format!("{base_url} admin stats probe failed: {e}"),
            );
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return CheckResult::warn(
            "cache service admin",
            "admin stats not authorized; grant admin to this credential for full readiness checks",
        );
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return CheckResult::warn(
            "cache service admin",
            "admin stats endpoint missing; upgrade crab-cache-server",
        );
    }
    if !status.is_success() {
        return CheckResult::warn(
            "cache service admin",
            format!("admin stats returned HTTP {}", status.as_u16()),
        );
    }

    let body = match read_cache_service_response_bounded(resp).await {
        Ok(body) => body,
        Err(e) => {
            return CheckResult::warn(
                "cache service admin",
                format!("admin stats body read failed: {e}"),
            );
        }
    };
    let stats: CacheServiceAdminProbe = match serde_json::from_slice(&body) {
        Ok(stats) => stats,
        Err(e) => {
            return CheckResult::warn(
                "cache service admin",
                format!("admin stats JSON did not match expected schema: {e}"),
            );
        }
    };

    cache_service_admin_stats_result(mode, &stats)
}

fn cache_service_admin_stats_result(
    mode: ServiceMode,
    stats: &CacheServiceAdminProbe,
) -> CheckResult {
    let max_cache_bytes = stats
        .limits
        .as_ref()
        .and_then(|limits| limits.max_cache_bytes)
        .or(stats.max_bytes);
    let cache_detail = format!(
        "cache {} / {}, object limit {}, xorbs {}, shards {}, metadata {}",
        stats
            .total_bytes
            .map_or_else(|| "unknown".to_owned(), format_bytes),
        max_cache_bytes.map_or_else(|| "unknown".to_owned(), format_bytes),
        stats
            .limits
            .as_ref()
            .and_then(|limits| limits.max_object_bytes)
            .map_or_else(|| "unknown".to_owned(), format_bytes),
        stats
            .xorb_count
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
        stats
            .shard_count
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
        stats
            .metadata_count
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string()),
    );

    let Some(dedup) = &stats.dedup_index else {
        if cache_service_mode_uses_dedup(mode) {
            return CheckResult::warn(
                "cache service admin",
                format!("{cache_detail}; dedup stats missing"),
            );
        }
        return CheckResult::ok("cache service admin", cache_detail);
    };

    let repo_context = if dedup.requires_repo_context {
        "repo-scoped"
    } else {
        "global"
    };
    let rebuild_detail = dedup.startup_rebuild.as_ref().map_or_else(
        || "rebuild unknown".to_owned(),
        |rebuild| {
            format!(
                "rebuild {}, entries {}{}",
                rebuild.status,
                rebuild.entries,
                rebuild
                    .error
                    .as_ref()
                    .map(|e| format!(", error {e}"))
                    .unwrap_or_default()
            )
        },
    );
    let detail = format!(
        "{cache_detail}; dedup {} chunks, scope {}, {}, {}",
        dedup.indexed_chunks, dedup.scope, repo_context, rebuild_detail
    );

    if let Some(rebuild) = &dedup.startup_rebuild
        && rebuild.status != "ok"
    {
        return CheckResult::warn("cache service admin", detail);
    }
    if dedup.last_ingestion_error.is_some() {
        return CheckResult::warn(
            "cache service admin",
            format!("{detail}; last shard ingestion error recorded"),
        );
    }

    CheckResult::ok("cache service admin", detail)
}

fn cache_service_http_client(
    ca_cert: Option<&Path>,
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
) -> std::result::Result<reqwest::Client, String> {
    build_cache_service_http_client(
        CACHE_SERVICE_DOCTOR_HTTP_TIMEOUT,
        ca_cert,
        client_cert,
        client_key,
    )
    .map_err(|e| e.to_string())
}

fn apply_cache_service_auth(
    req: reqwest::RequestBuilder,
    auth: &ServiceAuth,
) -> reqwest::RequestBuilder {
    match auth {
        ServiceAuth::None | ServiceAuth::Mtls => req,
        ServiceAuth::Psk(psk) => req.header("x-cache-psk", psk.as_str()),
        ServiceAuth::Bearer(token) => req.bearer_auth(token),
    }
}

fn cache_service_auth_label(auth: &ServiceAuth) -> &'static str {
    match auth {
        ServiceAuth::None => "auth none",
        ServiceAuth::Psk(_) if std::env::var_os("CRAB_CACHE_PSK").is_some() => {
            "psk via CRAB_CACHE_PSK"
        }
        ServiceAuth::Psk(_) => "psk configured",
        ServiceAuth::Bearer(_) if std::env::var_os("CRAB_CACHE_TOKEN").is_some() => {
            "bearer via CRAB_CACHE_TOKEN"
        }
        ServiceAuth::Bearer(_) => "bearer configured",
        ServiceAuth::Mtls => "mtls client cert configured",
    }
}

fn cache_service_auth_failure_hint(auth: &ServiceAuth) -> &'static str {
    match auth {
        ServiceAuth::Mtls => {
            "check cache.service_client_cert, cache.service_client_key, and authorization policy"
        }
        _ => "set CRAB_CACHE_PSK/CRAB_CACHE_TOKEN or fix [cache] auth",
    }
}

fn cache_service_ca_label(ca_cert: Option<&Path>) -> &'static str {
    if ca_cert.is_some() {
        "custom CA"
    } else {
        "system CA"
    }
}

fn cache_service_client_cert_label(client_cert: Option<&Path>) -> &'static str {
    if client_cert.is_some() {
        "client cert configured"
    } else {
        "no client cert"
    }
}

fn cache_service_mode_label(mode: ServiceMode) -> &'static str {
    mode.as_str()
}

fn cache_service_mode_uses_dedup(mode: ServiceMode) -> bool {
    mode.dedup_enabled()
}

fn cache_service_push_warming_label(enabled: bool) -> &'static str {
    if enabled {
        "push warming on"
    } else {
        "push warming off"
    }
}

/// Check the required_cli_version guard from the remote config.
fn check_version_guard(root: &Path) -> CheckResult {
    let config_path = root.join(".crab/local.toml");
    if !config_path.exists() {
        return CheckResult::ok("version guard", "no config (skipped)");
    }

    match Config::resolve_local() {
        Ok(config) => match config.check_version_guard() {
            Ok(()) => CheckResult::ok(
                "version guard",
                "binary version satisfies remote requirement",
            ),
            Err(e) => CheckResult::fail("version guard", format!("{e}")),
        },
        Err(_) => CheckResult::ok("version guard", "could not load config (skipped)"),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crab_cache::path_class::cache_route_contract;
    use crab_staging::stream::{StreamStageProgress, stage_file_streaming_as};
    use crab_staging::{PublicationIntentEntry, StagingArea, StagingAreaReadOnly};

    #[test]
    fn check_git_version_succeeds() {
        let result = check_git_version();
        // Git should be installed in the test environment.
        assert_ne!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn check_git_repo_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_git_repo(dir.path());
        assert_eq!(result.status, CheckStatus::Warn);
    }

    #[test]
    fn check_git_repo_inside_repo() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let result = check_git_repo(dir.path());
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[test]
    fn check_filter_driver_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let result = check_filter_driver(dir.path());
        // If the user has crab installed globally, the git drivers
        // will be found even in a fresh repo. Both Ok and Fail are valid
        // depending on the test environment.
        assert!(
            result.status == CheckStatus::Fail || result.status == CheckStatus::Ok,
            "expected Fail or Ok, got {:?}: {}",
            result.status,
            result.detail,
        );
    }

    #[test]
    fn check_gitattributes_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_gitattributes(dir.path());
        assert_eq!(result.status, CheckStatus::Warn);
    }

    #[test]
    fn check_gitattributes_with_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let result = check_gitattributes(dir.path());
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("1 crab pattern"));
    }

    #[test]
    fn check_gitattributes_no_crab_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "*.txt text\n").unwrap();

        let result = check_gitattributes(dir.path());
        assert_eq!(result.status, CheckStatus::Warn);
    }

    #[test]
    fn check_remote_url_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_remote_url(dir.path());
        assert_eq!(result.status, CheckStatus::Warn);
    }

    #[test]
    fn check_remote_url_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "[remote]\nurl = \"crab://bucket/repo\"\n",
        )
        .unwrap();

        let result = check_remote_url(dir.path());
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("crab://bucket/repo"));
    }

    #[test]
    fn check_remote_url_invalid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "[remote]\nurl = \"https://not-crab\"\n",
        )
        .unwrap();

        let result = check_remote_url(dir.path());
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn bucket_not_found_names_the_bucket_and_next_action() {
        let result = remote_access_failure(
            "team-data",
            None,
            &CrabError::NotFound {
                path: "models".into(),
            },
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("bucket 'team-data' not found"));
        assert!(result.detail.contains("create it"));
    }

    #[test]
    fn repository_not_found_is_distinct_from_bucket_not_found() {
        let result = remote_access_failure(
            "team-data",
            Some("models"),
            &CrabError::NotFound {
                path: "models/layout".into(),
            },
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(
            result
                .detail
                .contains("repository 'models' in bucket 'team-data' is not initialized")
        );
        assert!(result.detail.contains("crab configure"));
    }

    #[test]
    fn permission_failure_names_the_denied_scope() {
        let result = remote_access_failure(
            "team-data",
            Some("models"),
            &CrabError::Forbidden {
                path: "models/layout".into(),
            },
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("repository 'models'"));
        assert!(result.detail.contains("active identity"));
    }

    #[tokio::test]
    async fn check_staging_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_staging(dir.path()).await;
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_staging_distinguishes_safe_snapshot_pin_from_leak() {
        let dir = tempfile::tempdir().unwrap();
        let staging_root = dir.path().join(".crab/staging");
        let logical_path = Path::new("models/model.bin");
        let first_source = dir.path().join("first-source.bin");
        let second_source = dir.path().join("second-source.bin");
        std::fs::write(&first_source, vec![0x41; 256 * 1024]).unwrap();
        std::fs::write(&second_source, vec![0x42; 256 * 1024]).unwrap();

        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let first = stage_file_streaming_as(
            &first_source,
            dir.path(),
            logical_path,
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&first.batch_id).unwrap();
        staging.close().await.unwrap();

        let reader = StagingAreaReadOnly::open(staging_root.clone())
            .await
            .unwrap();
        reader
            .create_push_snapshot("doctor-push", std::slice::from_ref(&first.recipe))
            .unwrap();
        drop(reader);

        let staging = StagingArea::open(staging_root).await.unwrap();
        let second = stage_file_streaming_as(
            &second_source,
            dir.path(),
            logical_path,
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.mark_batch_published(&second.batch_id).unwrap();
        staging.close().await.unwrap();

        let result = check_staging(dir.path()).await;
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("1 superseded lease pin"));
        assert!(result.detail.contains("retaining"));
        assert!(result.detail.contains("safely"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_staging_reports_unpublished_batch_and_ambiguous_intent() {
        let dir = tempfile::tempdir().unwrap();
        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        staging.create_batch().unwrap();
        staging.close().await.unwrap();

        let result = check_staging(dir.path()).await;
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("unpublished staging batch"));

        let source = dir.path().join("intent-source.bin");
        std::fs::write(&source, vec![0x51; 128 * 1024]).unwrap();
        let staging = StagingArea::open(staging_root).await.unwrap();
        let staged = stage_file_streaming_as(
            &source,
            dir.path(),
            Path::new("models/intent.bin"),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging
            .create_publication_intent(&[PublicationIntentEntry {
                batch_id: staged.batch_id,
                path: PathBuf::from("models/intent.bin"),
                expected_pointer_oid: "expected-pointer".to_owned(),
                previous_index_state: "absent".to_owned(),
            }])
            .unwrap();
        staging.close().await.unwrap();

        let result = check_staging(dir.path()).await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("unresolved Git-index publication"));
        assert!(result.detail.contains("rerun `crab add`"));
    }

    #[tokio::test]
    async fn check_cache_nonexistent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing");
        let report = inspect_cache(&root, 1024, &CancellationToken::new())
            .await
            .unwrap();
        let checks = cache_checks(&report);
        assert_eq!(checks[0].status, CheckStatus::Ok);
        assert!(!root.exists());
    }

    #[test]
    fn cache_service_auth_label_redacts_secret_values() {
        let psk = cache_service_auth_label(&ServiceAuth::Psk("super-secret-psk".to_string()));
        assert!(psk.starts_with("psk"));
        assert!(!psk.contains("super-secret-psk"));

        let bearer = cache_service_auth_label(&ServiceAuth::Bearer("secret-token".to_string()));
        assert!(bearer.starts_with("bearer"));
        assert!(!bearer.contains("secret-token"));
    }

    #[test]
    fn cache_service_mode_label_matches_config_values() {
        assert_eq!(cache_service_mode_label(ServiceMode::Cache), "cache");
        assert_eq!(cache_service_mode_label(ServiceMode::Dedup), "dedup");
        assert_eq!(
            cache_service_mode_label(ServiceMode::CacheAndDedup),
            "cache+dedup"
        );
    }

    #[test]
    fn cache_service_doctor_timeout_covers_readiness_probe_budget() {
        assert!(CACHE_SERVICE_DOCTOR_HTTP_TIMEOUT > Duration::from_secs(3));
    }

    #[test]
    fn cache_service_auth_probe_accepts_not_found_as_reachable() {
        let result = cache_service_auth_probe_result(
            "http://cache.local",
            ServiceMode::CacheAndDedup,
            &ServiceAuth::None,
            reqwest::StatusCode::NOT_FOUND,
        );

        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("object route reachable"));
    }

    #[test]
    fn cache_service_auth_probe_rejects_forbidden_credentials() {
        let result = cache_service_auth_probe_result(
            "http://cache.local",
            ServiceMode::CacheAndDedup,
            &ServiceAuth::Psk("secret".to_owned()),
            reqwest::StatusCode::FORBIDDEN,
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(!result.detail.contains("secret"));
        assert!(result.detail.contains("rejected cache credentials"));
    }

    #[test]
    fn cache_service_auth_probe_uses_mtls_failure_hint() {
        let result = cache_service_auth_probe_result(
            "https://cache.local",
            ServiceMode::CacheAndDedup,
            &ServiceAuth::Mtls,
            reqwest::StatusCode::FORBIDDEN,
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("service_client_cert"));
        assert!(!result.detail.contains("CRAB_CACHE_PSK"));
    }

    #[test]
    fn cache_service_capabilities_ok_reports_limits() {
        let result = cache_service_capabilities_result(&CacheServiceCapabilitiesProbe {
            schema: Some(CACHE_SERVICE_CAPABILITIES_SCHEMA.to_owned()),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(8192),
            }),
            routes: Some(cache_route_contract()),
        });

        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("schema v1"));
        assert!(result.detail.contains("cache limit 4.0 KB"));
        assert!(result.detail.contains("object limit 8.0 KB"));
    }

    #[test]
    fn cache_service_capabilities_rejects_wrong_schema() {
        let result = cache_service_capabilities_result(&CacheServiceCapabilitiesProbe {
            schema: Some("wrong".to_owned()),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(8192),
            }),
            routes: Some(cache_route_contract()),
        });

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("unexpected capabilities schema"));
    }

    #[test]
    fn cache_service_capabilities_rejects_missing_limits() {
        let result = cache_service_capabilities_result(&CacheServiceCapabilitiesProbe {
            schema: Some(CACHE_SERVICE_CAPABILITIES_SCHEMA.to_owned()),
            limits: None,
            routes: Some(cache_route_contract()),
        });

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("limits missing"));
    }

    #[test]
    fn cache_service_capabilities_rejects_zero_object_limit() {
        let snapshot = CacheServiceCapabilitiesProbe {
            schema: Some(CACHE_SERVICE_CAPABILITIES_SCHEMA.to_owned()),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(0),
            }),
            routes: Some(cache_route_contract()),
        };

        assert!(!cache_service_capabilities_are_valid(&snapshot));
        let result = cache_service_capabilities_result(&snapshot);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("max_object_bytes must be positive"));
    }

    #[test]
    fn cache_service_capabilities_rejects_missing_route_contract() {
        let snapshot = CacheServiceCapabilitiesProbe {
            schema: Some(CACHE_SERVICE_CAPABILITIES_SCHEMA.to_owned()),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(8192),
            }),
            routes: None,
        };

        assert!(!cache_service_capabilities_are_valid(&snapshot));
        let result = cache_service_capabilities_result(&snapshot);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("route contract missing"));
    }

    fn authz_probe(read: bool, write: bool, dedup: bool, admin: bool) -> CacheServiceAuthzProbe {
        CacheServiceAuthzProbe {
            schema: Some(CACHE_SERVICE_AUTHZ_SCHEMA.to_string()),
            repo_path: Some("org/repo".to_string()),
            policy_configured: Some(true),
            actions: Some(CacheServiceAuthzActionsProbe {
                read: Some(read),
                write: Some(write),
                dedup: Some(dedup),
                admin: Some(admin),
            }),
        }
    }

    #[test]
    fn cache_service_authz_ok_reports_action_matrix() {
        let result = cache_service_authz_result(
            ServiceMode::CacheAndDedup,
            true,
            &authz_probe(true, true, true, true),
        );

        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("read true"));
        assert!(result.detail.contains("write true"));
        assert!(result.detail.contains("dedup true"));
        assert!(result.detail.contains("admin true"));
    }

    #[test]
    fn cache_service_authz_warns_when_only_admin_missing() {
        let result = cache_service_authz_result(
            ServiceMode::CacheAndDedup,
            true,
            &authz_probe(true, true, true, false),
        );

        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("admin denied"));
    }

    #[test]
    fn cache_service_authz_fails_when_required_action_missing() {
        let result = cache_service_authz_result(
            ServiceMode::CacheAndDedup,
            true,
            &authz_probe(true, false, true, true),
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("missing required action(s): write"));
    }

    #[test]
    fn cache_service_active_probe_path_uses_remote_repo_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "[remote]\nurl = \"crab://bucket/org/repo\"\n",
        )
        .unwrap();

        let probe = build_cache_service_active_probe(dir.path()).unwrap();

        assert!(probe.path.starts_with("org/repo/packs/crab-doctor-probe-"));
        assert!(probe.path.ends_with(".pack"));
        assert!(
            probe
                .body
                .starts_with(b"crab doctor cache-service active probe\n")
        );
    }

    #[test]
    fn cache_service_active_probe_requires_remote() {
        let dir = tempfile::tempdir().unwrap();

        let err = build_cache_service_active_probe(dir.path()).unwrap_err();

        assert!(err.contains("crab.toml"));
    }

    #[test]
    fn cache_service_admin_stats_ok_reports_cache_and_dedup_state() {
        let stats = CacheServiceAdminProbe {
            total_bytes: Some(2048),
            max_bytes: Some(4096),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(8192),
            }),
            xorb_count: Some(2),
            shard_count: Some(1),
            pack_count: Some(0),
            metadata_count: Some(3),
            eviction: None,
            startup_integrity: None,
            runtime_integrity: None,
            traffic: None,
            dedup_index: Some(CacheServiceDedupProbe {
                indexed_chunks: 9,
                scope: "repos:org/repo".to_owned(),
                requires_repo_context: true,
                startup_rebuild: Some(CacheServiceDedupRebuildProbe {
                    status: "ok".to_owned(),
                    entries: 9,
                    error: None,
                }),
                last_ingestion_error: None,
            }),
        };

        let result = cache_service_admin_stats_result(ServiceMode::CacheAndDedup, &stats);

        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.detail.contains("2.0 KB / 4.0 KB"));
        assert!(result.detail.contains("object limit 8.0 KB"));
        assert!(result.detail.contains("dedup 9 chunks"));
        assert!(result.detail.contains("repo-scoped"));
    }

    #[test]
    fn cache_service_admin_stats_warns_on_dedup_rebuild_failure() {
        let stats = CacheServiceAdminProbe {
            total_bytes: Some(0),
            max_bytes: Some(4096),
            limits: None,
            xorb_count: Some(0),
            shard_count: Some(0),
            pack_count: Some(0),
            metadata_count: Some(0),
            eviction: None,
            startup_integrity: None,
            runtime_integrity: None,
            traffic: None,
            dedup_index: Some(CacheServiceDedupProbe {
                indexed_chunks: 0,
                scope: "all".to_owned(),
                requires_repo_context: false,
                startup_rebuild: Some(CacheServiceDedupRebuildProbe {
                    status: "failed".to_owned(),
                    entries: 0,
                    error: Some("bad shard".to_owned()),
                }),
                last_ingestion_error: None,
            }),
        };

        let result = cache_service_admin_stats_result(ServiceMode::Dedup, &stats);

        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.contains("rebuild failed"));
    }

    #[test]
    fn cache_service_support_config_redacts_endpoint_and_auth_secret() {
        let mut config = Config::default();
        config.cache.service_url = Some("https://cache.internal.example:8443/team".to_owned());
        config.cache.service_auth = ServiceAuth::Psk("super-secret-psk".to_owned());

        let support =
            cache_service_support_config(&config, Some("https://cache.internal.example:8443/team"));

        assert!(support.configured);
        assert_eq!(support.service_url, "configured-redacted");
        assert_eq!(support.scheme.as_deref(), Some("https"));
        assert_eq!(support.auth, Some("psk configured"));

        let body = serde_json::to_string(&support).unwrap();
        assert!(!body.contains("cache.internal.example"));
        assert!(!body.contains("super-secret-psk"));
    }

    #[test]
    fn cache_service_support_signals_summarize_admin_stats() {
        let stats = CacheServiceAdminProbe {
            total_bytes: Some(2048),
            max_bytes: Some(4096),
            limits: Some(CacheServiceLimitsProbe {
                max_cache_bytes: Some(4096),
                max_object_bytes: Some(8192),
            }),
            xorb_count: Some(2),
            shard_count: Some(1),
            pack_count: Some(0),
            metadata_count: Some(3),
            eviction: Some(CacheServiceEvictionProbe {
                total: 3,
                xorb: 2,
                shard: 1,
                pack: 0,
                pack_index: 0,
                metadata: 0,
            }),
            startup_integrity: Some(CacheServiceStartupIntegrityProbe {
                metadata_entries_removed: 1,
                metadata_size_corrections: 2,
                unindexed_objects_indexed: 3,
                unindexed_paths_removed: 4,
            }),
            runtime_integrity: Some(CacheServiceRuntimeIntegrityProbe {
                missing_files_repaired: 5,
                invalid_objects_evicted: 6,
                metadata_entries_recreated: 7,
            }),
            traffic: Some(CacheServiceTrafficProbe {
                cache_hits: Some(8),
                cache_misses: Some(2),
                origin_avoided_reads: Some(8),
                origin_fetches: Some(2),
                bytes_served_from_cache: Some(80),
                bytes_served_total: Some(100),
                push_warming_writes: Some(4),
                mutable_proxy_reads: Some(1),
            }),
            dedup_index: None,
        };

        let signals = cache_service_support_signals(Some(&stats));

        assert_eq!(signals.cache_hit_rate, Some(0.8));
        assert_eq!(signals.origin_fallback_rate, Some(0.2));
        assert_eq!(signals.bytes_from_cache_rate, Some(0.8));
        assert_eq!(signals.integrity_repairs, Some(28));
        assert_eq!(signals.push_warming_writes, Some(4));
        assert_eq!(signals.mutable_proxy_reads, Some(1));
        assert_eq!(signals.evicted_objects, Some(3));
    }

    #[test]
    fn cache_service_metrics_summary_sums_selected_metrics_without_labels() {
        let totals = summarize_cache_service_metrics(
            r#"
# TYPE cache_hit_total counter
cache_hit_total{object_type="xorb"} 2
cache_hit_total{object_type="shard"} 3
origin_avoided_reads_total{object_type="xorb"} 2
origin_avoided_reads_total{object_type="shard"} 3
origin_fetch_total{object_type="xorb"} 1
mutable_path_proxy_read_total{method="GET"} 4
cache_eviction_total{object_type="xorb"} 6
cache_eviction_total{object_type="shard"} 7
cache_max_bytes 1024
cache_max_object_bytes 2048
unrelated_metric_total 99
"#,
        );

        assert_eq!(totals.get("cache_hit_total"), Some(&5.0));
        assert_eq!(totals.get("origin_avoided_reads_total"), Some(&5.0));
        assert_eq!(totals.get("origin_fetch_total"), Some(&1.0));
        assert_eq!(totals.get("mutable_path_proxy_read_total"), Some(&4.0));
        assert_eq!(totals.get("cache_eviction_total"), Some(&13.0));
        assert_eq!(totals.get("cache_max_bytes"), Some(&1024.0));
        assert_eq!(totals.get("cache_max_object_bytes"), Some(&2048.0));
        assert!(!totals.contains_key("unrelated_metric_total"));
    }

    #[test]
    fn cache_service_auth_probe_uses_control_plane_endpoint() {
        // The support-bundle auth probe must not depend on origin storage
        // availability; origin health is reported separately by /v1/health.
        assert_eq!(CACHE_SERVICE_AUTH_PROBE_PATH, "/v1/capabilities");
        assert_eq!(
            crab_cache::path_class::classify_path(CACHE_SERVICE_AUTH_PROBE_PATH),
            crab_cache::path_class::PathClass::Mutable
        );
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(2_147_483_648), "2.0 GB");
    }

    #[test]
    fn check_auth_defaults_to_static_ok() {
        // Default config uses Static provider — should return Ok.
        let result = check_auth();
        // In the test environment, Config::resolve_local() may or may not
        // find a config file. Either Ok (static) or Warn (config load) is valid.
        assert!(
            result.status == CheckStatus::Ok || result.status == CheckStatus::Warn,
            "expected Ok or Warn, got {:?}: {}",
            result.status,
            result.detail,
        );
    }

    #[test]
    fn parse_token_expiry_future_token() {
        // Token that expires far in the future.
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let claims = format!(r#"{{"sub":"u1","exp":{exp}}}"#);
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_some());
        assert!(!is_expired);
    }

    #[test]
    fn parse_token_expiry_past_token() {
        let exp = 1000u64; // way in the past
        let claims = format!(r#"{{"sub":"u1","exp":{exp}}}"#);
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_some());
        assert!(is_expired);
    }

    #[test]
    fn parse_token_expiry_no_exp_claim() {
        let claims = r#"{"sub":"u1"}"#;
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        let jwt = format!("{header}.{payload}.sig");

        let (expiry_str, is_expired) = parse_token_expiry(&jwt);
        assert!(expiry_str.is_none());
        assert!(!is_expired);
    }

    #[test]
    fn parse_token_expiry_malformed_jwt() {
        let (expiry_str, is_expired) = parse_token_expiry("not-a-jwt");
        assert!(expiry_str.is_none());
        assert!(is_expired);
    }

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_token_cache_path("~/.config/crab/tokens/");
        assert!(!result.as_os_str().is_empty());
        // Should not start with ~ after expansion.
        assert!(!result.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let result = expand_token_cache_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }
}
