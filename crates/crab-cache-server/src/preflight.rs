//! Server-side startup preflight for `crab-cache-server`.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path as ObjectPath;
use serde::Serialize;
use tokio::net::TcpListener;

use crate::auth::AuthPolicyDiagnostics;
use crate::config::{AuthConfig, CacheServerConfig, DedupScope, MutablePathMode};
use crate::error::CacheServiceError;
use crate::metrics::CacheMetrics;
use crate::origin_client::{ORIGIN_HEALTH_PROBE_PATH, origin_probe_reached_origin};
use crate::server::{ServerStartupOptions, build_rustls_config, prepare_server};
use crate::state::{AppState, MAX_CACHE_OBJECT_BYTES};

/// Status for a single cache-server preflight check or the aggregate report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightStatus {
    Ok,
    Warn,
    Fail,
}

/// One named cache-server startup readiness check.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightCheck {
    pub name: &'static str,
    pub status: PreflightStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<&'static str>,
}

macro_rules! preflight_issues {
    ($($variant:ident => ($code:literal, $remediation:literal $(,)?),)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum PreflightIssue {
            $($variant,)+
        }

        impl PreflightIssue {
            const ALL: &'static [Self] = &[
                $(Self::$variant,)+
            ];

            fn code(self) -> &'static str {
                match self {
                    $(Self::$variant => $code,)+
                }
            }

            fn remediation(self) -> &'static str {
                match self {
                    $(Self::$variant => $remediation,)+
                }
            }
        }
    };
}

preflight_issues! {
    ConfigInvalid => (
        "config_invalid",
        "Fix the config file and environment overrides, then rerun preflight.",
    ),
    StartupComponentsFailed => (
        "startup_components_failed",
        "Fix cache root permissions, policy file contents, origin configuration, or metadata database errors reported in detail.",
    ),
    ListenBindFailed => (
        "listen_bind_failed",
        "Stop the process using the configured address or change server.listen_addr.",
    ),
    TlsNotConfigured => (
        "tls_not_configured",
        "Enable [tls] on the cache server or terminate TLS before traffic reaches it.",
    ),
    TlsInvalid => (
        "tls_invalid",
        "Fix tls.cert_path, tls.key_path, tls.client_ca_path, and file permissions.",
    ),
    AuthPskPlainHttp => (
        "auth_psk_plain_http",
        "Enable native TLS, place the cache service behind TLS termination, or keep PSK traffic on a trusted private network.",
    ),
    AuthBearerSignatureNotEnforced => (
        "auth_bearer_signature_not_enforced",
        "Put bearer mode behind trusted authentication or use native mTLS until bearer signature validation is enforced.",
    ),
    AuthBearerTrustedOnly => (
        "auth_bearer_trusted_only",
        "Configure upstream authentication that validates tokens before requests reach the cache service.",
    ),
    AuthProxyMtlsHeaderTrust => (
        "auth_proxy_mtls_header_trust",
        "Ensure only the trusted proxy or service mesh can reach the cache service and strip client-supplied X-Client-CN.",
    ),
    PolicyNotConfigured => (
        "policy_not_configured",
        "Configure server.policy_path with least-privilege rules before enterprise rollout.",
    ),
    CacheBudgetExceeded => (
        "cache_budget_exceeded",
        "Increase cache.max_bytes, free disk space, or let startup eviction reduce the cache.",
    ),
    CacheBudgetAboveHighWater => (
        "cache_budget_above_high_water",
        "Increase cache.max_bytes or let eviction reduce the cache below the low-water mark.",
    ),
    CacheBudgetUnavailable => (
        "cache_budget_unavailable",
        "Fix cache metadata permissions or database errors before serving traffic.",
    ),
    DedupIndexRebuildIncomplete => (
        "dedup_index_rebuild_incomplete",
        "Inspect the dedup rebuild error or let startup finish rebuilding the shared index.",
    ),
    OriginUnreachable => (
        "origin_unreachable",
        "Fix origin credentials, endpoint, network path, or object-store permissions.",
    ),
    OriginProbeFailed => (
        "origin_probe_failed",
        "Fix the origin configuration or object-store permissions reported in detail.",
    ),
    OriginProbeTimeout => (
        "origin_probe_timeout",
        "Fix origin network latency/connectivity or verify the object store is reachable from the cache server.",
    ),
    EnterpriseTrustedBoundaryRequired => (
        "enterprise_trusted_boundary_required",
        "Enable native TLS/mTLS, or rerun preflight with --trusted-proxy-boundary after verifying a trusted proxy terminates TLS and controls identity headers.",
    ),
    EnterprisePolicyRequired => (
        "enterprise_policy_required",
        "Configure server.policy_path with least-privilege rules before using the enterprise profile.",
    ),
    EnterpriseAuthEnforcementRequired => (
        "enterprise_auth_enforcement_required",
        "Use PSK over a protected boundary or native/proxy mTLS; bearer placeholder modes are not accepted by the enterprise profile.",
    ),
    EnterpriseStrictMutablePathsRequired => (
        "enterprise_strict_mutable_paths_required",
        "Set server.mutable_path_mode to strict so mutable object paths are rejected instead of proxied.",
    ),
}

