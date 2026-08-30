//! Enterprise onboarding bundle rendering.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crab_cache::active_probe::{self, ActiveProbeAuth};
use crab_cache::path_class::{CacheRouteContract, cache_route_contract_matches_current};

use crate::auth::{AuthPolicy, AuthPolicyDiagnostics};
use crate::config::{AuthConfig, CacheServerConfig, DedupScope, MutablePathMode};
use crate::preflight::{
    CacheServerPreflightReport, PreflightProfile, PreflightProfileOptions, PreflightStatus,
    apply_preflight_profile, run_preflight,
};

const DEFAULT_POLICY_PATH: &str = "/etc/crab-cache-server/policy.yaml";
const DEFAULT_CACHE_ROOT: &str = "/data/crab-cache";
const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8443";
const DEFAULT_MAX_CACHE_BYTES: u64 = 1_099_511_627_776;
const CLIENT_PSK_PLACEHOLDER: &str = "<secret-from-secret-manager>";
const CAPABILITIES_SCHEMA: &str = "crab-cache-service.capabilities.v1";
const AUTHZ_SCHEMA: &str = "crab-cache-service.authz-check.v1";

#[derive(Debug)]
pub struct OnboardingRenderOptions {
    pub output_dir: PathBuf,
    pub origin_url: String,
    pub cache_service_url: String,
    pub repo_prefixes: Vec<String>,
    pub psk_hash: String,
    pub cache_root: String,
    pub max_cache_bytes: u64,
    pub listen_addr: String,
    pub policy_path: String,
    pub force: bool,
}

#[derive(Debug)]
pub struct OnboardingBundle {
    pub output_dir: PathBuf,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct OnboardingCheckReport {
    pub status: PreflightStatus,
    pub bundle_dir: String,
    pub checks: Vec<OnboardingCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_diagnostics: Option<AuthPolicyDiagnostics>,
}

#[derive(Debug, Serialize)]
pub struct OnboardingProbeReport {
    pub status: PreflightStatus,
    pub bundle_check: OnboardingCheckReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_preflight: Option<CacheServerPreflightReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_probe: Option<OnboardingClientProbeReport>,
}

#[derive(Debug)]
pub struct OnboardingProbeOptions {
    pub trusted_proxy_boundary: bool,
    pub client_probe_repo: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OnboardingClientProbeReport {
    pub status: PreflightStatus,
    pub repo_path: String,
    pub service_url: String,
    pub checks: Vec<OnboardingCheck>,
}

#[derive(Debug)]
struct OnboardingClientConfig {
    service_url: String,
    service_mode: String,
    service_auth: String,
    push_warming: bool,
}

#[derive(Debug, Deserialize)]
struct CapabilitiesResponse {
    schema: Option<String>,
    limits: Option<CapabilitiesLimits>,
    routes: Option<CacheRouteContract>,
}

#[derive(Debug, Deserialize)]
struct CapabilitiesLimits {
    max_cache_bytes: Option<u64>,
    max_object_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AuthzResponse {
    schema: Option<String>,
    actions: Option<AuthzActions>,
}

#[derive(Debug, Deserialize)]
struct AuthzActions {
    read: Option<bool>,
    write: Option<bool>,
    dedup: Option<bool>,
    admin: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct OnboardingCheck {
    pub name: &'static str,
    pub status: PreflightStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<&'static str>,
}

impl OnboardingRenderOptions {
    pub fn with_defaults(
        output_dir: PathBuf,
        origin_url: String,
        cache_service_url: String,
        repo_prefixes: Vec<String>,
        psk_hash: String,
        force: bool,
    ) -> Self {
        Self {
            output_dir,
            origin_url,
            cache_service_url,
            repo_prefixes,
            psk_hash,
            cache_root: DEFAULT_CACHE_ROOT.to_string(),
            max_cache_bytes: DEFAULT_MAX_CACHE_BYTES,
            listen_addr: DEFAULT_LISTEN_ADDR.to_string(),
            policy_path: DEFAULT_POLICY_PATH.to_string(),
            force,
        }
    }
}

pub fn render_onboarding_bundle(
    options: &OnboardingRenderOptions,
) -> Result<OnboardingBundle, String> {
    validate_options(options)?;
    fs::create_dir_all(&options.output_dir).map_err(|error| {
        format!(
            "failed to create onboarding output directory {}: {error}",
            options.output_dir.display()
        )
    })?;

    let files = [
        ("server-config.toml", server_config(options)),
        ("policy.yaml", policy_yaml(options)),
        ("client-config.toml", client_config(options)),
        ("client.env", client_env(options)),
        ("README.md", readme(options)),
    ];

    let mut written = Vec::with_capacity(files.len());
    for (name, contents) in files {
        let path = options.output_dir.join(name);
        write_bundle_file(&path, &contents, options.force)?;
        written.push(path);
    }

    Ok(OnboardingBundle {
        output_dir: options.output_dir.clone(),
        files: written,
    })
}

pub fn check_onboarding_bundle(bundle_dir: &Path) -> OnboardingCheckReport {
    let bundle_dir = fs::canonicalize(bundle_dir).unwrap_or_else(|_| bundle_dir.to_path_buf());
    let mut checks = Vec::new();

    let expected_files = [
        "server-config.toml",
        "policy.yaml",
        "client-config.toml",
        "client.env",
        "README.md",
    ];
    for name in expected_files {
        let path = bundle_dir.join(name);
        if path.is_file() {
            checks.push(OnboardingCheck::ok(
                "bundle file",
                format!("{name} present"),
            ));
        } else {
            checks.push(OnboardingCheck::fail(
                "bundle file",
                "onboarding_file_missing",
                format!("{name} is missing"),
                "Regenerate the bundle or restore the missing file before rollout.",
            ));
        }
    }

    let config = match CacheServerConfig::from_file(&bundle_dir.join("server-config.toml")) {
        Ok(config) => {
            checks.push(OnboardingCheck::ok(
                "server config",
                "server-config.toml parses",
            ));
            Some(config)
        }
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "server config",
                "onboarding_server_config_invalid",
                error.to_string(),
                "Fix server-config.toml or rerender the onboarding bundle.",
            ));
            None
        }
    };

