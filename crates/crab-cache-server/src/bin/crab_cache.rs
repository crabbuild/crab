use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use crab_cache_server::config::CacheServerConfig;
use crab_cache_server::error::CacheServiceError;
use crab_cache_server::evidence::{
    EvidenceDoctorReport, EvidenceReleaseVerification, EvidenceSummary, EvidenceVerificationReport,
    EvidenceVerificationStatus, doctor_evidence_verification, find_release_evidence_report,
    summarize_evidence_report, verify_evidence_report, verify_release_evidence_report,
};
use crab_cache_server::onboarding::{
    OnboardingCheckReport, OnboardingProbeOptions, OnboardingProbeReport, OnboardingRenderOptions,
    check_onboarding_bundle, probe_onboarding_bundle, render_onboarding_bundle,
};
use crab_cache_server::preflight::{
    CacheServerPreflightReport, PreflightProfile, PreflightProfileOptions, PreflightStatus,
    apply_preflight_profile, redacted_origin_url, run_preflight,
};
use crab_cache_server::server::run_server;

/// Crab org-level cache server.
///
/// Caches immutable objects on local NVMe storage and serves cross-repo
/// chunk dedup queries for an organization.
#[derive(Parser)]
#[command(name = "crab-cache-server", version)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short = 'c', value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start serving cache-service HTTP traffic.
    Serve,
    /// Validate server startup readiness without serving traffic.
    #[command(visible_alias = "doctor", visible_alias = "preflight")]
    Check {
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
        /// Return a non-zero exit code when preflight emits warnings.
        #[arg(long)]
        fail_on_warn: bool,
        /// Additional deployment policy to enforce.
        #[arg(long, value_enum, default_value = "standard")]
        profile: CheckProfile,
        /// Assert a trusted proxy or service mesh protects TLS and identity headers.
        #[arg(long)]
        trusted_proxy_boundary: bool,
    },
    /// Verify retained cache-service evidence artifacts.
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommand,
    },
    /// Generate enterprise cache-service onboarding files.
    Onboarding {
        #[command(subcommand)]
        command: OnboardingCommand,
    },
}