/// Redacted cache-server configuration summary included in preflight output.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightSummary {
    pub listen_addr: String,
    pub tls: &'static str,
    pub auth: &'static str,
    pub policy: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_diagnostics: Option<AuthPolicyDiagnostics>,
    pub origin_url: String,
    pub cache_root: String,
    pub max_cache_bytes: u64,
    pub max_object_bytes: u64,
    pub current_cache_bytes: Option<u64>,
    pub dedup_scope: String,
    pub mutable_path_mode: &'static str,
}

/// Structured output from `crab-cache-server check`.
#[derive(Debug, Clone, Serialize)]
pub struct CacheServerPreflightReport {
    pub status: PreflightStatus,
    pub summary: Option<PreflightSummary>,
    pub checks: Vec<PreflightCheck>,
}

/// Additional preflight policy to evaluate after startup checks finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightProfile {
    Standard,
    Enterprise,
}

/// Profile-specific assertions for `crab-cache-server check`.
#[derive(Debug, Clone, Copy)]
pub struct PreflightProfileOptions {
    pub profile: PreflightProfile,
    pub trusted_proxy_boundary: bool,
}

impl CacheServerPreflightReport {
    /// Builds a failed preflight report for config parse errors.
    pub fn from_config_error(error: &CacheServiceError) -> Self {
        Self::from_checks(
            None,
            vec![PreflightCheck::fail(
                "config",
                PreflightIssue::ConfigInvalid,
                error.to_string(),
            )],
        )
    }

    /// Returns true when no check failed.
    pub fn is_success(&self) -> bool {
        self.status != PreflightStatus::Fail
    }

    /// Writes human-readable preflight output.
    pub fn write_text(&self, mut out: impl Write) -> std::io::Result<()> {
        writeln!(out, "cache server preflight: {}", self.status.as_str())?;
        for check in &self.checks {
            writeln!(
                out,
                "[{}] {}: {}",
                check.status.as_str(),
                check.name,
                check.detail
            )?;
        }
        Ok(())
    }

    fn from_checks(summary: Option<PreflightSummary>, checks: Vec<PreflightCheck>) -> Self {
        let status = if checks
            .iter()
            .any(|check| check.status == PreflightStatus::Fail)
        {
            PreflightStatus::Fail
        } else if checks
            .iter()
            .any(|check| check.status == PreflightStatus::Warn)
        {
            PreflightStatus::Warn
        } else {
            PreflightStatus::Ok
        };
        Self {
            status,
            summary,
            checks,
        }
    }
}

impl PreflightStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

impl PreflightCheck {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: PreflightStatus::Ok,
            detail: detail.into(),
            code: None,
            remediation: None,
        }
    }

    fn warn(name: &'static str, issue: PreflightIssue, detail: impl Into<String>) -> Self {
        debug_assert!(PreflightIssue::ALL.contains(&issue));
        Self {
            name,
            status: PreflightStatus::Warn,
            detail: detail.into(),
            code: Some(issue.code()),
            remediation: Some(issue.remediation()),
        }
    }

    fn fail(name: &'static str, issue: PreflightIssue, detail: impl Into<String>) -> Self {
        debug_assert!(PreflightIssue::ALL.contains(&issue));
        Self {
            name,
            status: PreflightStatus::Fail,
            detail: detail.into(),
            code: Some(issue.code()),
            remediation: Some(issue.remediation()),
        }
    }
}