    if let Some(config) = &config {
        check_enterprise_server_config(config, &mut checks);
    }

    let policy = match AuthPolicy::from_file(&bundle_dir.join("policy.yaml")) {
        Ok(policy) => {
            checks.push(OnboardingCheck::ok(
                "authorization policy",
                "policy.yaml parses",
            ));
            Some(policy)
        }
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "authorization policy",
                "onboarding_policy_invalid",
                error.to_string(),
                "Fix policy.yaml so enterprise preflight can load least-privilege rules.",
            ));
            None
        }
    };
    if let Some(policy) = &policy {
        check_policy(policy, &mut checks);
    }

    check_client_config(&bundle_dir.join("client-config.toml"), &mut checks);
    check_client_env(&bundle_dir.join("client.env"), &mut checks);
    check_secret_redaction(&bundle_dir, config.as_ref(), &mut checks);

    let policy_diagnostics = policy.as_ref().map(AuthPolicy::diagnostics);
    OnboardingCheckReport {
        status: aggregate_status(&checks),
        bundle_dir: bundle_dir.display().to_string(),
        checks,
        policy_diagnostics,
    }
}

pub async fn probe_onboarding_bundle(
    bundle_dir: &Path,
    options: &OnboardingProbeOptions,
) -> OnboardingProbeReport {
    let bundle_check = check_onboarding_bundle(bundle_dir);
    let config_path = Path::new(&bundle_check.bundle_dir).join("server-config.toml");
    let run_server_preflight = options.client_probe_repo.is_none();
    let server_preflight = if run_server_preflight {
        Some(match CacheServerConfig::from_file(&config_path) {
            Ok(config) => apply_preflight_profile(
                run_preflight(config).await,
                PreflightProfileOptions {
                    profile: PreflightProfile::Enterprise,
                    trusted_proxy_boundary: options.trusted_proxy_boundary,
                },
            ),
            Err(error) => CacheServerPreflightReport::from_config_error(&error),
        })
    } else {
        None
    };
    let client_probe = match options.client_probe_repo.as_deref() {
        Some(repo_path) => {
            Some(probe_onboarding_client(Path::new(&bundle_check.bundle_dir), repo_path).await)
        }
        None => None,
    };
    OnboardingProbeReport {
        status: aggregate_probe_status(
            bundle_check.status,
            server_preflight.as_ref().map(|preflight| preflight.status),
            client_probe.as_ref().map(|probe| probe.status),
        ),
        bundle_check,
        server_preflight,
        client_probe,
    }
}

fn validate_options(options: &OnboardingRenderOptions) -> Result<(), String> {
    validate_non_empty("origin-url", &options.origin_url)?;
    validate_non_empty("cache-service-url", &options.cache_service_url)?;
    validate_non_empty("cache-root", &options.cache_root)?;
    validate_non_empty("listen-addr", &options.listen_addr)?;
    validate_non_empty("policy-path", &options.policy_path)?;
    if options.max_cache_bytes == 0 {
        return Err("max-cache-bytes must be greater than zero".to_string());
    }
    if options.repo_prefixes.is_empty() {
        return Err("at least one --repo-prefix is required".to_string());
    }
    for prefix in &options.repo_prefixes {
        validate_repo_prefix(prefix)?;
    }
    validate_psk_hash(&options.psk_hash)
}