#[derive(Subcommand)]
enum EvidenceCommand {
    /// Verify a retained RustFS smoke report and its evidence manifest.
    Verify {
        /// Path to the retained report.json file.
        #[arg(long, value_name = "FILE")]
        report: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Verify retained RustFS smoke evidence for a release gate.
    ReleaseVerify {
        /// Path to the retained report.json file.
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with = "evidence_dir",
            required_unless_present = "evidence_dir"
        )]
        report: Option<PathBuf>,
        /// Directory containing the retained cache-service smoke artifact.
        #[arg(long, value_name = "DIR", conflicts_with = "report")]
        evidence_dir: Option<PathBuf>,
        /// Expected report run id, usually `gha-<workflow-run-id>-<attempt>`.
        #[arg(long, value_name = "RUN_ID")]
        expected_run_id: String,
        /// Write structured verification JSON to this file.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Write structured evidence summary JSON to this file.
        #[arg(long, value_name = "FILE")]
        summary_output: Option<PathBuf>,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Run the release evidence gate and write diagnosis artifacts.
    Gate {
        /// Path to the retained report.json file.
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with = "evidence_dir",
            required_unless_present = "evidence_dir"
        )]
        report: Option<PathBuf>,
        /// Directory containing the retained cache-service smoke artifact.
        #[arg(long, value_name = "DIR", conflicts_with = "report")]
        evidence_dir: Option<PathBuf>,
        /// Expected report run id, usually `gha-<workflow-run-id>-<attempt>`.
        #[arg(long, value_name = "RUN_ID")]
        expected_run_id: String,
        /// Write structured verification JSON to this file.
        #[arg(
            long,
            value_name = "FILE",
            default_value = "cache-service-release-evidence-verify.json"
        )]
        output: PathBuf,
        /// Write structured evidence summary JSON to this file.
        #[arg(
            long,
            value_name = "FILE",
            default_value = "cache-service-release-evidence-summary.json"
        )]
        summary_output: PathBuf,
        /// Write structured doctor JSON here when verification fails.
        #[arg(
            long,
            value_name = "FILE",
            default_value = "cache-service-release-evidence-doctor.json"
        )]
        doctor_output: PathBuf,
        /// Write human-readable doctor output here when verification fails.
        #[arg(
            long,
            value_name = "FILE",
            default_value = "cache-service-release-evidence-doctor.txt"
        )]
        doctor_text_output: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Summarize retained cache-service proof for support handoff.
    Summarize {
        /// Path to the retained report.json file.
        #[arg(long, value_name = "FILE")]
        report: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose a release evidence verification JSON.
    Doctor {
        /// Path to release-verify JSON written with --output.
        #[arg(long, value_name = "FILE")]
        verification: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum OnboardingCommand {
    /// Render a strict-mode enterprise onboarding bundle.
    Render {
        /// Directory that will receive the generated files.
        #[arg(
            long,
            value_name = "DIR",
            default_value = "cache-service-enterprise-onboarding"
        )]
        output_dir: PathBuf,
        /// Object-store origin URL, such as s3://bucket or s3://bucket/prefix.
        #[arg(long, value_name = "URL")]
        origin_url: String,
        /// Cache-service URL that Crab clients should use.
        #[arg(long, value_name = "URL")]
        cache_service_url: String,
        /// Repository prefix served by this cache instance. Repeat for more prefixes.
        #[arg(long, value_name = "PREFIX", required = true)]
        repo_prefix: Vec<String>,
        /// Blake3 hash of the cache-service PSK. Do not pass the raw secret.
        #[arg(long, value_name = "64_HEX")]
        psk_hash: String,
        /// Cache root used by crab-cache-server.
        #[arg(long, value_name = "PATH", default_value = "/data/crab-cache")]
        cache_root: String,
        /// Cache size budget in bytes.
        #[arg(long, value_name = "BYTES", default_value_t = 1_099_511_627_776_u64)]
        max_cache_bytes: u64,
        /// Listen address for the generated server config.
        #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:8443")]
        listen_addr: String,
        /// Policy path written into the generated server config.
        #[arg(
            long,
            value_name = "PATH",
            default_value = "/etc/crab-cache-server/policy.yaml"
        )]
        policy_path: String,
        /// Overwrite files in the output directory.
        #[arg(long)]
        force: bool,
    },
    /// Check a rendered enterprise onboarding bundle.
    Check {
        /// Directory containing the generated onboarding files.
        #[arg(
            long,
            value_name = "DIR",
            default_value = "cache-service-enterprise-onboarding"
        )]
        bundle_dir: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Run static bundle checks plus live server preflight.
    #[command(visible_alias = "live-check")]
    Probe {
        /// Directory containing the generated onboarding files.
        #[arg(
            long,
            value_name = "DIR",
            default_value = "cache-service-enterprise-onboarding"
        )]
        bundle_dir: PathBuf,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
        /// Return a non-zero exit code when the live probe emits warnings.
        #[arg(long)]
        fail_on_warn: bool,
        /// Trust that TLS, client identity, and proxy header scrubbing are enforced upstream.
        #[arg(long)]
        trusted_proxy_boundary: bool,
        /// Actively verify the generated client config against a running cache server.
        #[arg(long)]
        client_probe: bool,
        /// Repository path authorized for the active client probe.
        #[arg(long, value_name = "REPO")]
        client_probe_repo: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CheckProfile {
    Standard,
    Enterprise,
}