/// Runs cache-server startup preflight without serving traffic.
pub async fn run_preflight(config: CacheServerConfig) -> CacheServerPreflightReport {
    let origin_url = config.origin_url.clone();
    let redacted_origin_url = redact_url(&origin_url);
    let mut checks = Vec::new();

    checks.push(config_check(&config, &redacted_origin_url));
    checks.push(check_listen_bind(&config).await);
    checks.push(check_tls(&config).await);
    checks.push(auth_check(&config));

    let prepared = match prepare_server(
        config,
        ServerStartupOptions {
            metrics: CacheMetrics::stub(),
            start_evictor: false,
            run_startup_eviction: false,
        },
    ) {
        Ok(prepared) => prepared,
        Err(e) => {
            checks.push(PreflightCheck::fail(
                "startup components",
                PreflightIssue::StartupComponentsFailed,
                redact_error(&e, &origin_url, &redacted_origin_url),
            ));
            return CacheServerPreflightReport::from_checks(None, checks);
        }
    };

    let state = Arc::clone(&prepared.state);
    checks.push(PreflightCheck::ok(
        "startup components",
        "cache database, cache store, chunk index, and origin client opened",
    ));
    checks.push(policy_check(&state));
    checks.push(cache_budget_check(&state));
    checks.push(dedup_rebuild_check(&state));
    checks.push(check_origin(&state, &redacted_origin_url).await);

    let summary = preflight_summary(&state, redacted_origin_url);
    prepared.shutdown().await;
    CacheServerPreflightReport::from_checks(Some(summary), checks)
}

/// Applies profile-specific deployment policy checks to a preflight report.
pub fn apply_preflight_profile(
    report: CacheServerPreflightReport,
    options: PreflightProfileOptions,
) -> CacheServerPreflightReport {
    if options.profile == PreflightProfile::Standard {
        return report;
    }

    let CacheServerPreflightReport {
        summary,
        mut checks,
        ..
    } = report;
    checks.extend(enterprise_profile_checks(
        summary.as_ref(),
        &checks,
        options.trusted_proxy_boundary,
    ));
    CacheServerPreflightReport::from_checks(summary, checks)
}

fn enterprise_profile_checks(
    summary: Option<&PreflightSummary>,
    checks: &[PreflightCheck],
    trusted_proxy_boundary: bool,
) -> Vec<PreflightCheck> {
    let mut profile_checks = Vec::new();
    let has_code = |code: &'static str| checks.iter().any(|check| check.code == Some(code));

    if !trusted_proxy_boundary
        && (has_code(PreflightIssue::TlsNotConfigured.code())
            || has_code(PreflightIssue::AuthPskPlainHttp.code())
            || has_code(PreflightIssue::AuthProxyMtlsHeaderTrust.code()))
    {
        profile_checks.push(PreflightCheck::fail(
            "enterprise trusted boundary",
            PreflightIssue::EnterpriseTrustedBoundaryRequired,
            "enterprise profile requires native TLS/mTLS or an explicit trusted proxy boundary",
        ));
    }

    if has_code(PreflightIssue::PolicyNotConfigured.code()) {
        profile_checks.push(PreflightCheck::fail(
            "enterprise authorization policy",
            PreflightIssue::EnterprisePolicyRequired,
            "enterprise profile requires server.policy_path",
        ));
    }

    if has_code(PreflightIssue::AuthBearerSignatureNotEnforced.code())
        || has_code(PreflightIssue::AuthBearerTrustedOnly.code())
    {
        profile_checks.push(PreflightCheck::fail(
            "enterprise auth enforcement",
            PreflightIssue::EnterpriseAuthEnforcementRequired,
            "enterprise profile requires enforced auth; bearer placeholder modes are not accepted",
        ));
    }

    if summary.is_some_and(|summary| summary.mutable_path_mode != "strict") {
        profile_checks.push(PreflightCheck::fail(
            "enterprise mutable path mode",
            PreflightIssue::EnterpriseStrictMutablePathsRequired,
            "enterprise profile requires server.mutable_path_mode = \"strict\"",
        ));
    }

    if profile_checks.is_empty() {
        profile_checks.push(PreflightCheck::ok(
            "enterprise profile",
            "enterprise profile requirements satisfied",
        ));
    }

    profile_checks
}

/// Returns the configured origin URL with userinfo redacted.
pub fn redacted_origin_url(config: &CacheServerConfig) -> String {
    redact_url(&config.origin_url)
}

fn config_check(config: &CacheServerConfig, redacted_origin_url: &str) -> PreflightCheck {
    PreflightCheck::ok(
        "config",
        format!(
            "listen {}, origin {}, cache root {}, mode {}, dedup {}",
            config.listen_addr,
            redacted_origin_url,
            config.cache_root.display(),
            mutable_path_mode_label(config.mutable_path_mode),
            dedup_scope_label(&config.dedup_scope)
        ),
    )
}

async fn check_listen_bind(config: &CacheServerConfig) -> PreflightCheck {
    match TcpListener::bind(config.listen_addr).await {
        Ok(listener) => {
            let detail = match listener.local_addr() {
                Ok(addr) => format!("{} bind ok", addr),
                Err(_) => format!("{} bind ok", config.listen_addr),
            };
            PreflightCheck::ok("listen bind", detail)
        }
        Err(e) => PreflightCheck::fail(
            "listen bind",
            PreflightIssue::ListenBindFailed,
            format!("{} cannot bind: {e}", config.listen_addr),
        ),
    }
}