fn check_enterprise_server_config(config: &CacheServerConfig, checks: &mut Vec<OnboardingCheck>) {
    if config.mutable_path_mode == MutablePathMode::Strict {
        checks.push(OnboardingCheck::ok(
            "enterprise mutable path mode",
            "mutable_path_mode is strict",
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "enterprise mutable path mode",
            "onboarding_mutable_paths_not_strict",
            "mutable_path_mode must be strict for enterprise onboarding",
            "Set server.mutable_path_mode = \"strict\".",
        ));
    }

    match config.auth {
        AuthConfig::Psk { .. } | AuthConfig::Mtls => checks.push(OnboardingCheck::ok(
            "enterprise auth",
            "auth mechanism is accepted for enterprise onboarding",
        )),
        AuthConfig::Bearer { .. } => checks.push(OnboardingCheck::fail(
            "enterprise auth",
            "onboarding_auth_not_enforced",
            "bearer mode is not accepted by the enterprise onboarding profile",
            "Use PSK behind a trusted boundary or native mTLS.",
        )),
    }

    if config.policy_path.is_some() {
        checks.push(OnboardingCheck::ok(
            "enterprise policy path",
            "server.policy_path is configured",
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "enterprise policy path",
            "onboarding_policy_path_missing",
            "server.policy_path is required for enterprise onboarding",
            "Set server.policy_path to the installed policy.yaml path.",
        ));
    }

    if config.max_cache_bytes > 0 {
        checks.push(OnboardingCheck::ok(
            "cache budget",
            format!("cache.max_bytes is {}", config.max_cache_bytes),
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "cache budget",
            "onboarding_cache_budget_invalid",
            "cache.max_bytes must be greater than zero",
            "Set cache.max_bytes to the byte budget for the cache volume.",
        ));
    }

    match &config.dedup_scope {
        DedupScope::All | DedupScope::BucketPrefix(_) | DedupScope::Repos(_) => {
            checks.push(OnboardingCheck::ok("dedup scope", "dedup scope parses"));
        }
    }
}

fn check_policy(policy: &AuthPolicy, checks: &mut Vec<OnboardingCheck>) {
    let diagnostics = policy.diagnostics();
    for action in ["read", "write", "dedup", "admin"] {
        if diagnostics
            .actions
            .iter()
            .any(|candidate| candidate == action)
        {
            checks.push(OnboardingCheck::ok(
                "policy action",
                format!("{action} covered"),
            ));
        } else {
            checks.push(OnboardingCheck::fail(
                "policy action",
                "onboarding_policy_action_missing",
                format!("{action} is not covered by policy.yaml"),
                "Grant read, write, dedup, and admin to the onboarding principal before rollout.",
            ));
        }
    }

    if policy.is_authorized("psk-client", ".crab", "read") {
        checks.push(OnboardingCheck::ok(
            "policy shared objects",
            ".crab read is authorized for the onboarding principal",
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "policy shared objects",
            "onboarding_policy_crab_missing",
            ".crab read is required for shared immutable objects",
            "Include .crab in the onboarding policy repo patterns.",
        ));
    }
}

fn check_client_config(path: &Path, checks: &mut Vec<OnboardingCheck>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client config",
                "onboarding_client_config_unreadable",
                format!("failed to read {}: {error}", path.display()),
                "Regenerate client-config.toml.",
            ));
            return;
        }
    };
    let value = match text.parse::<toml::Value>() {
        Ok(value) => value,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client config",
                "onboarding_client_config_invalid",
                format!("client-config.toml is invalid TOML: {error}"),
                "Fix client-config.toml or rerender the onboarding bundle.",
            ));
            return;
        }
    };
    let cache = value.get("cache").and_then(toml::Value::as_table);
    check_client_field(
        checks,
        cache,
        "service_url",
        "onboarding_client_service_url_missing",
        "cache.service_url is configured",
    );
    check_client_field_equals(
        checks,
        cache,
        "service_mode",
        "cache+dedup",
        "onboarding_client_mode_invalid",
    );
    check_client_field_equals(
        checks,
        cache,
        "service_auth",
        "psk",
        "onboarding_client_auth_invalid",
    );
    if cache
        .and_then(|cache| cache.get("push_warming"))
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        checks.push(OnboardingCheck::ok(
            "client push warming",
            "push_warming is enabled",
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "client push warming",
            "onboarding_client_push_warming_disabled",
            "cache.push_warming must be true for the onboarding profile",
            "Set cache.push_warming = true.",
        ));
    }
}