impl From<CheckProfile> for PreflightProfile {
    fn from(profile: CheckProfile) -> Self {
        match profile {
            CheckProfile::Standard => Self::Standard,
            CheckProfile::Enterprise => Self::Enterprise,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    match command {
        Command::Serve => {
            let Some(config) = load_config(cli.config.as_ref(), false) else {
                return ExitCode::from(1);
            };
            init_tracing();
            run(config)
        }
        Command::Check {
            json,
            fail_on_warn,
            profile,
            trusted_proxy_boundary,
        } => {
            let Some(config) = load_config(cli.config.as_ref(), json) else {
                return ExitCode::from(1);
            };
            init_tracing();
            check(config, json, fail_on_warn, profile, trusted_proxy_boundary)
        }
        Command::Evidence { command } => evidence(command),
        Command::Onboarding { command } => onboarding(command),
    }
}

fn load_config(path: Option<&PathBuf>, json: bool) -> Option<CacheServerConfig> {
    let Some(path) = path else {
        let err = CacheServiceError::ConfigError("--config is required".to_string());
        if json {
            emit_preflight_json(&CacheServerPreflightReport::from_config_error(&err));
        } else {
            eprintln!("error: {err}");
        }
        return None;
    };
    match CacheServerConfig::from_file(path) {
        Ok(config) => Some(config),
        Err(err) => {
            if json {
                emit_preflight_json(&CacheServerPreflightReport::from_config_error(&err));
            } else {
                eprintln!("error: {err}");
            }
            None
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

fn run(config: CacheServerConfig) -> ExitCode {
    tracing::info!(
        listen_addr = %config.listen_addr,
        origin_url = %redacted_origin_url(&config),
        cache_root = %config.cache_root.display(),
        max_cache_bytes = config.max_cache_bytes,
        "crab-cache-server configured"
    );

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    match rt.block_on(run_server(config)) {
        Ok(()) => {
            tracing::info!("crab-cache-server shut down cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "crab-cache-server exited with error");
            ExitCode::from(1)
        }
    }
}

fn check(
    config: CacheServerConfig,
    json: bool,
    fail_on_warn: bool,
    profile: CheckProfile,
    trusted_proxy_boundary: bool,
) -> ExitCode {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let report = apply_preflight_profile(
        rt.block_on(run_preflight(config)),
        PreflightProfileOptions {
            profile: profile.into(),
            trusted_proxy_boundary,
        },
    );

    if json {
        emit_preflight_json(&report);
    } else if let Err(e) = report.write_text(std::io::stdout()) {
        eprintln!("error: failed to write preflight output: {e}");
        return ExitCode::from(1);
    }

    if preflight_exit_success(&report, fail_on_warn) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn preflight_exit_success(report: &CacheServerPreflightReport, fail_on_warn: bool) -> bool {
    status_exit_success(report.status, fail_on_warn)
}

fn status_exit_success(status: PreflightStatus, fail_on_warn: bool) -> bool {
    match status {
        PreflightStatus::Ok => true,
        PreflightStatus::Warn => !fail_on_warn,
        PreflightStatus::Fail => false,
    }
}

fn evidence(command: EvidenceCommand) -> ExitCode {
    match command {
        EvidenceCommand::Verify { report, json } => {
            let verification = verify_evidence_report(&report);
            if json {
                emit_evidence_json(&verification);
            } else {
                emit_evidence_text(&verification);
            }
            if verification.status == EvidenceVerificationStatus::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        EvidenceCommand::ReleaseVerify {
            report,
            evidence_dir,
            expected_run_id,
            output,
            summary_output,
            json,
        } => {
            let Some(report) = release_report_path(report.as_deref(), evidence_dir.as_deref())
            else {
                return ExitCode::from(1);
            };
            let verification = verify_release_evidence_report(&report, &expected_run_id);
            if let Some(output) = output.as_deref()
                && !write_json_output(output, &verification, "release evidence verification")
            {
                return ExitCode::from(1);
            }
            if let Some(output) = summary_output.as_deref() {
                let summary = summarize_evidence_report(&report);
                if !write_json_output(output, &summary, "release evidence summary") {
                    return ExitCode::from(1);
                }
            }
            if json {
                emit_release_evidence_json(&verification);
            } else {
                emit_release_evidence_text(&verification);
            }
            if verification.status == EvidenceVerificationStatus::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        EvidenceCommand::Gate {
            report,
            evidence_dir,
            expected_run_id,
            output,
            summary_output,
            doctor_output,
            doctor_text_output,
            json,
        } => {
            let Some(report) = release_report_path(report.as_deref(), evidence_dir.as_deref())
            else {
                return ExitCode::from(1);
            };
            let verification = verify_release_evidence_report(&report, &expected_run_id);
            if !write_json_output(&output, &verification, "release evidence verification") {
                return ExitCode::from(1);
            }

            let summary = summarize_evidence_report(&report);
            if !write_json_output(&summary_output, &summary, "release evidence summary") {
                return ExitCode::from(1);
            }

            let doctor = if verification.status == EvidenceVerificationStatus::Failed {
                let doctor = doctor_evidence_verification(&output);
                if !write_json_output(&doctor_output, &doctor, "release evidence doctor") {
                    return ExitCode::from(1);
                }
                if !write_text_output(
                    &doctor_text_output,
                    &evidence_doctor_text(&doctor),
                    "release evidence doctor text",
                ) {
                    return ExitCode::from(1);
                }
                Some(doctor)
            } else {
                None
            };

            if json {
                emit_release_evidence_json(&verification);
            } else {
                emit_release_evidence_text(&verification);
                if let Some(doctor) = &doctor {
                    emit_evidence_doctor_text(doctor);
                }
            }
            if verification.status == EvidenceVerificationStatus::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        EvidenceCommand::Summarize { report, json } => {
            let summary = summarize_evidence_report(&report);
            if json {
                emit_evidence_summary_json(&summary);
            } else {
                emit_evidence_summary_text(&summary);
            }
            if summary.status == EvidenceVerificationStatus::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        EvidenceCommand::Doctor { verification, json } => {
            let report = doctor_evidence_verification(&verification);
            if json {
                emit_evidence_doctor_json(&report);
            } else {
                emit_evidence_doctor_text(&report);
            }
            if report.status == EvidenceVerificationStatus::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn onboarding(command: OnboardingCommand) -> ExitCode {
    match command {
        OnboardingCommand::Render {
            output_dir,
            origin_url,
            cache_service_url,
            repo_prefix,
            psk_hash,
            cache_root,
            max_cache_bytes,
            listen_addr,
            policy_path,
            force,
        } => {
            let mut options = OnboardingRenderOptions::with_defaults(
                output_dir,
                origin_url,
                cache_service_url,
                repo_prefix,
                psk_hash,
                force,
            );
            options.cache_root = cache_root;
            options.max_cache_bytes = max_cache_bytes;
            options.listen_addr = listen_addr;
            options.policy_path = policy_path;

            match render_onboarding_bundle(&options) {
                Ok(bundle) => {
                    println!(
                        "wrote cache-service enterprise onboarding bundle: {}",
                        bundle.output_dir.display()
                    );
                    for file in bundle.files {
                        println!("  - {}", file.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        OnboardingCommand::Check { bundle_dir, json } => {
            let report = check_onboarding_bundle(&bundle_dir);
            if json {
                emit_onboarding_check_json(&report);
            } else {
                emit_onboarding_check_text(&report);
            }
            if report.status == PreflightStatus::Fail {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        OnboardingCommand::Probe {
            bundle_dir,
            json,
            fail_on_warn,
            trusted_proxy_boundary,
            client_probe,
            client_probe_repo,
        } => {
            let client_probe_repo = if client_probe {
                match client_probe_repo {
                    Some(repo) => Some(repo),
                    None => {
                        eprintln!("error: --client-probe requires --client-probe-repo");
                        return ExitCode::from(2);
                    }
                }
            } else {
                None
            };
            let options = OnboardingProbeOptions {
                trusted_proxy_boundary,
                client_probe_repo,
            };
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            let report = rt.block_on(probe_onboarding_bundle(&bundle_dir, &options));
            if json {
                emit_onboarding_probe_json(&report);
            } else {
                emit_onboarding_probe_text(&report);
            }
            if status_exit_success(report.status, fail_on_warn) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn release_report_path(report: Option<&Path>, evidence_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(report) = report {
        return Some(report.to_path_buf());
    }

    let Some(evidence_dir) = evidence_dir else {
        eprintln!("error: --report or --evidence-dir is required");
        return None;
    };
    match find_release_evidence_report(evidence_dir) {
        Ok(report) => Some(report),
        Err(error) => {
            eprintln!("error: {error}");
            None
        }
    }
}

fn emit_release_evidence_text(report: &EvidenceReleaseVerification) {
    println!(
        "cache service release evidence: {}",
        evidence_status(report.status)
    );
    println!("report: {}", report.report);
    println!("expected_run_id: {}", report.expected_run_id);
    if let Some(run_id) = &report.run_id {
        println!("run_id: {run_id}");
    }
    println!("verified checks: {}", report.verified_checks);
    let failed = report.failed_check_names();
    if !failed.is_empty() {
        println!("failed checks:");
        for name in failed {
            println!("  - {name}");
        }
    }
}

fn emit_evidence_text(report: &EvidenceVerificationReport) {
    println!("cache service evidence: {}", evidence_status(report.status));
    println!("report: {}", report.report);
    if let Some(run_id) = &report.run_id {
        println!("run_id: {run_id}");
    }
    println!("verified checks: {}", report.verified_checks);
    let failed = report.failed_check_names();
    if !failed.is_empty() {
        println!("failed checks:");
        for name in failed {
            println!("  - {name}");
        }
    }
}

fn emit_evidence_summary_text(summary: &EvidenceSummary) {
    println!(
        "cache service evidence: {}",
        evidence_status(summary.status)
    );
    println!("report: {}", summary.report);
    if let Some(run_id) = &summary.run_id {
        println!("run_id: {run_id}");
    }
    println!("verified checks: {}", summary.verified_checks);
    println!(
        "cache: hit_rate={} hits={} origin_avoided={} origin_fetches={} fallback_rate={}",
        fmt_f64(summary.cache.cache_hit_rate),
        fmt_f64(summary.cache.cache_hit_total),
        fmt_f64(summary.cache.origin_avoided_reads_total),
        fmt_f64(summary.cache.origin_fetch_total),
        fmt_f64(summary.cache.origin_fallback_rate),
    );
    for hydrate in &summary.hydrates {
        println!(
            "hydrate {}: origin_get_delta={} origin_fetches={} cache_hits={} cache_misses={} origin_avoided={}",
            hydrate.name,
            fmt_i64(hydrate.origin_gets_delta),
            fmt_i64(hydrate.origin_fetches_delta),
            fmt_i64(hydrate.cache_hits_delta),
            fmt_i64(hydrate.cache_misses_delta),
            fmt_i64(hydrate.origin_avoided_reads_delta),
        );
    }
    if let Some(dedup) = &summary.dedup {
        println!(
            "dedup: queries={} known_chunks={} cacheable_origin_gets={} mutable_origin_gets={} xorb_puts={}",
            fmt_i64(dedup.dedup_queries_delta),
            fmt_i64(dedup.dedup_known_chunks_delta),
            fmt_i64(dedup.cacheable_origin_gets_delta),
            fmt_i64(dedup.mutable_origin_gets_delta),
            fmt_i64(dedup.xorb_puts_delta),
        );
    }
    println!(
        "enterprise: preflight={} policy={} mutable_paths={} max_object_bytes={} policy_rules={} authz_read={} authz_write={} authz_dedup={} authz_admin={}",
        fmt_str(summary.enterprise.preflight_status.as_deref()),
        fmt_str(summary.enterprise.policy.as_deref()),
        fmt_str(summary.enterprise.mutable_path_mode.as_deref()),
        fmt_i64(summary.enterprise.max_object_bytes),
        fmt_i64(summary.enterprise.policy_rule_count),
        fmt_bool(summary.enterprise.authz_read),
        fmt_bool(summary.enterprise.authz_write),
        fmt_bool(summary.enterprise.authz_dedup),
        fmt_bool(summary.enterprise.authz_admin),
    );
    println!(
        "routes: capabilities_status={} route_schema={} prefix={} immutable={}/{} mutable={}/{} retired={} read_probes={} read_unique={} write_probes={} write_unique={}",
        fmt_i64(summary.routes.capabilities_status),
        fmt_str(summary.routes.route_schema.as_deref()),
        fmt_str(summary.routes.route_transport_prefix.as_deref()),
        fmt_usize(summary.routes.immutable_route_count),
        summary.routes.expected_immutable_route_count,
        fmt_usize(summary.routes.mutable_route_count),
        summary.routes.expected_mutable_route_count,
        fmt_usize(summary.routes.retired_route_count),
        fmt_usize(summary.routes.mutable_read_probe_count),
        fmt_usize(summary.routes.mutable_read_probe_unique_patterns),
        fmt_usize(summary.routes.mutable_write_probe_count),
        fmt_usize(summary.routes.mutable_write_probe_unique_patterns),
    );
    if !summary.routes.retired_routes.is_empty() {
        println!("retired routes:");
        for route in &summary.routes.retired_routes {
            println!("  - {route}");
        }
    }
    if !summary.artifacts.is_empty() {
        println!("artifacts:");
        for artifact in &summary.artifacts {
            println!(
                "  - {} sha256={} bytes={}",
                artifact.name,
                fmt_str(artifact.sha256.as_deref()),
                artifact
                    .bytes
                    .map_or_else(|| "missing".to_string(), |bytes| bytes.to_string()),
            );
        }
    }
    if !summary.failed_checks.is_empty() {
        println!("failed checks:");
        for name in &summary.failed_checks {
            println!("  - {name}");
        }
    }
}

fn emit_evidence_doctor_text(report: &EvidenceDoctorReport) {
    print!("{}", evidence_doctor_text(report));
}

fn evidence_doctor_text(report: &EvidenceDoctorReport) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "cache service evidence doctor: {}\n",
        evidence_status(report.status)
    ));
    output.push_str(&format!("verification: {}\n", report.verification));
    if report.categories.is_empty() {
        output.push_str("actions: none\n");
        return output;
    }
    output.push_str("actions:\n");
    for category in &report.categories {
        output.push_str(&format!("  - {}: {}\n", category.category, category.title));
        output.push_str(&format!("    checks: {}\n", category.checks.join(", ")));
        for detail in &category.details {
            output.push_str(&format!("    detail: {detail}\n"));
        }
        output.push_str(&format!("    remediation: {}\n", category.remediation));
    }
    output
}

fn emit_evidence_json(report: &EvidenceVerificationReport) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write evidence verification JSON: {e}");
    } else {
        println!();
    }
}

fn emit_release_evidence_json(report: &EvidenceReleaseVerification) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write release evidence verification JSON: {e}");
    } else {
        println!();
    }
}

fn emit_evidence_doctor_json(report: &EvidenceDoctorReport) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write evidence doctor JSON: {e}");
    } else {
        println!();
    }
}

fn write_json_output<T: serde::Serialize>(path: &Path, value: &T, label: &str) -> bool {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "error: failed to create {label} output directory {}: {error}",
            parent.display()
        );
        return false;
    }
    let file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "error: failed to create {label} output {}: {error}",
                path.display()
            );
            return false;
        }
    };
    if let Err(error) = serde_json::to_writer_pretty(file, value) {
        eprintln!(
            "error: failed to write {label} output {}: {error}",
            path.display()
        );
        return false;
    }
    if let Err(error) = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(b"\n")
        })
    {
        eprintln!(
            "error: failed to finalize {label} output {}: {error}",
            path.display()
        );
        return false;
    }
    true
}

fn write_text_output(path: &Path, value: &str, label: &str) -> bool {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "error: failed to create {label} output directory {}: {error}",
            parent.display()
        );
        return false;
    }
    if let Err(error) = std::fs::write(path, value) {
        eprintln!(
            "error: failed to write {label} output {}: {error}",
            path.display()
        );
        return false;
    }
    true
}

fn emit_evidence_summary_json(summary: &EvidenceSummary) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), summary) {
        eprintln!("error: failed to write evidence summary JSON: {e}");
    } else {
        println!();
    }
}

fn emit_preflight_json(report: &CacheServerPreflightReport) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write preflight JSON: {e}");
    } else {
        println!();
    }
}

fn emit_onboarding_check_text(report: &OnboardingCheckReport) {
    println!(
        "cache-service onboarding check: {}",
        preflight_status(report.status)
    );
    println!("bundle: {}", report.bundle_dir);
    for check in &report.checks {
        println!(
            "[{}] {}: {}",
            preflight_status(check.status),
            check.name,
            check.detail
        );
        if let Some(code) = check.code {
            println!("  code: {code}");
        }
        if let Some(remediation) = check.remediation {
            println!("  remediation: {remediation}");
        }
    }
}

fn emit_onboarding_check_json(report: &OnboardingCheckReport) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write onboarding check JSON: {e}");
    } else {
        println!();
    }
}