async fn check_tls(config: &CacheServerConfig) -> PreflightCheck {
    let Some(tls) = &config.tls else {
        return PreflightCheck::warn(
            "tls",
            PreflightIssue::TlsNotConfigured,
            "no TLS configured; terminate TLS before the cache service or keep traffic on a trusted network",
        );
    };

    match build_rustls_config(tls) {
        Ok(_) if tls.client_ca_path.is_some() => PreflightCheck::ok(
            "tls",
            "server certificate, key, and client CA loaded; native mTLS client certs required",
        ),
        Ok(_) => PreflightCheck::ok("tls", "server certificate and key loaded"),
        Err(e) => PreflightCheck::fail("tls", PreflightIssue::TlsInvalid, e.to_string()),
    }
}

fn auth_check(config: &CacheServerConfig) -> PreflightCheck {
    match &config.auth {
        AuthConfig::Psk { .. } if config.tls.is_none() => PreflightCheck::warn(
            "auth",
            PreflightIssue::AuthPskPlainHttp,
            "psk configured over plain HTTP; enable native TLS or terminate TLS before clients",
        ),
        AuthConfig::Psk { .. } => PreflightCheck::ok(
            "auth",
            "psk configured with TLS; secret value is not stored",
        ),
        AuthConfig::Bearer { jwks_url: Some(_) } => PreflightCheck::warn(
            "auth",
            PreflightIssue::AuthBearerSignatureNotEnforced,
            "bearer configured with jwks_url, but signature validation is not enforced yet",
        ),
        AuthConfig::Bearer { jwks_url: None } => PreflightCheck::warn(
            "auth",
            PreflightIssue::AuthBearerTrustedOnly,
            "bearer tokens are accepted as principals; use only behind trusted authentication",
        ),
        AuthConfig::Mtls
            if config
                .tls
                .as_ref()
                .is_some_and(|tls| tls.client_ca_path.is_some()) =>
        {
            PreflightCheck::ok(
                "auth",
                "native mtls configured; principal is mtls-sha256:<leaf-cert-fingerprint>",
            )
        }
        AuthConfig::Mtls => PreflightCheck::warn(
            "auth",
            PreflightIssue::AuthProxyMtlsHeaderTrust,
            "mtls mode trusts x-client-cn from a trusted proxy or service mesh",
        ),
    }
}

fn policy_check(state: &AppState) -> PreflightCheck {
    if let Some(policy) = &state.policy {
        let diagnostics = policy.diagnostics();
        PreflightCheck::ok(
            "authorization policy",
            format!(
                "policy loaded; rules {}, repo patterns {}, actions {}",
                diagnostics.rule_count,
                diagnostics.repo_pattern_count,
                diagnostics.actions.join(",")
            ),
        )
    } else {
        PreflightCheck::warn(
            "authorization policy",
            PreflightIssue::PolicyNotConfigured,
            "no policy_path configured; authenticated principals can read, write, dedup, and admin",
        )
    }
}

fn cache_budget_check(state: &AppState) -> PreflightCheck {
    match state.cache_store.stats() {
        Ok(stats) => {
            let current = stats.total_bytes;
            let high_water =
                (state.cache_store.max_bytes() as f64 * state.config.high_water_ratio) as u64;
            let detail = format!(
                "{} / {} stored, xorbs {}, shards {}, packs {}, metadata {}",
                format_bytes(current),
                format_bytes(stats.max_bytes),
                stats.xorb_count,
                stats.shard_count,
                stats.pack_count,
                stats.metadata_count
            );
            if current > state.cache_store.max_bytes() {
                return PreflightCheck::warn(
                    "cache budget",
                    PreflightIssue::CacheBudgetExceeded,
                    format!("{detail}; startup eviction will run"),
                );
            }
            if current > high_water {
                return PreflightCheck::warn(
                    "cache budget",
                    PreflightIssue::CacheBudgetAboveHighWater,
                    format!("{detail}; above high-water mark"),
                );
            }
            PreflightCheck::ok("cache budget", detail)
        }
        Err(e) => PreflightCheck::fail(
            "cache budget",
            PreflightIssue::CacheBudgetUnavailable,
            e.to_string(),
        ),
    }
}

fn dedup_rebuild_check(state: &AppState) -> PreflightCheck {
    let rebuild = &state.dedup_index_rebuild;
    let detail = format!(
        "status {}, entries {}{}",
        rebuild.status,
        rebuild.entries,
        rebuild
            .error
            .as_ref()
            .map(|e| format!(", error {e}"))
            .unwrap_or_default()
    );
    if rebuild.status == "ok" {
        PreflightCheck::ok("dedup index rebuild", detail)
    } else {
        PreflightCheck::warn(
            "dedup index rebuild",
            PreflightIssue::DedupIndexRebuildIncomplete,
            detail,
        )
    }
}