fn check_client_field(
    checks: &mut Vec<OnboardingCheck>,
    cache: Option<&toml::map::Map<String, toml::Value>>,
    field: &'static str,
    code: &'static str,
    ok_detail: &'static str,
) {
    if cache
        .and_then(|cache| cache.get(field))
        .and_then(toml::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        checks.push(OnboardingCheck::ok("client config", ok_detail));
    } else {
        checks.push(OnboardingCheck::fail(
            "client config",
            code,
            format!("cache.{field} must be configured"),
            "Set service_url, service_mode, service_auth, and push_warming in client-config.toml.",
        ));
    }
}

fn check_client_field_equals(
    checks: &mut Vec<OnboardingCheck>,
    cache: Option<&toml::map::Map<String, toml::Value>>,
    field: &'static str,
    expected: &'static str,
    code: &'static str,
) {
    if cache
        .and_then(|cache| cache.get(field))
        .and_then(toml::Value::as_str)
        == Some(expected)
    {
        checks.push(OnboardingCheck::ok(
            "client config",
            format!("cache.{field} is {expected}"),
        ));
    } else {
        checks.push(OnboardingCheck::fail(
            "client config",
            code,
            format!("cache.{field} must be {expected:?}"),
            "Use the generated client-config.toml or rerender the onboarding bundle.",
        ));
    }
}

async fn probe_onboarding_client(
    bundle_dir: &Path,
    repo_path: &str,
) -> OnboardingClientProbeReport {
    let mut checks = Vec::new();
    let repo_path = repo_path.trim().trim_matches('/').to_string();
    if repo_path.is_empty() || repo_path.contains("..") {
        checks.push(OnboardingCheck::fail(
            "client probe repo",
            "onboarding_client_probe_config_invalid",
            "client probe repo must be a repository path such as org/team/repo",
            "Pass --client-probe-repo with a concrete repository served by this cache instance.",
        ));
        return OnboardingClientProbeReport {
            status: aggregate_status(&checks),
            repo_path,
            service_url: String::new(),
            checks,
        };
    }

    let client_config = match read_client_probe_config(bundle_dir) {
        Ok(config) => {
            checks.push(OnboardingCheck::ok(
                "client probe config",
                "client-config.toml is usable for active probes",
            ));
            config
        }
        Err(detail) => {
            checks.push(OnboardingCheck::fail(
                "client probe config",
                "onboarding_client_probe_config_invalid",
                detail,
                "Fix client-config.toml or rerender the onboarding bundle.",
            ));
            return OnboardingClientProbeReport {
                status: aggregate_status(&checks),
                repo_path,
                service_url: String::new(),
                checks,
            };
        }
    };

    if client_config.service_mode != "cache+dedup"
        || client_config.service_auth != "psk"
        || !client_config.push_warming
    {
        checks.push(OnboardingCheck::fail(
            "client probe config",
            "onboarding_client_probe_config_invalid",
            "client probe requires service_mode cache+dedup, service_auth psk, and push_warming true",
            "Use the generated client-config.toml or rerender the onboarding bundle.",
        ));
        return OnboardingClientProbeReport {
            status: aggregate_status(&checks),
            repo_path,
            service_url: client_config.service_url,
            checks,
        };
    }

    let psk = match std::env::var("CRAB_CACHE_PSK") {
        Ok(value) if !value.trim().is_empty() && value.trim() != CLIENT_PSK_PLACEHOLDER => value,
        _ => {
            checks.push(OnboardingCheck::fail(
                "client probe secret",
                "onboarding_client_probe_secret_missing",
                "CRAB_CACHE_PSK must contain the real PSK for active client probes",
                "Export CRAB_CACHE_PSK from your secret manager before running --client-probe.",
            ));
            return OnboardingClientProbeReport {
                status: aggregate_status(&checks),
                repo_path,
                service_url: redact_url_value(&client_config.service_url),
                checks,
            };
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client probe http",
                "onboarding_client_probe_config_invalid",
                format!("failed to build HTTP client: {error}"),
                "Fix the local TLS/client certificate configuration and rerun the probe.",
            ));
            return OnboardingClientProbeReport {
                status: aggregate_status(&checks),
                repo_path,
                service_url: redact_url_value(&client_config.service_url),
                checks,
            };
        }
    };

    let base_url = client_config.service_url.trim_end_matches('/').to_string();
    probe_health(&client, &base_url, &mut checks).await;
    probe_capabilities(&client, &base_url, &psk, &mut checks).await;
    probe_authz(&client, &base_url, &psk, &repo_path, &mut checks).await;
    probe_active_cache_roundtrip(&client, &base_url, &psk, &repo_path, &mut checks).await;

    OnboardingClientProbeReport {
        status: aggregate_status(&checks),
        repo_path,
        service_url: redact_url_value(&client_config.service_url),
        checks,
    }
}