fn emit_onboarding_probe_text(report: &OnboardingProbeReport) {
    println!(
        "cache-service onboarding probe: {}",
        preflight_status(report.status)
    );
    println!(
        "bundle check: {}",
        preflight_status(report.bundle_check.status)
    );
    println!("bundle: {}", report.bundle_check.bundle_dir);
    for check in &report.bundle_check.checks {
        println!(
            "[{}] bundle {}: {}",
            preflight_status(check.status),
            check.name,
            check.detail
        );
    }
    if let Some(server_preflight) = &report.server_preflight {
        println!(
            "server preflight: {}",
            preflight_status(server_preflight.status)
        );
        for check in &server_preflight.checks {
            println!(
                "[{}] server {}: {}",
                preflight_status(check.status),
                check.name,
                check.detail
            );
        }
    }
    if let Some(client_probe) = &report.client_probe {
        println!("client probe: {}", preflight_status(client_probe.status));
        println!("client probe repo: {}", client_probe.repo_path);
        println!("client probe service: {}", client_probe.service_url);
        for check in &client_probe.checks {
            println!(
                "[{}] client {}: {}",
                preflight_status(check.status),
                check.name,
                check.detail
            );
        }
    }
}

fn emit_onboarding_probe_json(report: &OnboardingProbeReport) {
    if let Err(e) = serde_json::to_writer_pretty(std::io::stdout(), report) {
        eprintln!("error: failed to write onboarding probe JSON: {e}");
    } else {
        println!();
    }
}