async fn check_origin(state: &AppState, redacted_origin_url: &str) -> PreflightCheck {
    let probe = tokio::time::timeout(
        Duration::from_secs(3),
        state
            .origin
            .head(&ObjectPath::from(ORIGIN_HEALTH_PROBE_PATH)),
    )
    .await;

    match probe {
        Ok(result) => {
            if origin_probe_reached_origin(&result) {
                return PreflightCheck::ok("origin", format!("{redacted_origin_url} reachable"));
            }
            match result {
                Err(CacheServiceError::OriginUnreachable { reason }) => PreflightCheck::fail(
                    "origin",
                    PreflightIssue::OriginUnreachable,
                    format!("{redacted_origin_url} unreachable: {reason}"),
                ),
                Err(e) => PreflightCheck::fail(
                    "origin",
                    PreflightIssue::OriginProbeFailed,
                    format!("{redacted_origin_url} probe failed: {e}"),
                ),
                Ok(_) => PreflightCheck::ok("origin", format!("{redacted_origin_url} reachable")),
            }
        }
        Err(_) => PreflightCheck::fail(
            "origin",
            PreflightIssue::OriginProbeTimeout,
            format!("{redacted_origin_url} probe timed out after 3s"),
        ),
    }
}

fn preflight_summary(state: &AppState, origin_url: String) -> PreflightSummary {
    PreflightSummary {
        listen_addr: state.config.listen_addr.to_string(),
        tls: if state.config.tls.is_some() {
            "configured"
        } else {
            "plain_http"
        },
        auth: auth_label(&state.config.auth),
        policy: if state.policy.is_some() {
            "configured"
        } else {
            "not_configured"
        },
        policy_diagnostics: state.policy.as_ref().map(|policy| policy.diagnostics()),
        origin_url,
        cache_root: state.config.cache_root.display().to_string(),
        max_cache_bytes: state.config.max_cache_bytes,
        max_object_bytes: MAX_CACHE_OBJECT_BYTES as u64,
        current_cache_bytes: Some(state.cache_store.current_bytes()),
        dedup_scope: dedup_scope_label(&state.config.dedup_scope),
        mutable_path_mode: mutable_path_mode_label(state.config.mutable_path_mode),
    }
}

fn auth_label(auth: &AuthConfig) -> &'static str {
    match auth {
        AuthConfig::Mtls => "mtls",
        AuthConfig::Bearer { .. } => "bearer",
        AuthConfig::Psk { .. } => "psk",
    }
}

fn dedup_scope_label(scope: &DedupScope) -> String {
    match scope {
        DedupScope::All => "all".to_string(),
        DedupScope::BucketPrefix(prefix) => format!("bucket-prefix:{prefix}"),
        DedupScope::Repos(repos) => format!("repos:{}", repos.join(",")),
    }
}

fn mutable_path_mode_label(mode: MutablePathMode) -> &'static str {
    match mode {
        MutablePathMode::Strict => "strict",
        MutablePathMode::Transparent => "transparent",
    }
}

fn redact_url(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return "<invalid-url>".to_string();
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return raw.to_string();
    }

    let host = parsed.host_str().unwrap_or("<host>");
    let port = parsed
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    format!(
        "{}://<redacted>@{}{}{}",
        parsed.scheme(),
        host,
        port,
        parsed.path()
    )
}