fn read_client_probe_config(bundle_dir: &Path) -> Result<OnboardingClientConfig, String> {
    let path = bundle_dir.join("client-config.toml");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let value = text
        .parse::<toml::Value>()
        .map_err(|error| format!("client-config.toml is invalid TOML: {error}"))?;
    let cache = value
        .get("cache")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "client-config.toml must contain [cache]".to_string())?;
    let string_field = |field: &'static str| -> Result<String, String> {
        cache
            .get(field)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("cache.{field} must be configured"))
    };
    Ok(OnboardingClientConfig {
        service_url: string_field("service_url")?,
        service_mode: string_field("service_mode")?,
        service_auth: string_field("service_auth")?,
        push_warming: cache
            .get("push_warming")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
    })
}

async fn probe_health(client: &reqwest::Client, base_url: &str, checks: &mut Vec<OnboardingCheck>) {
    match client.get(format!("{base_url}/v1/health")).send().await {
        Ok(response) if response.status().is_success() => {
            checks.push(OnboardingCheck::ok("client probe health", "/v1/health ok"));
        }
        Ok(response) => checks.push(OnboardingCheck::fail(
            "client probe health",
            "onboarding_client_probe_health_failed",
            format!("/v1/health returned HTTP {}", response.status().as_u16()),
            "Start the cache server and verify origin connectivity before rollout.",
        )),
        Err(error) => checks.push(OnboardingCheck::fail(
            "client probe health",
            "onboarding_client_probe_health_failed",
            redact_probe_error(base_url, &error.to_string()),
            "Start the cache server and verify network reachability from this host.",
        )),
    }
}

async fn probe_capabilities(
    client: &reqwest::Client,
    base_url: &str,
    psk: &str,
    checks: &mut Vec<OnboardingCheck>,
) {
    let response = match auth_request(client.get(format!("{base_url}/v1/capabilities")), psk)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client probe capabilities",
                "onboarding_client_probe_capabilities_failed",
                redact_probe_error(base_url, &error.to_string()),
                "Verify CRAB_CACHE_PSK and the cache service URL.",
            ));
            return;
        }
    };
    let status = response.status();
    if !status.is_success() {
        checks.push(OnboardingCheck::fail(
            "client probe capabilities",
            "onboarding_client_probe_capabilities_failed",
            format!("/v1/capabilities returned HTTP {}", status.as_u16()),
            "Verify CRAB_CACHE_PSK and the cache service authorization policy.",
        ));
        return;
    }
    match response.json::<CapabilitiesResponse>().await {
        Ok(snapshot)
            if snapshot.schema.as_deref() == Some(CAPABILITIES_SCHEMA)
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
                    .is_some_and(cache_route_contract_matches_current) =>
        {
            checks.push(OnboardingCheck::ok(
                "client probe capabilities",
                "/v1/capabilities schema, limits, and route contract ok",
            ));
        }
        Ok(_) => checks.push(OnboardingCheck::fail(
            "client probe capabilities",
            "onboarding_client_probe_capabilities_failed",
            "/v1/capabilities did not return expected schema, limits, and route contract",
            "Upgrade crab-cache-server or fix the capabilities endpoint.",
        )),
        Err(error) => checks.push(OnboardingCheck::fail(
            "client probe capabilities",
            "onboarding_client_probe_capabilities_failed",
            format!("failed to parse capabilities JSON: {error}"),
            "Upgrade crab-cache-server or fix the capabilities endpoint.",
        )),
    }
}

async fn probe_authz(
    client: &reqwest::Client,
    base_url: &str,
    psk: &str,
    repo_path: &str,
    checks: &mut Vec<OnboardingCheck>,
) {
    let response = match auth_request(
        client
            .post(format!("{base_url}/v1/authz/check"))
            .json(&serde_json::json!({ "repo_path": repo_path })),
        psk,
    )
    .send()
    .await
    {
        Ok(response) => response,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client probe authz",
                "onboarding_client_probe_authz_failed",
                redact_probe_error(base_url, &error.to_string()),
                "Verify CRAB_CACHE_PSK and the cache service URL.",
            ));
            return;
        }
    };
    let status = response.status();
    if !status.is_success() {
        checks.push(OnboardingCheck::fail(
            "client probe authz",
            "onboarding_client_probe_authz_failed",
            format!("/v1/authz/check returned HTTP {}", status.as_u16()),
            "Grant read, write, dedup, and admin for the probed repository.",
        ));
        return;
    }
    match response.json::<AuthzResponse>().await {
        Ok(snapshot) if authz_allows_enterprise_client(&snapshot) => {
            checks.push(OnboardingCheck::ok(
                "client probe authz",
                "read/write/dedup/admin authorized for probed repo",
            ));
        }
        Ok(_) => checks.push(OnboardingCheck::fail(
            "client probe authz",
            "onboarding_client_probe_authz_failed",
            "authz response did not authorize read, write, dedup, and admin",
            "Grant read, write, dedup, and admin for the probed repository.",
        )),
        Err(error) => checks.push(OnboardingCheck::fail(
            "client probe authz",
            "onboarding_client_probe_authz_failed",
            format!("failed to parse authz JSON: {error}"),
            "Upgrade crab-cache-server or fix the authz endpoint.",
        )),
    }
}