fn preflight_status(status: PreflightStatus) -> &'static str {
    match status {
        PreflightStatus::Ok => "ok",
        PreflightStatus::Warn => "warn",
        PreflightStatus::Fail => "fail",
    }
}

fn evidence_status(status: EvidenceVerificationStatus) -> &'static str {
    match status {
        EvidenceVerificationStatus::Passed => "passed",
        EvidenceVerificationStatus::Failed => "failed",
    }
}

fn fmt_str(value: Option<&str>) -> &str {
    value.unwrap_or("missing")
}

fn fmt_i64(value: Option<i64>) -> String {
    value.map_or_else(|| "missing".to_string(), |value| value.to_string())
}

fn fmt_usize(value: Option<usize>) -> String {
    value.map_or_else(|| "missing".to_string(), |value| value.to_string())
}

fn fmt_f64(value: Option<f64>) -> String {
    value.map_or_else(|| "missing".to_string(), |value| format!("{value:.3}"))
}

fn fmt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "missing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(status: PreflightStatus) -> CacheServerPreflightReport {
        CacheServerPreflightReport {
            status,
            summary: None,
            checks: Vec::new(),
        }
    }

    #[test]
    fn preflight_exit_allows_warnings_by_default() {
        assert!(preflight_exit_success(
            &report(PreflightStatus::Warn),
            false
        ));
    }

    #[test]
    fn preflight_exit_rejects_warnings_when_requested() {
        assert!(!preflight_exit_success(
            &report(PreflightStatus::Warn),
            true
        ));
    }

    #[test]
    fn preflight_exit_always_rejects_failures() {
        assert!(!preflight_exit_success(
            &report(PreflightStatus::Fail),
            false
        ));
        assert!(!preflight_exit_success(
            &report(PreflightStatus::Fail),
            true
        ));
    }
}