fn redact_error(
    error: &CacheServiceError,
    raw_origin_url: &str,
    redacted_origin_url: &str,
) -> String {
    error
        .to_string()
        .replace(raw_origin_url, redacted_origin_url)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::*;
    use crate::config::{AuthConfig, DedupScope, MutablePathMode, TlsConfig};

    fn test_config(cache_root: std::path::PathBuf, origin_url: String) -> CacheServerConfig {
        CacheServerConfig {
            listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            tls: None,
            auth: AuthConfig::Psk {
                key_hash: *blake3::hash(b"secret").as_bytes(),
            },
            origin_url,
            cache_root,
            max_cache_bytes: 1024 * 1024,
            dedup_scope: DedupScope::All,
            drain_timeout: Duration::from_secs(1),
            mutable_path_mode: MutablePathMode::Strict,
            high_water_ratio: 0.95,
            low_water_ratio: 0.90,
            policy_path: None,
        }
    }

    #[tokio::test]
    async fn preflight_opens_local_origin_and_cache_stack() {
        let tempdir = tempfile::tempdir().unwrap();
        let origin_dir = tempdir.path().join("origin");
        let cache_root = tempdir.path().join("cache");
        std::fs::create_dir_all(&origin_dir).unwrap();
        let origin_url = url::Url::from_directory_path(&origin_dir)
            .unwrap()
            .to_string();

        let report = run_preflight(test_config(cache_root, origin_url)).await;

        assert_ne!(report.status, PreflightStatus::Fail);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "startup components"
                    && check.status == PreflightStatus::Ok)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "origin" && check.status == PreflightStatus::Ok)
        );
        assert_eq!(
            report.summary.unwrap().max_object_bytes,
            MAX_CACHE_OBJECT_BYTES as u64
        );
    }

    #[tokio::test]
    async fn preflight_fails_invalid_authorization_policy() {
        let tempdir = tempfile::tempdir().unwrap();
        let origin_dir = tempdir.path().join("origin");
        let cache_root = tempdir.path().join("cache");
        let policy_path = tempdir.path().join("policy.yaml");
        std::fs::create_dir_all(&origin_dir).unwrap();
        std::fs::write(
            &policy_path,
            r#"
rules:
  - principal: "psk-client"
    repos: [".crab"]
    actions: ["read", "admn"]
"#,
        )
        .unwrap();
        let origin_url = url::Url::from_directory_path(&origin_dir)
            .unwrap()
            .to_string();
        let mut config = test_config(cache_root, origin_url);
        config.policy_path = Some(policy_path);

        let report = run_preflight(config).await;

        assert_eq!(report.status, PreflightStatus::Fail);
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "startup components")
            .unwrap();
        assert_eq!(check.status, PreflightStatus::Fail);
        assert!(check.detail.contains("unknown action"));
    }

    #[tokio::test]
    async fn preflight_reports_policy_diagnostics_without_principals() {
        let tempdir = tempfile::tempdir().unwrap();
        let origin_dir = tempdir.path().join("origin");
        let cache_root = tempdir.path().join("cache");
        let policy_path = tempdir.path().join("policy.yaml");
        std::fs::create_dir_all(&origin_dir).unwrap();
        std::fs::write(
            &policy_path,
            r#"
rules:
  - principal: "psk-client"
    repos: [".crab", "org/allowed/*"]
    actions: ["read", "dedup"]
"#,
        )
        .unwrap();
        let origin_url = url::Url::from_directory_path(&origin_dir)
            .unwrap()
            .to_string();
        let mut config = test_config(cache_root, origin_url);
        config.policy_path = Some(policy_path);

        let report = run_preflight(config).await;

        assert_ne!(report.status, PreflightStatus::Fail);
        let diagnostics = report
            .summary
            .as_ref()
            .unwrap()
            .policy_diagnostics
            .as_ref()
            .unwrap();
        assert_eq!(diagnostics.rule_count, 1);
        assert_eq!(diagnostics.repo_pattern_count, 2);
        assert_eq!(
            diagnostics.actions,
            vec!["read".to_string(), "dedup".to_string()]
        );
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "authorization policy")
            .unwrap();
        assert_eq!(check.status, PreflightStatus::Ok);
        assert!(check.detail.contains("rules 1"));
        assert!(check.detail.contains("repo patterns 2"));
        assert!(!check.detail.contains("psk-client"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("policy_diagnostics"));
        assert!(!json.contains("psk-client"));
    }

    #[test]
    fn redacts_origin_url_userinfo() {
        assert_eq!(
            redact_url("https://user:secret@example.com/path/to/bucket"),
            "https://<redacted>@example.com/path/to/bucket"
        );
    }

    #[test]
    fn report_status_fails_when_any_check_fails() {
        let report = CacheServerPreflightReport::from_checks(
            None,
            vec![
                PreflightCheck::ok("a", "ok"),
                PreflightCheck::fail("b", PreflightIssue::ConfigInvalid, "failed"),
            ],
        );

        assert_eq!(report.status, PreflightStatus::Fail);
        assert!(!report.is_success());
    }

    #[test]
    fn non_ok_checks_serialize_issue_code_and_remediation() {
        let report = CacheServerPreflightReport::from_checks(
            None,
            vec![
                PreflightCheck::ok("a", "ok"),
                PreflightCheck::warn("b", PreflightIssue::TlsNotConfigured, "warned"),
            ],
        );

        let json = serde_json::to_value(&report).unwrap();
        assert!(json["checks"][0].get("code").is_none());
        assert_eq!(json["checks"][1]["code"], "tls_not_configured");
        assert!(
            json["checks"][1]["remediation"]
                .as_str()
                .unwrap()
                .contains("Enable [tls]")
        );
    }

    fn preflight_summary_for_mode(mutable_path_mode: &'static str) -> PreflightSummary {
        PreflightSummary {
            listen_addr: "127.0.0.1:0".to_string(),
            tls: "plain_http",
            auth: "psk",
            policy: "configured",
            policy_diagnostics: None,
            origin_url: "file:///origin".to_string(),
            cache_root: "/cache".to_string(),
            max_cache_bytes: 1024,
            max_object_bytes: MAX_CACHE_OBJECT_BYTES as u64,
            current_cache_bytes: Some(0),
            dedup_scope: "all".to_string(),
            mutable_path_mode,
        }
    }

    fn report_for_enterprise_checks(checks: Vec<PreflightCheck>) -> CacheServerPreflightReport {
        CacheServerPreflightReport::from_checks(Some(preflight_summary_for_mode("strict")), checks)
    }

    fn codes(report: &CacheServerPreflightReport) -> BTreeSet<&'static str> {
        report
            .checks
            .iter()
            .filter_map(|check| check.code)
            .collect()
    }

    #[test]
    fn enterprise_profile_rejects_untrusted_boundary_and_missing_policy() {
        let report = report_for_enterprise_checks(vec![
            PreflightCheck::warn("tls", PreflightIssue::TlsNotConfigured, "plain"),
            PreflightCheck::warn("auth", PreflightIssue::AuthPskPlainHttp, "plain psk"),
            PreflightCheck::warn(
                "authorization policy",
                PreflightIssue::PolicyNotConfigured,
                "open",
            ),
        ]);

        let report = apply_preflight_profile(
            report,
            PreflightProfileOptions {
                profile: PreflightProfile::Enterprise,
                trusted_proxy_boundary: false,
            },
        );

        let codes = codes(&report);
        assert_eq!(report.status, PreflightStatus::Fail);
        assert!(codes.contains(PreflightIssue::EnterpriseTrustedBoundaryRequired.code()));
        assert!(codes.contains(PreflightIssue::EnterprisePolicyRequired.code()));
    }

    #[test]
    fn enterprise_profile_accepts_explicit_proxy_boundary() {
        let report = report_for_enterprise_checks(vec![
            PreflightCheck::warn("tls", PreflightIssue::TlsNotConfigured, "plain"),
            PreflightCheck::warn("auth", PreflightIssue::AuthPskPlainHttp, "plain psk"),
        ]);

        let report = apply_preflight_profile(
            report,
            PreflightProfileOptions {
                profile: PreflightProfile::Enterprise,
                trusted_proxy_boundary: true,
            },
        );

        assert_eq!(report.status, PreflightStatus::Warn);
        assert!(report.checks.iter().any(|check| {
            check.name == "enterprise profile" && check.status == PreflightStatus::Ok
        }));
        assert!(!codes(&report).contains(PreflightIssue::EnterpriseTrustedBoundaryRequired.code()));
    }

    #[test]
    fn enterprise_profile_rejects_bearer_and_transparent_mutable_paths() {
        let report = CacheServerPreflightReport::from_checks(
            Some(preflight_summary_for_mode("transparent")),
            vec![PreflightCheck::warn(
                "auth",
                PreflightIssue::AuthBearerTrustedOnly,
                "bearer",
            )],
        );

        let report = apply_preflight_profile(
            report,
            PreflightProfileOptions {
                profile: PreflightProfile::Enterprise,
                trusted_proxy_boundary: true,
            },
        );

        let codes = codes(&report);
        assert_eq!(report.status, PreflightStatus::Fail);
        assert!(codes.contains(PreflightIssue::EnterpriseAuthEnforcementRequired.code()));
        assert!(codes.contains(PreflightIssue::EnterpriseStrictMutablePathsRequired.code()));
    }

    #[test]
    fn enterprise_profile_json_contract_rejects_weak_startup_posture() {
        let mut summary = preflight_summary_for_mode("transparent");
        summary.policy = "not_configured";
        let report = CacheServerPreflightReport::from_checks(
            Some(summary),
            vec![
                PreflightCheck::warn("tls", PreflightIssue::TlsNotConfigured, "plain"),
                PreflightCheck::warn("auth", PreflightIssue::AuthPskPlainHttp, "plain psk"),
                PreflightCheck::warn(
                    "authorization policy",
                    PreflightIssue::PolicyNotConfigured,
                    "open",
                ),
            ],
        );

        let report = apply_preflight_profile(
            report,
            PreflightProfileOptions {
                profile: PreflightProfile::Enterprise,
                trusted_proxy_boundary: false,
            },
        );
        let json = serde_json::to_value(&report).unwrap();
        let checks: Vec<_> = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|check| {
                Some(serde_json::json!({
                    "name": check.get("name")?.as_str()?,
                    "status": check.get("status")?.as_str()?,
                    "code": check.get("code")?.as_str()?,
                    "has_remediation": check.get("remediation")?.as_str()?.is_empty() == false,
                }))
            })
            .collect();

        assert_eq!(json["status"], "fail");
        assert_eq!(json["summary"]["tls"], "plain_http");
        assert_eq!(json["summary"]["auth"], "psk");
        assert_eq!(json["summary"]["policy"], "not_configured");
        assert_eq!(json["summary"]["mutable_path_mode"], "transparent");
        assert_eq!(
            checks,
            vec![
                serde_json::json!({
                    "name": "tls",
                    "status": "warn",
                    "code": "tls_not_configured",
                    "has_remediation": true,
                }),
                serde_json::json!({
                    "name": "auth",
                    "status": "warn",
                    "code": "auth_psk_plain_http",
                    "has_remediation": true,
                }),
                serde_json::json!({
                    "name": "authorization policy",
                    "status": "warn",
                    "code": "policy_not_configured",
                    "has_remediation": true,
                }),
                serde_json::json!({
                    "name": "enterprise trusted boundary",
                    "status": "fail",
                    "code": "enterprise_trusted_boundary_required",
                    "has_remediation": true,
                }),
                serde_json::json!({
                    "name": "enterprise authorization policy",
                    "status": "fail",
                    "code": "enterprise_policy_required",
                    "has_remediation": true,
                }),
                serde_json::json!({
                    "name": "enterprise mutable path mode",
                    "status": "fail",
                    "code": "enterprise_strict_mutable_paths_required",
                    "has_remediation": true,
                }),
            ]
        );
    }

    #[test]
    fn issue_registry_codes_are_unique_and_documented() {
        let docs = include_str!(
            "../../../crab-web/content/docs/cli/cache-service/server-configuration.mdx"
        );
        let mut codes = BTreeSet::new();

        for issue in PreflightIssue::ALL {
            let code = issue.code();
            assert!(codes.insert(code), "duplicate preflight issue code {code}");
            assert!(
                !issue.remediation().is_empty(),
                "missing remediation for preflight issue code {code}"
            );
            assert!(
                docs.contains(&format!("`{code}`")),
                "server configuration docs missing preflight issue code {code}"
            );
        }
    }

    #[tokio::test]
    async fn tls_check_warns_when_tls_is_not_configured() {
        let config = test_config(std::path::PathBuf::from("/cache"), "file:///origin".into());

        let check = check_tls(&config).await;

        assert_eq!(check.status, PreflightStatus::Warn);
        assert_eq!(check.code, Some("tls_not_configured"));
        assert!(check.detail.contains("no TLS configured"));
    }

    #[test]
    fn auth_check_warns_for_psk_without_tls() {
        let config = test_config(std::path::PathBuf::from("/cache"), "file:///origin".into());

        let check = auth_check(&config);

        assert_eq!(check.status, PreflightStatus::Warn);
        assert_eq!(check.code, Some("auth_psk_plain_http"));
        assert!(check.detail.contains("plain HTTP"));
    }

    #[test]
    fn auth_check_reports_psk_ok_when_tls_is_configured() {
        let mut config = test_config(std::path::PathBuf::from("/cache"), "file:///origin".into());
        config.tls = Some(TlsConfig {
            cert_path: "/tls/server.pem".into(),
            key_path: "/tls/server-key.pem".into(),
            client_ca_path: None,
        });

        let check = auth_check(&config);

        assert_eq!(check.status, PreflightStatus::Ok);
        assert_eq!(check.code, None);
        assert!(check.detail.contains("with TLS"));
    }

    #[test]
    fn auth_check_reports_native_mtls_when_client_ca_is_configured() {
        let mut config = test_config(std::path::PathBuf::from("/cache"), "file:///origin".into());
        config.auth = AuthConfig::Mtls;
        config.tls = Some(TlsConfig {
            cert_path: "/tls/server.pem".into(),
            key_path: "/tls/server-key.pem".into(),
            client_ca_path: Some("/tls/client-ca.pem".into()),
        });

        let check = auth_check(&config);

        assert_eq!(check.status, PreflightStatus::Ok);
        assert!(check.detail.contains("mtls-sha256"));
    }

    #[test]
    fn auth_check_warns_for_proxy_mtls() {
        let mut config = test_config(std::path::PathBuf::from("/cache"), "file:///origin".into());
        config.auth = AuthConfig::Mtls;

        let check = auth_check(&config);

        assert_eq!(check.status, PreflightStatus::Warn);
        assert_eq!(check.code, Some("auth_proxy_mtls_header_trust"));
        assert!(check.detail.contains("x-client-cn"));
    }
}