fn authz_allows_enterprise_client(snapshot: &AuthzResponse) -> bool {
    if snapshot.schema.as_deref() != Some(AUTHZ_SCHEMA) {
        return false;
    }
    let Some(actions) = &snapshot.actions else {
        return false;
    };
    actions.read == Some(true)
        && actions.write == Some(true)
        && actions.dedup == Some(true)
        && actions.admin == Some(true)
}

async fn probe_active_cache_roundtrip(
    client: &reqwest::Client,
    base_url: &str,
    psk: &str,
    repo_path: &str,
    checks: &mut Vec<OnboardingCheck>,
) {
    let probe = active_probe::build_active_probe(
        repo_path,
        "crab-onboarding-probe",
        "crab cache-service onboarding active probe",
    );
    match active_probe::run_active_probe(
        client,
        base_url,
        ActiveProbeAuth::Psk(psk),
        "verify CRAB_CACHE_PSK and the cache service authorization policy",
        &probe,
    )
    .await
    {
        Ok(_) => checks.push(OnboardingCheck::ok(
            "client probe cache roundtrip",
            "write/read/range/cleanup ok",
        )),
        Err(detail) => checks.push(OnboardingCheck::fail(
            "client probe cache roundtrip",
            "onboarding_client_probe_cache_failed",
            detail,
            "Grant read, write, and admin permissions, then verify cache-service local storage.",
        )),
    }
}

fn auth_request(request: reqwest::RequestBuilder, psk: &str) -> reqwest::RequestBuilder {
    request.header("x-cache-psk", psk)
}

fn redact_probe_error(base_url: &str, error: &str) -> String {
    error
        .replace(base_url, "configured-redacted")
        .replace(CLIENT_PSK_PLACEHOLDER, "<redacted>")
}

fn redact_url_value(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(mut url) => {
            if !url.username().is_empty() {
                let _ = url.set_username("<redacted>");
            }
            if url.password().is_some() {
                let _ = url.set_password(Some("<redacted>"));
            }
            url.to_string()
        }
        Err(_) => "configured-redacted".to_string(),
    }
}

fn check_client_env(path: &Path, checks: &mut Vec<OnboardingCheck>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            checks.push(OnboardingCheck::fail(
                "client env",
                "onboarding_client_env_unreadable",
                format!("failed to read {}: {error}", path.display()),
                "Regenerate client.env.",
            ));
            return;
        }
    };
    for name in ["CRAB_CACHE_SERVICE_URL", "CRAB_CACHE_PSK"] {
        if text.contains(name) {
            checks.push(OnboardingCheck::ok(
                "client env",
                format!("{name} is present"),
            ));
        } else {
            checks.push(OnboardingCheck::fail(
                "client env",
                "onboarding_client_env_missing",
                format!("{name} is missing"),
                "Regenerate client.env and set the real PSK through your secret manager.",
            ));
        }
    }
}

fn check_secret_redaction(
    bundle_dir: &Path,
    config: Option<&CacheServerConfig>,
    checks: &mut Vec<OnboardingCheck>,
) {
    let Some(AuthConfig::Psk { key_hash }) = config.map(|config| &config.auth) else {
        checks.push(OnboardingCheck::ok(
            "secret redaction",
            "no PSK hash available for bundle redaction check",
        ));
        return;
    };
    let hash_hex = hex_string(key_hash);
    let safe_files = [
        "policy.yaml",
        "client-config.toml",
        "client.env",
        "README.md",
    ];
    for name in safe_files {
        let path = bundle_dir.join(name);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if text.contains(&hash_hex) {
            checks.push(OnboardingCheck::fail(
                "secret redaction",
                "onboarding_secret_hash_leaked",
                format!("{name} contains auth.psk_hash"),
                "Keep auth.psk_hash only in server-config.toml; do not copy it into client files.",
            ));
        }
    }
    if !checks
        .iter()
        .any(|check| check.code == Some("onboarding_secret_hash_leaked"))
    {
        checks.push(OnboardingCheck::ok(
            "secret redaction",
            "client files do not contain auth.psk_hash",
        ));
    }
}

fn aggregate_status(checks: &[OnboardingCheck]) -> PreflightStatus {
    if checks
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
    }
}

fn aggregate_probe_status(
    bundle_status: PreflightStatus,
    preflight_status: Option<PreflightStatus>,
    client_probe_status: Option<PreflightStatus>,
) -> PreflightStatus {
    if bundle_status == PreflightStatus::Fail
        || preflight_status == Some(PreflightStatus::Fail)
        || client_probe_status == Some(PreflightStatus::Fail)
    {
        PreflightStatus::Fail
    } else if bundle_status == PreflightStatus::Warn
        || preflight_status == Some(PreflightStatus::Warn)
        || client_probe_status == Some(PreflightStatus::Warn)
    {
        PreflightStatus::Warn
    } else {
        PreflightStatus::Ok
    }
}

fn hex_string(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn validate_non_empty(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    Ok(())
}

fn validate_repo_prefix(prefix: &str) -> Result<(), String> {
    validate_non_empty("repo-prefix", prefix)?;
    if prefix.starts_with('/') || prefix.contains("..") {
        return Err(format!(
            "repo-prefix {prefix:?} must be a repository prefix such as org/team/*"
        ));
    }
    Ok(())
}

fn validate_psk_hash(hash: &str) -> Result<(), String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("psk-hash must be a 64-character Blake3 hex digest".to_string());
    }
    Ok(())
}

fn write_bundle_file(path: &Path, contents: &str, force: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    if force {
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

impl OnboardingCheck {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: PreflightStatus::Ok,
            detail: detail.into(),
            code: None,
            remediation: None,
        }
    }

    fn fail(
        name: &'static str,
        code: &'static str,
        detail: impl Into<String>,
        remediation: &'static str,
    ) -> Self {
        Self {
            name,
            status: PreflightStatus::Fail,
            detail: detail.into(),
            code: Some(code),
            remediation: Some(remediation),
        }
    }
}

fn server_config(options: &OnboardingRenderOptions) -> String {
    format!(
        r#"# Generated by `crab-cache-server onboarding render`.
# Render the real PSK hash from your secret manager before production.

[server]
listen_addr = {}
drain_timeout_secs = 30
mutable_path_mode = "strict"
policy_path = {}

[auth]
mechanism = "psk"
psk_hash = {}

[origin]
url = {}

[cache]
root = {}
max_bytes = {}

[dedup]
scope = "all"

[eviction]
high_water_ratio = 0.95
low_water_ratio = 0.90

[logging]
format = "json"
level = "info"
"#,
        toml_string(&options.listen_addr),
        toml_string(&options.policy_path),
        toml_string(&options.psk_hash),
        toml_string(&options.origin_url),
        toml_string(&options.cache_root),
        options.max_cache_bytes,
    )
}

fn policy_yaml(options: &OnboardingRenderOptions) -> String {
    let repos = std::iter::once(".crab")
        .chain(options.repo_prefixes.iter().map(String::as_str))
        .map(yaml_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# Generated by `crab-cache-server onboarding render`.
# Replace or split this PSK rule when each team needs isolated credentials.

rules:
  - principal: "psk-client"
    repos: [{repos}]
    actions: ["read", "write", "dedup", "admin"]
"#
    )
}

fn client_config(options: &OnboardingRenderOptions) -> String {
    format!(
        r#"# Generated by `crab-cache-server onboarding render`.
# Install into ~/.crab/local.toml or <repo>/.crab/local.toml.

[cache]
service_url = {}
service_mode = "cache+dedup"
service_auth = "psk"
push_warming = true
"#,
        toml_string(&options.cache_service_url),
    )
}

fn client_env(options: &OnboardingRenderOptions) -> String {
    format!(
        r#"# Generated by `crab-cache-server onboarding render`.
# Source this in shells and CI steps that run `crab`.

export CRAB_CACHE_SERVICE_URL={}
export CRAB_CACHE_PSK={}
export RUST_LOG="${{RUST_LOG:-info,crab::cache=debug}}"
"#,
        shell_string(&options.cache_service_url),
        shell_string(CLIENT_PSK_PLACEHOLDER),
    )
}

fn readme(options: &OnboardingRenderOptions) -> String {
    let repo_prefixes = options.repo_prefixes.join(", ");
    format!(
        r#"# Cache-Service Enterprise Onboarding Bundle

Generated by `crab-cache-server onboarding render`.

Inputs:

- Origin URL: `{}`
- Cache service URL: `{}`
- Repo prefixes: `{}`
- Cache root: `{}`
- Cache bytes: `{}`

## 1. Install Server Files

Check the rendered bundle before installing it:

```bash
crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json
```

The JSON report uses stable `checks[].code` values for CI routing:

| Code | Meaning |
|------|---------|
| `onboarding_file_missing` | Required bundle file is missing. |
| `onboarding_server_config_invalid` | `server-config.toml` could not be parsed. |
| `onboarding_mutable_paths_not_strict` | `server.mutable_path_mode` is not strict. |
| `onboarding_auth_not_enforced` | Enterprise auth is not PSK or mTLS. |
| `onboarding_policy_path_missing` | `server.policy_path` is not configured. |
| `onboarding_cache_budget_invalid` | `cache.max_bytes` is not positive. |
| `onboarding_policy_invalid` | `policy.yaml` could not be loaded. |
| `onboarding_policy_action_missing` | `policy.yaml` lacks read, write, dedup, or admin coverage. |
| `onboarding_policy_crab_missing` | The onboarding principal cannot read `.crab`. |
| `onboarding_client_config_unreadable` | `client-config.toml` could not be read. |
| `onboarding_client_config_invalid` | `client-config.toml` could not be parsed. |
| `onboarding_client_service_url_missing` | `client-config.toml` has no cache service URL. |
| `onboarding_client_mode_invalid` | `service_mode` is not `cache+dedup`. |
| `onboarding_client_auth_invalid` | `service_auth` is not `psk`. |
| `onboarding_client_push_warming_disabled` | `push_warming` is not enabled. |
| `onboarding_client_env_unreadable` | `client.env` could not be read. |
| `onboarding_client_env_missing` | `client.env` is missing required variables. |
| `onboarding_secret_hash_leaked` | Client-facing files contain the server PSK hash. |
| `onboarding_client_probe_config_invalid` | The active probe cannot use `client-config.toml` or the repo path. |
| `onboarding_client_probe_secret_missing` | `CRAB_CACHE_PSK` is not set to the real PSK. |
| `onboarding_client_probe_health_failed` | The active probe cannot reach `/v1/health`. |
| `onboarding_client_probe_capabilities_failed` | `/v1/capabilities` did not return the expected schema, limits, and route contract. |
| `onboarding_client_probe_authz_failed` | `/v1/authz/check` did not authorize the probed repo. |
| `onboarding_client_probe_cache_failed` | Cache write/read/range/cleanup did not complete through the server. |

Run the live probe after origin credentials are available and `policy_path`
points at a readable policy file:

```bash
crab-cache-server onboarding probe --bundle-dir . \
  --json --trusted-proxy-boundary > onboarding-probe.json
```

```bash
install -d -m 0750 /etc/crab-cache-server
install -m 0640 server-config.toml /etc/crab-cache-server/config.toml
install -m 0640 policy.yaml /etc/crab-cache-server/policy.yaml
install -d -m 0750 {}
```

Render with `--policy-path` when your installed policy path differs from
`/etc/crab-cache-server/policy.yaml`.

## 2. Preflight The Server

Use `--trusted-proxy-boundary` only when TLS, client identity, and header
scrubbing are enforced before traffic reaches `crab-cache-server`.

```bash
crab-cache-server --config /etc/crab-cache-server/config.toml check \
  --json --profile enterprise --trusted-proxy-boundary
```

Start the server after preflight is clean:

```bash
crab-cache-server --config /etc/crab-cache-server/config.toml serve
```

After the server is listening, run the active client probe with the same client
config and secret that Crab users will receive:

```bash
export CRAB_CACHE_PSK="<secret-from-secret-manager>"
crab-cache-server onboarding probe --bundle-dir . \
  --json --trusted-proxy-boundary \
  --client-probe --client-probe-repo org/example/repo > onboarding-client-probe.json
```

## 3. Wire Crab Clients

```bash
install -d -m 0750 ~/.crab
install -m 0640 client-config.toml ~/.crab/local.toml
set -a
. ./client.env
set +a
```

Set the real PSK through your shell, CI, or secret manager:

```bash
export CRAB_CACHE_PSK="<secret-from-secret-manager>"
```

Verify from a configured Crab repository:

```bash
crab doctor --json
crab doctor --cache-service-active-probe --json
```

## 4. Prove Object-Store Traffic Reduction

```bash
cd crab
make cache-service-onboarding-rustfs-smoke
make cache-service-release-gate \
  CACHE_SERVICE_RELEASE_EVIDENCE_DIR=../cache-service-release-evidence \
  CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID=gha-<github-run-id>-<attempt>
```
"#,
        options.origin_url,
        options.cache_service_url,
        repo_prefixes,
        options.cache_root,
        options.max_cache_bytes,
        options.cache_root,
    )
}

fn toml_string(value: &str) -> String {
    quoted_string(value)
}

fn yaml_string(value: &str) -> String {
    quoted_string(value)
}

fn shell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn quoted_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}
