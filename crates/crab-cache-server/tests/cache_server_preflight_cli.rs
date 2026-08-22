//! CLI contract tests for `crab-cache-server`.

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crab_cache_server::auth::AuthPolicy;
use crab_cache_server::config::CacheServerConfig;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab-cache-server")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("cache-server crate has repository grandparent")
        .to_path_buf()
}

fn workflow_body(path: &str) -> String {
    fs::read_to_string(repo_root().join(path)).expect("read workflow")
}

const DEFAULT_PSK_BLAKE3: &str = "4fb898757c4c93662343bbbb25419f8c4f9c979352d40ff896578cabf620cf6e";
const STABLE_ONBOARDING_CHECK_CODES: &[&str] = &[
    "onboarding_file_missing",
    "onboarding_server_config_invalid",
    "onboarding_mutable_paths_not_strict",
    "onboarding_auth_not_enforced",
    "onboarding_policy_path_missing",
    "onboarding_cache_budget_invalid",
    "onboarding_policy_invalid",
    "onboarding_policy_action_missing",
    "onboarding_policy_crab_missing",
    "onboarding_client_config_unreadable",
    "onboarding_client_config_invalid",
    "onboarding_client_service_url_missing",
    "onboarding_client_mode_invalid",
    "onboarding_client_auth_invalid",
    "onboarding_client_push_warming_disabled",
    "onboarding_client_env_unreadable",
    "onboarding_client_env_missing",
    "onboarding_secret_hash_leaked",
    "onboarding_client_probe_config_invalid",
    "onboarding_client_probe_secret_missing",
    "onboarding_client_probe_health_failed",
    "onboarding_client_probe_capabilities_failed",
    "onboarding_client_probe_authz_failed",
    "onboarding_client_probe_cache_failed",
];

#[test]
fn cache_service_workflow_gates_client_cache_integration_surfaces() {
    let body = workflow_body(".github/workflows/cache-service.yml");

    for path in [
        "crates/crab-cache-server/**",
        "crates/crab-cache/**",
        "crab/src/cache/**",
        "crab/src/git/**",
        "crab/src/metadata/**",
        "crab/src/storage/**",
        "crab/src/cmd/add.rs",
        "crab/src/cmd/clone.rs",
        "crab/src/cmd/doctor.rs",
        "crab/src/cmd/hydrate.rs",
    ] {
        assert!(body.contains(path), "{path}");
    }

    for gate in [
        "make cache-service-rustfs-smoke",
        "make cache-service-verify-smoke-report",
        "target/debug/crab-cache-server evidence verify",
        "target/debug/crab-cache-server evidence summarize",
        "name: cache-service-rustfs-smoke-${{ github.run_id }}-${{ github.run_attempt }}",
    ] {
        assert!(body.contains(gate), "{gate}");
    }
}

#[test]
fn makefile_exposes_cache_service_release_gate() {
    let body = workflow_body("crab/Makefile");

    for needle in [
        "make cache-service-release-gate",
        "CACHE_SERVICE_RELEASE_EVIDENCE_DIR ?= ../cache-service-release-evidence",
        "CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID ?=",
        "CACHE_SERVICE_RELEASE_VERIFY_OUTPUT ?= cache-service-release-evidence-verify.json",
        "CACHE_SERVICE_RELEASE_DOCTOR_TEXT_OUTPUT ?= cache-service-release-evidence-doctor.txt",
        "CACHE_SERVICE_RELEASE_REQUIRE_EVIDENCE ?= 1",
        "make cache-service-onboarding-rustfs-smoke",
        "cache-service-onboarding-rustfs-smoke: cache-service-rustfs-smoke",
        "Prove onboarding bundle wiring and traffic reduction against RustFS/S3",
        "cache-service-release-gate:",
        "\"$(CACHE_SERVER_DEBUG_BIN)\" evidence gate",
        "--evidence-dir \"$(CACHE_SERVICE_RELEASE_EVIDENCE_DIR)\"",
        "--expected-run-id \"$(CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID)\"",
        "release-build: replica-release-gate cache-service-release-gate",
        "release-strict: replica-release-gate cache-service-release-gate",
        "release-macos-full: replica-release-gate cache-service-release-gate",
        "@$(MAKE) --no-print-directory cache-service-release-gate",
    ] {
        assert!(body.contains(needle), "{needle}");
    }
}

#[test]
fn enterprise_onboarding_bundle_matches_cache_service_contract() {
    let bundle = repo_root().join("crab/deploy/cache-service/enterprise-onboarding");
    let server_config_path = bundle.join("server-config.toml");
    let policy_path = bundle.join("policy.yaml");
    let client_config_path = bundle.join("client-config.toml");
    let client_env_path = bundle.join("client.env");
    let readme_path = bundle.join("README.md");

    let server_config = fs::read_to_string(&server_config_path).unwrap();
    let parsed_config = CacheServerConfig::from_toml_str(&server_config).unwrap();
    assert_eq!(
        parsed_config.policy_path.as_deref(),
        Some(Path::new("/etc/crab-cache-server/policy.yaml"))
    );
    assert!(server_config.contains("mutable_path_mode = \"strict\""));
    assert!(server_config.contains("url = \"s3://crab\""));

    let policy = AuthPolicy::from_file(&policy_path).unwrap();
    for action in ["read", "write", "dedup"] {
        assert!(
            policy.is_authorized("psk-client", "org/example/repo", action),
            "sample policy should authorize {action}"
        );
    }
    assert!(policy.is_authorized("psk-client", ".crab", "read"));
    assert!(policy.has_action("psk-client", "admin"));

    let client_config = fs::read_to_string(&client_config_path).unwrap();
    for needle in [
        "service_url = \"https://crab-cache.example.com:8443\"",
        "service_mode = \"cache+dedup\"",
        "service_auth = \"psk\"",
        "push_warming = true",
    ] {
        assert!(client_config.contains(needle), "{needle}");
    }

    let client_env = fs::read_to_string(&client_env_path).unwrap();
    for needle in [
        "export CRAB_CACHE_SERVICE_URL=\"https://crab-cache.example.com:8443\"",
        "export CRAB_CACHE_PSK=\"replace-with-long-random-secret\"",
    ] {
        assert!(client_env.contains(needle), "{needle}");
    }

    let readme = fs::read_to_string(&readme_path).unwrap();
    for needle in [
        "crab-cache-server onboarding render",
        "crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json",
        "crab-cache-server onboarding probe --bundle-dir .",
        "onboarding-probe.json",
        "--client-probe --client-probe-repo org/example/repo",
        "onboarding-client-probe.json",
        "CRAB_CACHE_PSK=\"<secret-from-secret-manager>\"",
        "stable `checks[].code` values",
        "crab-cache-server --config /etc/crab-cache-server/config.toml check",
        "--json --profile enterprise --trusted-proxy-boundary",
        "crab doctor --cache-service-active-probe --json",
        "make cache-service-onboarding-rustfs-smoke",
        "make cache-service-release-gate",
        "--policy-path",
        "CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID=gha-<github-run-id>-<attempt>",
    ] {
        assert!(readme.contains(needle), "{needle}");
    }
    for code in STABLE_ONBOARDING_CHECK_CODES {
        assert!(readme.contains(code), "{code}");
    }
}

#[test]
fn onboarding_check_stable_codes_are_documented_for_ci() {
    let readme = workflow_body("crab/deploy/cache-service/enterprise-onboarding/README.md");
    let deployment = workflow_body("packages/web/content/docs/cli/cache-service/deployment.mdx");

    for body in [&readme, &deployment] {
        assert!(body.contains("--json > onboarding-check.json"));
        assert!(body.contains("onboarding probe"));
        assert!(body.contains("onboarding-probe.json"));
        assert!(body.contains("--client-probe"));
        assert!(body.contains("--client-probe-repo"));
        assert!(body.contains("onboarding-client-probe.json"));
        assert!(body.contains("CRAB_CACHE_PSK"));
        assert!(body.contains("stable `checks[].code` values"));
        assert!(body.contains("--policy-path"));
        assert!(body.contains("make cache-service-onboarding-rustfs-smoke"));
        for code in STABLE_ONBOARDING_CHECK_CODES {
            assert!(body.contains(code), "{code}");
        }
    }
}

#[test]
fn onboarding_render_writes_parsable_enterprise_bundle() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");
    let cache_root = fixture.path().join("cache");

    let output = run_onboarding_render(
        &output_dir,
        DEFAULT_PSK_BLAKE3,
        &[
            ("--origin-url", "s3://enterprise-crab"),
            (
                "--cache-service-url",
                "https://crab-cache.enterprise.example:8443",
            ),
            ("--repo-prefix", "org/team-a/*"),
            ("--repo-prefix", "org/shared/*"),
            ("--cache-root", cache_root.to_str().unwrap()),
            ("--max-cache-bytes", "1048576"),
            ("--policy-path", "/tmp/crab-cache-server/policy.yaml"),
        ],
    );
    assert_succeeded(&output);

    let server_config = fs::read_to_string(output_dir.join("server-config.toml")).unwrap();
    let parsed_config = CacheServerConfig::from_toml_str(&server_config).unwrap();
    assert_eq!(parsed_config.max_cache_bytes, 1048576);
    assert_eq!(
        parsed_config.policy_path.as_deref(),
        Some(Path::new("/tmp/crab-cache-server/policy.yaml"))
    );
    assert!(server_config.contains("url = \"s3://enterprise-crab\""));
    assert!(server_config.contains(DEFAULT_PSK_BLAKE3));

    let policy = AuthPolicy::from_file(&output_dir.join("policy.yaml")).unwrap();
    assert!(policy.is_authorized("psk-client", "org/team-a/repo", "read"));
    assert!(policy.is_authorized("psk-client", "org/shared/repo", "dedup"));
    assert!(policy.is_authorized("psk-client", ".crab", "write"));
    assert!(policy.has_action("psk-client", "admin"));

    let client_config = fs::read_to_string(output_dir.join("client-config.toml")).unwrap();
    assert!(client_config.contains("service_mode = \"cache+dedup\""));
    assert!(client_config.contains("service_auth = \"psk\""));

    let client_env = fs::read_to_string(output_dir.join("client.env")).unwrap();
    assert!(client_env.contains("CRAB_CACHE_SERVICE_URL"));
    assert!(client_env.contains("crab-cache.enterprise.example"));
    assert!(!client_env.contains(DEFAULT_PSK_BLAKE3));

    let readme = fs::read_to_string(output_dir.join("README.md")).unwrap();
    assert!(readme.contains(
        "crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json"
    ));
    assert!(readme.contains("crab-cache-server onboarding probe --bundle-dir ."));
    assert!(readme.contains("onboarding-probe.json"));
    assert!(readme.contains("--client-probe --client-probe-repo org/example/repo"));
    assert!(readme.contains("onboarding-client-probe.json"));
    assert!(readme.contains("CRAB_CACHE_PSK=\"<secret-from-secret-manager>\""));
    assert!(readme.contains("crab doctor --cache-service-active-probe --json"));
    assert!(readme.contains("make cache-service-onboarding-rustfs-smoke"));
    assert!(readme.contains("make cache-service-release-gate"));
    assert!(readme.contains("--policy-path"));
    for code in STABLE_ONBOARDING_CHECK_CODES {
        assert!(readme.contains(code), "{code}");
    }
}

#[test]
fn onboarding_check_accepts_rendered_bundle() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");
    let render = run_onboarding_render(
        &output_dir,
        DEFAULT_PSK_BLAKE3,
        &[
            ("--origin-url", "s3://enterprise-crab"),
            (
                "--cache-service-url",
                "https://crab-cache.enterprise.example:8443",
            ),
            ("--repo-prefix", "org/team-a/*"),
        ],
    );
    assert_succeeded(&render);

    let output = run_onboarding_check(&output_dir);
    assert_succeeded(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "ok");
    assert_eq!(report["policy_diagnostics"]["rule_count"].as_u64(), Some(1));
    let actions = report["policy_diagnostics"]["actions"].as_array().unwrap();
    for action in ["read", "write", "dedup", "admin"] {
        assert!(
            actions
                .iter()
                .any(|candidate| candidate.as_str() == Some(action)),
            "missing policy action {action}: {actions:?}"
        );
    }
}

#[test]
fn onboarding_check_rejects_psk_hash_in_client_files() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");
    let render = run_onboarding_render(
        &output_dir,
        DEFAULT_PSK_BLAKE3,
        &[
            ("--origin-url", "s3://enterprise-crab"),
            (
                "--cache-service-url",
                "https://crab-cache.enterprise.example:8443",
            ),
            ("--repo-prefix", "org/team-a/*"),
        ],
    );
    assert_succeeded(&render);
    fs::write(
        output_dir.join("client.env"),
        format!("export CRAB_CACHE_PSK_HASH={DEFAULT_PSK_BLAKE3}\n"),
    )
    .unwrap();

    let output = run_onboarding_check(&output_dir);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "fail");
    assert_report_code(&report, "onboarding_secret_hash_leaked");
}

#[test]
fn onboarding_probe_runs_live_enterprise_preflight() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");
    let origin_root = fixture.path().join("origin");
    let cache_root = fixture.path().join("cache");
    fs::create_dir_all(&origin_root).unwrap();
    let origin_url = url::Url::from_directory_path(&origin_root)
        .unwrap()
        .to_string();
    let policy_path = output_dir.join("policy.yaml");

    let render = run_onboarding_render(
        &output_dir,
        DEFAULT_PSK_BLAKE3,
        &[
            ("--origin-url", &origin_url),
            ("--cache-service-url", "http://127.0.0.1:1"),
            ("--repo-prefix", "org/team-a/*"),
            ("--cache-root", cache_root.to_str().unwrap()),
            ("--listen-addr", "127.0.0.1:0"),
            ("--policy-path", policy_path.to_str().unwrap()),
        ],
    );
    assert_succeeded(&render);

    let output = run_onboarding_probe(&output_dir, true, false, None);
    assert_succeeded(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stdout.contains(DEFAULT_PSK_BLAKE3)
            && !stderr.contains(DEFAULT_PSK_BLAKE3)
            && !stdout.contains("psk-client")
            && !stderr.contains("psk-client"),
        "onboarding probe output must not leak secret material or principals\nstdout: {stdout}\nstderr: {stderr}"
    );

    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "warn");
    assert_eq!(report["bundle_check"]["status"], "ok");
    assert_eq!(report["server_preflight"]["status"], "warn");
    assert_eq!(
        report["server_preflight"]["summary"]["policy"],
        "configured"
    );
    assert_eq!(
        report["server_preflight"]["summary"]["mutable_path_mode"],
        "strict"
    );
    let codes = report["server_preflight"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|check| check["code"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(codes.contains("tls_not_configured"));
    assert!(codes.contains("auth_psk_plain_http"));
    assert!(!codes.contains("enterprise_trusted_boundary_required"));
    assert!(!codes.contains("enterprise_policy_required"));
}

#[test]
fn onboarding_probe_catches_missing_installed_policy_path() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");
    let origin_root = fixture.path().join("origin");
    let cache_root = fixture.path().join("cache");
    fs::create_dir_all(&origin_root).unwrap();
    let origin_url = url::Url::from_directory_path(&origin_root)
        .unwrap()
        .to_string();
    let missing_policy_path = fixture.path().join("installed").join("policy.yaml");

    let render = run_onboarding_render(
        &output_dir,
        DEFAULT_PSK_BLAKE3,
        &[
            ("--origin-url", &origin_url),
            ("--cache-service-url", "http://127.0.0.1:1"),
            ("--repo-prefix", "org/team-a/*"),
            ("--cache-root", cache_root.to_str().unwrap()),
            ("--listen-addr", "127.0.0.1:0"),
            ("--policy-path", missing_policy_path.to_str().unwrap()),
        ],
    );
    assert_succeeded(&render);

    let output = run_onboarding_probe(&output_dir, true, false, None);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "fail");
    assert_eq!(report["bundle_check"]["status"], "ok");
    assert_eq!(report["server_preflight"]["status"], "fail");
    let codes = report["server_preflight"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|check| check["code"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(codes.contains("startup_components_failed"));
}

#[test]
fn onboarding_render_rejects_invalid_psk_hash_before_writing() {
    let fixture = TempDir::new().unwrap();
    let output_dir = fixture.path().join("bundle");

    let output = run_onboarding_render(
        &output_dir,
        "not-a-blake3-hash",
        &[
            ("--origin-url", "s3://enterprise-crab"),
            (
                "--cache-service-url",
                "https://crab-cache.enterprise.example:8443",
            ),
            ("--repo-prefix", "org/team-a/*"),
        ],
    );
    assert_failed(&output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("psk-hash must be a 64-character Blake3 hex digest"),
        "{stderr}"
    );
    assert!(!output_dir.exists());
}

#[test]
fn release_workflow_requires_cache_service_smoke_evidence() {
    let body = workflow_body(".github/workflows/release.yml");

    for needle in [
        "cache_service_evidence_run_id",
        "CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID",
        "Cache Service Tests",
        "event\" != \"workflow_dispatch\"",
        "artifact=\"cache-service-rustfs-smoke-${run_id}-${attempt}\"",
        "expected_report_run_id=\"gha-${run_id}-${attempt}\"",
        "target/debug/crab-cache-server evidence gate",
        "--evidence-dir cache-service-release-evidence",
        "--expected-run-id \"$CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID\"",
        "--output cache-service-release-evidence-verify.json",
        "--summary-output cache-service-release-evidence-summary.json",
        "--doctor-output cache-service-release-evidence-doctor.json",
        "--doctor-text-output cache-service-release-evidence-doctor.txt",
        "gate_status=$?",
        "cache-service-release-evidence-doctor.json",
        "cache-service-release-evidence-doctor.txt",
        "cache-service-release-evidence-gate-${{ github.run_id }}-${{ github.run_attempt }}",
        "if-no-files-found: ignore",
        "cache-service-enterprise-gate",
        "needs: [prepare, workflow-release-gate, workflow-native-release-gate, replica-enterprise-gate, cache-service-enterprise-gate, nfs-native-evidence-gate, nfs-feature-gate]",
    ] {
        assert!(body.contains(needle), "{needle}");
    }

    assert!(
        !body.contains("artifact=\"cache-service-rustfs-smoke-${run_id}\""),
        "cache-service release evidence must be bound to the workflow run attempt"
    );
    assert!(
        !body.contains("mapfile -t reports"),
        "report discovery belongs in crab-cache-server, not release workflow shell"
    );
    assert!(
        !body.contains("target/debug/crab-cache-server evidence release-verify"),
        "release workflow should use the canonical evidence gate command"
    );
}

#[test]
fn evidence_release_verify_accepts_expected_run_id() {
    let fixture = EvidenceFixture::new();
    let output = run_evidence_release_verify(&fixture.report_path, EvidenceFixture::RUN_ID);
    assert_succeeded(&output);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "passed");
    assert_eq!(report["expected_run_id"], EvidenceFixture::RUN_ID);
    assert_eq!(report["run_id"], EvidenceFixture::RUN_ID);
    assert_eq!(report["verification"]["status"], "passed");
    assert_check_ok(&report, "release-run-id-matches", true);
}

#[test]
fn evidence_release_verify_accepts_evidence_dir_and_writes_outputs() {
    let fixture = EvidenceFixture::new();
    let evidence_dir = fixture.report_path.parent().unwrap();
    let verification_path = evidence_dir.join("release-verification.json");
    let summary_path = evidence_dir.join("release-summary.json");

    let output = run_evidence_release_verify_dir(
        evidence_dir,
        EvidenceFixture::RUN_ID,
        &verification_path,
        &summary_path,
    );
    assert_succeeded(&output);

    let verification = read_json(&verification_path);
    assert_eq!(verification["status"], "passed");
    assert_eq!(verification["expected_run_id"], EvidenceFixture::RUN_ID);
    assert_eq!(verification["verification"]["status"], "passed");

    let summary = read_json(&summary_path);
    assert_eq!(summary["status"], "passed");
    assert_eq!(summary["run_id"], EvidenceFixture::RUN_ID);
    assert_eq!(
        summary["dedup"]["cacheable_origin_gets_delta"].as_i64(),
        Some(0)
    );
}

#[test]
fn evidence_gate_accepts_evidence_dir_and_writes_release_artifacts() {
    let fixture = EvidenceFixture::new();
    let evidence_dir = fixture.report_path.parent().unwrap();
    let verification_path = evidence_dir.join("gate-verification.json");
    let summary_path = evidence_dir.join("gate-summary.json");
    let doctor_path = evidence_dir.join("gate-doctor.json");
    let doctor_text_path = evidence_dir.join("gate-doctor.txt");

    let output = run_evidence_gate_dir(
        evidence_dir,
        EvidenceFixture::RUN_ID,
        &verification_path,
        &summary_path,
        &doctor_path,
        &doctor_text_path,
    );
    assert_succeeded(&output);

    let verification = read_json(&verification_path);
    assert_eq!(verification["status"], "passed");
    assert_eq!(verification["expected_run_id"], EvidenceFixture::RUN_ID);

    let summary = read_json(&summary_path);
    assert_eq!(summary["status"], "passed");
    assert_eq!(
        summary["dedup"]["cacheable_origin_gets_delta"].as_i64(),
        Some(0)
    );
    assert!(!doctor_path.exists());
    assert!(!doctor_text_path.exists());
}

#[test]
fn evidence_gate_writes_doctor_outputs_when_release_binding_fails() {
    let fixture = EvidenceFixture::new();
    let evidence_dir = fixture.report_path.parent().unwrap();
    let verification_path = evidence_dir.join("gate-verification.json");
    let summary_path = evidence_dir.join("gate-summary.json");
    let doctor_path = evidence_dir.join("gate-doctor.json");
    let doctor_text_path = evidence_dir.join("gate-doctor.txt");

    let output = run_evidence_gate_dir(
        evidence_dir,
        "gha-wrong-1",
        &verification_path,
        &summary_path,
        &doctor_path,
        &doctor_text_path,
    );
    assert_failed(&output);

    let verification = read_json(&verification_path);
    assert_eq!(verification["status"], "failed");
    assert_check_ok(&verification, "release-run-id-matches", false);

    let summary = read_json(&summary_path);
    assert_eq!(summary["status"], "passed");

    let doctor = read_json(&doctor_path);
    assert_eq!(doctor["status"], "failed");
    assert_doctor_category(&doctor, "release_run_binding", "release-run-id-matches");

    let doctor_text = fs::read_to_string(&doctor_text_path).unwrap();
    assert!(doctor_text.contains("release_run_binding"));
    assert!(doctor_text.contains("release-run-id-matches"));
}

#[test]
fn evidence_release_verify_rejects_wrong_run_id() {
    let fixture = EvidenceFixture::new();
    let output = run_evidence_release_verify(&fixture.report_path, "gha-wrong-1");
    assert_failed(&output);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["expected_run_id"], "gha-wrong-1");
    assert_eq!(report["run_id"], EvidenceFixture::RUN_ID);
    assert_eq!(report["verification"]["status"], "passed");
    assert_check_ok(&report, "release-run-id-matches", false);
}

#[test]
fn evidence_doctor_accepts_passing_release_verification() {
    let fixture = EvidenceFixture::new();
    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");

    let release = run_evidence_release_verify_output(
        &fixture.report_path,
        EvidenceFixture::RUN_ID,
        &verification_path,
    );
    assert_succeeded(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_succeeded(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "passed");
    assert!(report["categories"].as_array().unwrap().is_empty());
}

#[test]
fn evidence_doctor_classifies_wrong_release_run_id() {
    let fixture = EvidenceFixture::new();
    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");

    let release =
        run_evidence_release_verify_output(&fixture.report_path, "gha-wrong-1", &verification_path);
    assert_failed(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_doctor_category(&report, "release_run_binding", "release-run-id-matches");
}

#[test]
fn evidence_doctor_classifies_dedup_origin_regression() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["cli_push_dedup"][0]["origin_get_key_delta"][".crab/xorbs/regression"] = Value::from(1);
    report["cli_push_dedup"][0]["origin_gets_delta"] = Value::from(2);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");
    let release = run_evidence_release_verify_output(
        &fixture.report_path,
        EvidenceFixture::RUN_ID,
        &verification_path,
    );
    assert_failed(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_doctor_category(
        &report,
        "cache_dedup_traffic",
        "cli-dedup-only-manifest-cas-origin-read",
    );
}

#[test]
fn evidence_doctor_classifies_retired_route_contract_regression() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["capabilities"][0]["mutable_route_patterns"]
        .as_array_mut()
        .unwrap()
        .push(Value::String("{repo}/xet/xorbs/{hash}".to_string()));
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");
    let release = run_evidence_release_verify_output(
        &fixture.report_path,
        EvidenceFixture::RUN_ID,
        &verification_path,
    );
    assert_failed(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_doctor_category(
        &report,
        "route_contract",
        "route-contract-no-retired-routes",
    );
    assert_doctor_detail(
        &report,
        "route_contract",
        "mutable routes: expected 10, actual 11",
    );
    assert_doctor_detail(
        &report,
        "route_contract",
        "unexpected mutable routes: {repo}/xet/xorbs/{hash}",
    );
    assert_doctor_detail(
        &report,
        "route_contract",
        "retired routes: {repo}/xet/xorbs/{hash}",
    );

    let text_output = run_evidence_doctor_text(&verification_path);
    assert_failed(&text_output);
    let text = String::from_utf8(text_output.stdout).unwrap();
    assert!(text.contains("detail: mutable routes: expected 10, actual 11"));
    assert!(text.contains("detail: retired routes: {repo}/xet/xorbs/{hash}"));
}

#[test]
fn evidence_doctor_classifies_origin_outage_origin_regression() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["origin_outages"][0]["hot_origin_gets_after_hot"] = Value::from(2);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");
    let release = run_evidence_release_verify_output(
        &fixture.report_path,
        EvidenceFixture::RUN_ID,
        &verification_path,
    );
    assert_failed(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_doctor_category(
        &report,
        "origin_outage_cache_resilience",
        "origin-outage-hot-origin-counters-flat",
    );
}

#[test]
fn evidence_doctor_classifies_origin_outage_support_bundle_regression() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    let outage_bundle = report["support_bundles"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|bundle| bundle["name"].as_str() == Some("origin-outage"))
        .unwrap();
    outage_bundle["health_ok"] = Value::from(true);
    outage_bundle["health_status"] = Value::from(200);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let verification_path = fixture
        .report_path
        .parent()
        .unwrap()
        .join("release-verification.json");
    let release = run_evidence_release_verify_output(
        &fixture.report_path,
        EvidenceFixture::RUN_ID,
        &verification_path,
    );
    assert_failed(&release);

    let output = run_evidence_doctor(&verification_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_doctor_category(
        &report,
        "origin_outage_cache_resilience",
        "origin-outage-support-bundle-health-degraded",
    );
}

#[test]
fn evidence_release_verify_rejects_ambiguous_evidence_dir() {
    let fixture = EvidenceFixture::new();
    let evidence_dir = fixture.report_path.parent().unwrap();
    let nested = evidence_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::copy(&fixture.report_path, nested.join("report.json")).unwrap();

    let output = Command::new(bin())
        .args([
            "evidence",
            "release-verify",
            "--evidence-dir",
            evidence_dir.to_str().unwrap(),
            "--expected-run-id",
            EvidenceFixture::RUN_ID,
        ])
        .output()
        .expect("crab-cache-server evidence release-verify should spawn");

    assert_failed(&output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("multiple report.json"),
        "stderr should explain ambiguous report discovery: {stderr}"
    );
}

#[test]
fn enterprise_check_json_rejects_weak_startup_posture() {
    let fixture = TempDir::new().unwrap();
    let origin_root = fixture.path().join("origin");
    let cache_root = fixture.path().join("cache");
    fs::create_dir_all(&origin_root).unwrap();

    let psk_hash = blake3::hash(b"enterprise-test-psk").to_hex().to_string();
    let origin_url = url::Url::from_directory_path(&origin_root)
        .unwrap()
        .to_string();
    let config_path = fixture.path().join("cache-server.toml");
    fs::write(
        &config_path,
        weak_enterprise_config(&origin_url, &cache_root, &psk_hash),
    )
    .unwrap();

    let output = run_enterprise_json_check(&config_path, false);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stdout.contains(&psk_hash) && !stderr.contains(&psk_hash),
        "preflight output must not leak auth.psk_hash\nstdout: {stdout}\nstderr: {stderr}"
    );

    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "fail");
    assert_eq!(report["summary"]["tls"], "plain_http");
    assert_eq!(report["summary"]["auth"], "psk");
    assert_eq!(report["summary"]["policy"], "not_configured");
    assert_eq!(report["summary"]["mutable_path_mode"], "transparent");

    let checks = report["checks"].as_array().unwrap();
    let codes = checks
        .iter()
        .filter_map(|check| check["code"].as_str())
        .collect::<BTreeSet<_>>();
    for expected in [
        "tls_not_configured",
        "auth_psk_plain_http",
        "policy_not_configured",
        "enterprise_trusted_boundary_required",
        "enterprise_policy_required",
        "enterprise_strict_mutable_paths_required",
    ] {
        assert!(
            codes.contains(expected),
            "missing preflight code {expected}; got {codes:?}"
        );
    }

    for check in checks
        .iter()
        .filter(|check| check["status"].as_str() != Some("ok"))
    {
        let remediation = check["remediation"].as_str().unwrap_or_default();
        assert!(
            !remediation.trim().is_empty(),
            "non-ok check must include remediation: {check:?}"
        );
    }
}

#[test]
fn enterprise_check_json_accepts_minimal_trusted_proxy_posture() {
    let fixture = TempDir::new().unwrap();
    let origin_root = fixture.path().join("origin");
    let cache_root = fixture.path().join("cache");
    let policy_path = fixture.path().join("policy.yaml");
    fs::create_dir_all(&origin_root).unwrap();
    fs::write(
        &policy_path,
        r#"
rules:
  - principal: "psk-client"
    repos: [".crab", "org/allowed/*"]
    actions: ["read", "write", "dedup", "admin"]
"#,
    )
    .unwrap();

    let psk_hash = blake3::hash(b"enterprise-test-psk").to_hex().to_string();
    let origin_url = url::Url::from_directory_path(&origin_root)
        .unwrap()
        .to_string();
    let config_path = fixture.path().join("cache-server.toml");
    fs::write(
        &config_path,
        trusted_proxy_enterprise_config(&origin_url, &cache_root, &policy_path, &psk_hash),
    )
    .unwrap();

    let output = run_enterprise_json_check(&config_path, true);
    assert_succeeded(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stdout.contains(&psk_hash)
            && !stderr.contains(&psk_hash)
            && !stdout.contains("psk-client")
            && !stderr.contains("psk-client"),
        "preflight output must not leak secret material or principals\nstdout: {stdout}\nstderr: {stderr}"
    );

    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "warn");
    assert_eq!(report["summary"]["tls"], "plain_http");
    assert_eq!(report["summary"]["auth"], "psk");
    assert_eq!(report["summary"]["policy"], "configured");
    assert_eq!(report["summary"]["mutable_path_mode"], "strict");
    assert_eq!(report["summary"]["policy_diagnostics"]["rule_count"], 1);

    let checks = report["checks"].as_array().unwrap();
    let codes = checks
        .iter()
        .filter_map(|check| check["code"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(codes.contains("tls_not_configured"));
    assert!(codes.contains("auth_psk_plain_http"));
    for rejected in [
        "policy_not_configured",
        "enterprise_trusted_boundary_required",
        "enterprise_policy_required",
        "enterprise_strict_mutable_paths_required",
    ] {
        assert!(
            !codes.contains(rejected),
            "minimal trusted-proxy posture should not report {rejected}; got {codes:?}"
        );
    }
}

fn run_enterprise_json_check(config_path: &Path, trusted_proxy_boundary: bool) -> Output {
    let mut args = vec![
        "--config",
        config_path.to_str().unwrap(),
        "check",
        "--json",
        "--profile",
        "enterprise",
    ];
    if trusted_proxy_boundary {
        args.push("--trusted-proxy-boundary");
    }

    Command::new(bin())
        .args(args)
        .output()
        .expect("crab-cache-server check should spawn")
}

fn assert_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "command should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failed(output: &Output) {
    assert!(
        !output.status.success(),
        "command should fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn trusted_proxy_enterprise_config(
    origin_url: &str,
    cache_root: &Path,
    policy_path: &Path,
    psk_hash: &str,
) -> String {
    format!(
        r#"
[server]
listen_addr = "127.0.0.1:0"
drain_timeout_secs = 1
mutable_path_mode = "strict"
policy_path = {}

[auth]
mechanism = "psk"
psk_hash = "{psk_hash}"

[origin]
url = {}

[cache]
root = {}
max_bytes = 1048576

[dedup]
scope = "all"

[eviction]
high_water_ratio = 0.95
low_water_ratio = 0.90
"#,
        toml_string(policy_path.to_str().unwrap()),
        toml_string(origin_url),
        toml_string(cache_root.to_str().unwrap()),
    )
}

fn weak_enterprise_config(origin_url: &str, cache_root: &Path, psk_hash: &str) -> String {
    format!(
        r#"
[server]
listen_addr = "127.0.0.1:0"
drain_timeout_secs = 1
mutable_path_mode = "transparent"

[auth]
mechanism = "psk"
psk_hash = "{psk_hash}"

[origin]
url = {}

[cache]
root = {}
max_bytes = 1048576

[dedup]
scope = "all"

[eviction]
high_water_ratio = 0.95
low_water_ratio = 0.90
"#,
        toml_string(origin_url),
        toml_string(cache_root.to_str().unwrap()),
    )
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn evidence_verify_accepts_manifest_bundle_without_config() {
    let fixture = EvidenceFixture::new();
    let output = run_evidence_verify(&fixture.report_path);
    assert_succeeded(&output);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "passed");
    assert_eq!(report["run_id"], EvidenceFixture::RUN_ID);
    assert!(report["verified_checks"].as_u64().unwrap() > 0);
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(
        &report,
        "artifact-cache_server_preflight_json-relative",
        true,
    );
    assert_check_ok(
        &report,
        "evidence-manifest-cache_server_preflight_json-path",
        true,
    );
    assert_check_ok(&report, "retained-cache_server_config-secret-free", true);
    assert_check_ok(&report, "cli-dedup-cacheable-origin-get-zero", true);
    assert_check_ok(&report, "cli-dedup-only-manifest-cas-origin-read", true);
}

#[test]
fn evidence_verify_and_summarize_accept_relocated_manifest_bundle() {
    let fixture = EvidenceFixture::new();
    let relocated = fixture.relocated_copy();

    let verify = run_evidence_verify(&relocated.report_path);
    assert_succeeded(&verify);
    let verify_stdout = String::from_utf8(verify.stdout).unwrap();
    let verification: Value = serde_json::from_str(&verify_stdout).unwrap();
    assert_eq!(verification["status"], "passed");

    let summarize = run_evidence_summarize(&relocated.report_path);
    assert_succeeded(&summarize);
    let summary_stdout = String::from_utf8(summarize.stdout).unwrap();
    let summary: Value = serde_json::from_str(&summary_stdout).unwrap();
    assert_eq!(summary["status"], "passed");
    assert_eq!(
        summary["dedup"]["cacheable_origin_gets_delta"].as_i64(),
        Some(0)
    );
}

#[test]
fn evidence_verify_rejects_report_tampering() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["tamper"] = Value::String("changed after manifest".to_string());
    write_json(&fixture.report_path, &report);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", false);
}

#[test]
fn evidence_verify_rejects_retained_config_secret_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    fs::write(
        &fixture.config_path,
        redacted_config().replace(
            "psk_hash = \"<redacted>\"",
            &format!("psk_hash = \"{DEFAULT_PSK_BLAKE3}\""),
        ),
    )
    .unwrap();
    fixture.refresh_manifest_record("cache_server_config", &fixture.config_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(
        &report,
        "evidence-manifest-cache_server_config-sha256",
        true,
    );
    assert_check_ok(&report, "retained-cache_server_config-secret-free", false);
}

#[test]
fn evidence_verify_rejects_extra_dedup_origin_read_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["cli_push_dedup"][0]["origin_get_key_delta"][".crab/xorbs/regression"] = Value::from(1);
    report["cli_push_dedup"][0]["origin_gets_delta"] = Value::from(2);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(&report, "cli-dedup-only-manifest-cas-origin-read", false);
}

#[test]
fn evidence_verify_rejects_retired_route_contract_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["capabilities"][0]["mutable_route_patterns"]
        .as_array_mut()
        .unwrap()
        .push(Value::String("{repo}/xet/xorbs/{hash}".to_string()));
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(&report, "route-contract-no-retired-routes", false);
}

#[test]
fn evidence_verify_rejects_missing_mutable_route_behavior_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["mutable_route_behaviors"]
        .as_array_mut()
        .unwrap()
        .pop();
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(&report, "route-contract-mutable-behavior-count", false);
}

#[test]
fn evidence_verify_uses_stable_mutable_route_check_id() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["mutable_route_behaviors"][0]["status"] = Value::from(200);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(
        &report,
        "route-contract-mutable-read-repo-refs-heads-status",
        false,
    );
}

#[test]
fn evidence_verify_rejects_missing_mutable_write_behavior_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["mutable_route_write_behaviors"]
        .as_array_mut()
        .unwrap()
        .pop();
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(
        &report,
        "route-contract-mutable-write-behavior-count",
        false,
    );
}

#[test]
fn evidence_verify_rejects_origin_outage_origin_read_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["origin_outages"][0]["hot_origin_gets_after_hot"] = Value::from(2);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(&report, "origin-outage-hot-origin-counters-flat", false);
}

#[test]
fn evidence_verify_rejects_origin_outage_support_bundle_even_when_hash_matches() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    let outage_bundle = report["support_bundles"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|bundle| bundle["name"].as_str() == Some("origin-outage"))
        .unwrap();
    outage_bundle["health_ok"] = Value::from(true);
    outage_bundle["health_status"] = Value::from(200);
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(&report, "evidence-manifest-report-sha256", true);
    assert_check_ok(
        &report,
        "origin-outage-support-bundle-health-degraded",
        false,
    );
}

#[test]
fn evidence_verify_rejects_absolute_report_artifact_path() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["artifacts"]["cache_server_preflight_json"] =
        Value::String(fixture.preflight_path.to_string_lossy().to_string());
    write_json(&fixture.report_path, &report);
    fixture.refresh_manifest_record("report", &fixture.report_path);

    let output = run_evidence_verify(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_check_ok(
        &report,
        "artifact-cache_server_preflight_json-relative",
        false,
    );
}

#[test]
fn evidence_summarize_reports_customer_proof_without_config() {
    let fixture = EvidenceFixture::new();
    let output = run_evidence_summarize(&fixture.report_path);
    assert_succeeded(&output);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let summary: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(summary["status"], "passed");
    assert_eq!(summary["run_id"], EvidenceFixture::RUN_ID);
    assert!(summary["verified_checks"].as_u64().unwrap() > 0);
    assert_eq!(
        summary["cache"]["origin_avoided_reads_total"].as_f64(),
        Some(9.0)
    );
    assert_eq!(summary["cache"]["origin_fetch_total"].as_f64(), Some(2.0));
    assert_eq!(summary["enterprise"]["policy"], "configured");
    assert_eq!(summary["enterprise"]["mutable_path_mode"], "strict");
    assert_eq!(summary["enterprise"]["policy_rule_count"].as_i64(), Some(1));
    assert_eq!(summary["routes"]["capabilities_status"].as_i64(), Some(200));
    assert_eq!(
        summary["routes"]["route_schema"].as_str(),
        Some("crab-cache-service.routes.v2")
    );
    assert_eq!(
        summary["routes"]["route_transport_prefix"].as_str(),
        Some("/v1/")
    );
    assert_eq!(
        summary["routes"]["expected_immutable_route_count"].as_u64(),
        Some(12)
    );
    assert_eq!(
        summary["routes"]["immutable_route_count"].as_u64(),
        Some(12)
    );
    assert_eq!(
        summary["routes"]["expected_mutable_route_count"].as_u64(),
        Some(10)
    );
    assert_eq!(summary["routes"]["mutable_route_count"].as_u64(), Some(10));
    assert_eq!(summary["routes"]["retired_route_count"].as_u64(), Some(0));
    assert_eq!(
        summary["routes"]["mutable_read_probe_count"].as_u64(),
        Some(10)
    );
    assert_eq!(
        summary["routes"]["mutable_read_probe_unique_patterns"].as_u64(),
        Some(10)
    );
    assert_eq!(
        summary["routes"]["mutable_write_probe_count"].as_u64(),
        Some(10)
    );
    assert_eq!(
        summary["routes"]["mutable_write_probe_unique_patterns"].as_u64(),
        Some(10)
    );

    let hydrates = summary["hydrates"].as_array().unwrap();
    assert_eq!(hydrates.len(), 2);
    assert!(
        hydrates
            .iter()
            .all(|hydrate| hydrate["origin_gets_delta"].as_i64() == Some(0))
    );
    assert!(
        hydrates
            .iter()
            .all(|hydrate| hydrate["origin_fetches_delta"].as_i64() == Some(0))
    );
    assert!(
        hydrates
            .iter()
            .all(|hydrate| hydrate["cache_hits_delta"].as_i64() == Some(9))
    );
    assert_eq!(
        summary["dedup"]["cacheable_origin_gets_delta"].as_i64(),
        Some(0)
    );
    assert_eq!(summary["dedup"]["xorb_puts_delta"].as_i64(), Some(0));

    let artifacts = summary["artifacts"].as_array().unwrap();
    let report_artifact = artifacts
        .iter()
        .find(|artifact| artifact["name"].as_str() == Some("report"))
        .unwrap();
    assert_eq!(report_artifact["sha256"].as_str().unwrap().len(), 64);
    assert!(report_artifact["bytes"].as_u64().unwrap() > 0);
}

#[test]
fn evidence_summarize_rejects_report_tampering() {
    let fixture = EvidenceFixture::new();
    let mut report = read_json(&fixture.report_path);
    report["tamper"] = Value::String("changed after manifest".to_string());
    write_json(&fixture.report_path, &report);

    let output = run_evidence_summarize(&fixture.report_path);
    assert_failed(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let summary: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(summary["status"], "failed");
    let failed_checks = summary["failed_checks"].as_array().unwrap();
    assert!(failed_checks.iter().any(|check| {
        check
            .as_str()
            .is_some_and(|name| name == "evidence-manifest-report-sha256")
    }));
}

fn run_evidence_verify(report_path: &Path) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "verify",
            "--report",
            report_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("crab-cache-server evidence verify should spawn")
}

fn run_evidence_summarize(report_path: &Path) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "summarize",
            "--report",
            report_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("crab-cache-server evidence summarize should spawn")
}

fn run_evidence_release_verify(report_path: &Path, expected_run_id: &str) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "release-verify",
            "--report",
            report_path.to_str().unwrap(),
            "--expected-run-id",
            expected_run_id,
            "--json",
        ])
        .output()
        .expect("crab-cache-server evidence release-verify should spawn")
}

fn run_evidence_release_verify_dir(
    evidence_dir: &Path,
    expected_run_id: &str,
    output_path: &Path,
    summary_path: &Path,
) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "release-verify",
            "--evidence-dir",
            evidence_dir.to_str().unwrap(),
            "--expected-run-id",
            expected_run_id,
            "--output",
            output_path.to_str().unwrap(),
            "--summary-output",
            summary_path.to_str().unwrap(),
        ])
        .output()
        .expect("crab-cache-server evidence release-verify should spawn")
}

fn run_evidence_gate_dir(
    evidence_dir: &Path,
    expected_run_id: &str,
    output_path: &Path,
    summary_path: &Path,
    doctor_path: &Path,
    doctor_text_path: &Path,
) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "gate",
            "--evidence-dir",
            evidence_dir.to_str().unwrap(),
            "--expected-run-id",
            expected_run_id,
            "--output",
            output_path.to_str().unwrap(),
            "--summary-output",
            summary_path.to_str().unwrap(),
            "--doctor-output",
            doctor_path.to_str().unwrap(),
            "--doctor-text-output",
            doctor_text_path.to_str().unwrap(),
        ])
        .output()
        .expect("crab-cache-server evidence gate should spawn")
}

fn run_evidence_release_verify_output(
    report_path: &Path,
    expected_run_id: &str,
    output_path: &Path,
) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "release-verify",
            "--report",
            report_path.to_str().unwrap(),
            "--expected-run-id",
            expected_run_id,
            "--output",
            output_path.to_str().unwrap(),
        ])
        .output()
        .expect("crab-cache-server evidence release-verify should spawn")
}

fn run_evidence_doctor(verification_path: &Path) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "doctor",
            "--verification",
            verification_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("crab-cache-server evidence doctor should spawn")
}

fn run_evidence_doctor_text(verification_path: &Path) -> Output {
    Command::new(bin())
        .args([
            "evidence",
            "doctor",
            "--verification",
            verification_path.to_str().unwrap(),
        ])
        .output()
        .expect("crab-cache-server evidence doctor should spawn")
}

fn run_onboarding_render(output_dir: &Path, psk_hash: &str, extra_args: &[(&str, &str)]) -> Output {
    let mut args = vec![
        "onboarding".to_string(),
        "render".to_string(),
        "--output-dir".to_string(),
        output_dir.to_str().unwrap().to_string(),
        "--psk-hash".to_string(),
        psk_hash.to_string(),
    ];
    for (key, value) in extra_args {
        args.push((*key).to_string());
        args.push((*value).to_string());
    }

    Command::new(bin())
        .args(args)
        .output()
        .expect("crab-cache-server onboarding render should spawn")
}

fn run_onboarding_check(bundle_dir: &Path) -> Output {
    Command::new(bin())
        .args([
            "onboarding",
            "check",
            "--bundle-dir",
            bundle_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("crab-cache-server onboarding check should spawn")
}

fn run_onboarding_probe(
    bundle_dir: &Path,
    trusted_proxy_boundary: bool,
    fail_on_warn: bool,
    client_probe_repo: Option<&str>,
) -> Output {
    let mut args = vec![
        "onboarding".to_string(),
        "probe".to_string(),
        "--bundle-dir".to_string(),
        bundle_dir.to_str().unwrap().to_string(),
        "--json".to_string(),
    ];
    if trusted_proxy_boundary {
        args.push("--trusted-proxy-boundary".to_string());
    }
    if fail_on_warn {
        args.push("--fail-on-warn".to_string());
    }
    if let Some(repo) = client_probe_repo {
        args.push("--client-probe".to_string());
        args.push("--client-probe-repo".to_string());
        args.push(repo.to_string());
    }

    Command::new(bin())
        .args(args)
        .output()
        .expect("crab-cache-server onboarding probe should spawn")
}

fn assert_check_ok(report: &Value, name: &str, ok: bool) {
    let checks = report["checks"].as_array().unwrap();
    let check = checks
        .iter()
        .find(|check| check["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("missing check {name}"));
    assert_eq!(check["ok"].as_bool(), Some(ok), "check {name}: {check:?}");
}

fn assert_report_code(report: &Value, code: &str) {
    let checks = report["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|check| check["code"].as_str() == Some(code)),
        "missing report code {code}: {checks:?}"
    );
}

fn assert_doctor_category(report: &Value, category: &str, check_name: &str) {
    let categories = report["categories"].as_array().unwrap();
    let found = categories
        .iter()
        .find(|entry| entry["category"].as_str() == Some(category))
        .unwrap_or_else(|| panic!("missing doctor category {category}: {categories:?}"));
    let checks = found["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|check| check.as_str() == Some(check_name)),
        "doctor category {category} should include {check_name}: {found:?}"
    );
    assert!(
        found["remediation"]
            .as_str()
            .is_some_and(|remediation| !remediation.trim().is_empty()),
        "doctor category {category} should include remediation: {found:?}"
    );
}

fn assert_doctor_detail(report: &Value, category: &str, expected_detail: &str) {
    let categories = report["categories"].as_array().unwrap();
    let found = categories
        .iter()
        .find(|entry| entry["category"].as_str() == Some(category))
        .unwrap_or_else(|| panic!("missing doctor category {category}: {categories:?}"));
    let details = found["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|detail| detail.as_str() == Some(expected_detail)),
        "doctor category {category} should include detail {expected_detail}: {found:?}"
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap() + "\n").unwrap();
}

fn file_evidence(report_dir: &Path, path: &Path) -> Value {
    let path = fs::canonicalize(path).unwrap();
    serde_json::json!({
        "path": artifact_ref(report_dir, &path),
        "sha256": sha256_file(&path),
        "bytes": path.metadata().unwrap().len(),
    })
}

fn artifact_ref(report_dir: &Path, path: &Path) -> String {
    let report_dir = fs::canonicalize(report_dir).unwrap();
    path.strip_prefix(report_dir)
        .unwrap()
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

struct EvidenceFixture {
    _fixture: TempDir,
    report_path: PathBuf,
    manifest_path: PathBuf,
    preflight_path: PathBuf,
    config_path: PathBuf,
}

impl EvidenceFixture {
    const RUN_ID: &'static str = "fixture-run";

    fn new() -> Self {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path();
        let report_path = root.join("report.json");
        let manifest_path = root.join("cache-service-evidence-manifest.json");
        let preflight_path = root.join("cache-server-preflight.json");
        let config_path = root.join("cache-server.toml");
        let transparent_config_path = root.join("transparent-cache-server.toml");
        let policy_path = root.join("policy.yaml");
        let smoke_script_path = root.join("rustfs-smoke-script.py");
        let verifier_script_path = root.join("smoke-report-verifier.py");

        fs::write(&preflight_path, preflight_json()).unwrap();
        fs::write(&config_path, redacted_config()).unwrap();
        fs::write(&transparent_config_path, redacted_config()).unwrap();
        fs::write(&policy_path, redacted_policy()).unwrap();
        fs::write(&smoke_script_path, b"print('smoke')\n").unwrap();
        fs::write(&verifier_script_path, b"print('verify')\n").unwrap();

        let manifest_key = format!("e2e-cache-service/{}/cli-dedup/manifest", Self::RUN_ID);
        let mut manifest_delta = serde_json::Map::new();
        manifest_delta.insert(manifest_key, Value::from(1));

        let report = serde_json::json!({
            "status": "passed",
            "run_id": Self::RUN_ID,
            "bucket": "crab",
            "artifacts": {
                "report": "report.json",
                "cache_service_evidence_manifest": "cache-service-evidence-manifest.json",
                "cache_server_preflight_json": "cache-server-preflight.json",
                "cache_server_config": "cache-server.toml",
                "transparent_cache_server_config": "transparent-cache-server.toml",
                "cache_server_policy": "policy.yaml",
                "rustfs_smoke_script": "rustfs-smoke-script.py",
                "smoke_report_verifier": "smoke-report-verifier.py",
            },
            "checks": [
                {"name": "cache-server-preflight-no-failures", "ok": true},
            ],
            "capabilities": [
                {
                    "name": "cache-service-capabilities",
                    "status": 200,
                    "schema": "crab-cache-service.capabilities.v1",
                    "route_schema": "crab-cache-service.routes.v2",
                    "route_transport_prefix": "/v1/",
                    "immutable_route_patterns": immutable_route_patterns(),
                    "mutable_route_patterns": mutable_route_patterns(),
                    "max_cache_bytes": 16777216,
                    "admin_max_cache_bytes": 16777216,
                    "max_object_bytes": 268435456,
                    "admin_max_object_bytes": 268435456
                }
            ],
            "mutable_route_behaviors": mutable_route_behavior_records(),
            "mutable_route_write_behaviors": mutable_route_write_behavior_records(),
            "cli_hydrates": [
                hydrate_record("cli-cold-hydrate"),
                hydrate_record("cli-warm-hydrate"),
            ],
            "cli_push_dedup": [
                {
                    "name": "cli-dedup-push",
                    "dedup_queries_delta": 1,
                    "dedup_known_chunks_delta": 3,
                    "xorb_puts_delta": 0,
                    "xorb_gets_delta": 0,
                    "shard_gets_delta": 0,
                    "metadata_gets_delta": 0,
                    "cacheable_origin_gets_delta": 0,
                    "cacheable_origin_get_key_delta": {},
                    "origin_get_key_delta": manifest_delta.clone(),
                    "origin_gets_delta": 1,
                    "mutable_origin_get_key_delta": manifest_delta,
                    "mutable_origin_gets_delta": 1,
                    "mutable_read_rejections_delta": 0,
                    "mutable_write_rejections_delta": 0
                }
            ],
            "support_bundles": [
                {
                    "name": "post-traffic",
                    "schema": "cache-service.support-bundle",
                    "health_ok": true,
                    "health_status": 200,
                    "auth_ok": true,
                    "auth_status": 200,
                    "auth_endpoint": "/v1/capabilities",
                    "capabilities_ok": true,
                    "capabilities_status": 200,
                    "authz_ok": true,
                    "authz_status": 200,
                    "admin_stats_ok": true,
                    "admin_stats_status": 200,
                    "metrics_ok": true,
                    "metrics_status": 200,
                    "cache_hit_rate": 0.5,
                    "origin_avoided_reads_total": 9.0,
                    "origin_fetch_total": 2.0
                },
                {
                    "name": "origin-outage",
                    "schema": "cache-service.support-bundle",
                    "health_ok": false,
                    "health_status": 503,
                    "auth_ok": true,
                    "auth_status": 200,
                    "auth_endpoint": "/v1/capabilities",
                    "capabilities_ok": true,
                    "capabilities_status": 200,
                    "authz_ok": true,
                    "authz_status": 200,
                    "admin_stats_ok": true,
                    "admin_stats_status": 200,
                    "metrics_ok": true,
                    "metrics_status": 200,
                    "cache_hit_rate": 0.5,
                    "origin_avoided_reads_total": 9.0,
                    "origin_fetch_total": 2.0
                }
            ],
            "origin_outages": [
                {
                    "name": "origin-outage-cached-read-through",
                    "health_status": 503,
                    "live_status": 200,
                    "warm_status": 200,
                    "warm_cache_status": "MISS",
                    "hot_status": 200,
                    "hot_cache_status": "HIT",
                    "range_status": 206,
                    "range_cache_status": "HIT",
                    "cold_status": 504,
                    "cold_cache_status": "",
                    "hot_origin_gets_before_outage": 1,
                    "hot_origin_gets_after_hot": 1,
                    "hot_origin_gets_after_range": 1,
                    "cold_origin_gets_before_outage": 0,
                    "cold_origin_gets_after_cold": 0,
                    "total_origin_gets_before_outage": 10,
                    "total_origin_gets_after_hot": 10,
                    "total_origin_gets_after_range": 10,
                    "total_origin_gets_after_cold": 10,
                    "cache_hits_before_outage": 8,
                    "cache_hits_after_outage": 10,
                    "origin_fetches_before_outage": 4,
                    "origin_fetches_after_outage": 4,
                    "hot_body_len": 4096,
                    "range_body_len": 24,
                    "cold_body_len": 18
                }
            ]
        });
        write_json(&report_path, &report);

        let manifest = serde_json::json!({
            "schema": "crab-cache-service.evidence-manifest.v1",
            "run_id": Self::RUN_ID,
            "artifacts": {
                "report": file_evidence(root, &report_path),
                "cache_server_preflight_json": file_evidence(root, &preflight_path),
                "cache_server_config": file_evidence(root, &config_path),
                "transparent_cache_server_config": file_evidence(root, &transparent_config_path),
                "cache_server_policy": file_evidence(root, &policy_path),
                "rustfs_smoke_script": file_evidence(root, &smoke_script_path),
                "smoke_report_verifier": file_evidence(root, &verifier_script_path),
            },
            "runtime": {
                "crab_version": "crab 1.0.1",
                "cache_server_version": "crab-cache-server 1.0.1",
                "rustfs_bucket": "crab"
            },
            "parameters": {
                "dedup_scope": "all",
                "mutable_path_mode": "strict"
            }
        });
        write_json(&manifest_path, &manifest);

        Self {
            _fixture: fixture,
            report_path,
            manifest_path,
            preflight_path,
            config_path,
        }
    }

    fn refresh_manifest_record(&self, key: &str, path: &Path) {
        let mut manifest = read_json(&self.manifest_path);
        manifest["artifacts"][key] = file_evidence(self.report_path.parent().unwrap(), path);
        write_json(&self.manifest_path, &manifest);
    }

    fn relocated_copy(&self) -> RelocatedEvidenceFixture {
        let fixture = TempDir::new().unwrap();
        let target = fixture.path().join("evidence");
        copy_dir(self.report_path.parent().unwrap(), &target);
        RelocatedEvidenceFixture {
            _fixture: fixture,
            report_path: target.join("report.json"),
        }
    }
}

struct RelocatedEvidenceFixture {
    _fixture: TempDir,
    report_path: PathBuf,
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir(&source, &target);
        } else {
            fs::copy(&source, &target).unwrap();
        }
    }
}

fn immutable_route_patterns() -> Vec<&'static str> {
    vec![
        ".crab/xorbs/{first-two-hex}/{hash}",
        ".crab/shards/{first-two-hex}/{hash}",
        "{repo}/packs/pack-{id}.pack",
        "{repo}/packs/pack-{id}.idx",
        "{repo}/file_index_db/compacted/*.sst",
        "{repo}/file_index_db/manifest/*.manifest",
        "{repo}/file_index_db/wal/*.sst",
        "{repo}/file_index_db/compactions/*.compactions",
        ".crab/chunk_index_db/compacted/*.sst",
        ".crab/chunk_index_db/manifest/*.manifest",
        ".crab/chunk_index_db/wal/*.sst",
        ".crab/chunk_index_db/compactions/*.compactions",
    ]
}

fn mutable_route_patterns() -> Vec<&'static str> {
    vec![
        "{repo}/refs/heads/*",
        "{repo}/HEAD",
        "{repo}/locks/*",
        "{repo}/packs/pack-{id}.meta",
        "{repo}/manifests/*",
        "{repo}/pack-list",
        "{repo}/shard-list",
        ".crab/ref-registry",
        "{repo}/file_index_db/manifest/current",
        ".crab/chunk_index_db/manifest/current",
    ]
}

fn mutable_route_behavior_records() -> Vec<Value> {
    mutable_route_patterns()
        .into_iter()
        .enumerate()
        .map(|(idx, pattern)| {
            serde_json::json!({
                "name": format!("route-contract-mutable-{idx}"),
                "pattern": pattern,
                "key": format!("route-contract/mutable/{idx}"),
                "status": 400,
                "cache_status": "",
                "origin_gets_before": 0,
                "origin_gets_after": 0,
                "body_len": 19
            })
        })
        .collect()
}

fn mutable_route_write_behavior_records() -> Vec<Value> {
    mutable_route_patterns()
        .into_iter()
        .enumerate()
        .map(|(idx, pattern)| {
            serde_json::json!({
                "name": format!("route-contract-mutable-write-{idx}"),
                "pattern": pattern,
                "key": format!("route-contract/mutable/{idx}"),
                "status": 400,
                "cache_status": "",
                "origin_gets_before": 0,
                "origin_gets_after": 0,
                "origin_puts_before": 0,
                "origin_puts_after": 0,
                "total_origin_gets_before": 10,
                "total_origin_gets_after": 10,
                "total_origin_puts_before": 4,
                "total_origin_puts_after": 4,
                "total_bytes_before": 4096,
                "total_bytes_after": 4096,
                "push_warming_writes_before": 1,
                "push_warming_writes_after": 1,
                "push_warming_bytes_before": 1024,
                "push_warming_bytes_after": 1024,
                "request_body_len": 257,
                "response_body_len": 36
            })
        })
        .collect()
}

fn hydrate_record(name: &str) -> Value {
    serde_json::json!({
        "name": name,
        "origin_gets_before": 4,
        "origin_gets_after": 4,
        "origin_get_key_delta": {},
        "cache_hits_delta": 9,
        "origin_fetches_delta": 0,
        "origin_avoided_reads_delta": 9,
        "mutable_read_rejections_delta": 0,
        "mutable_write_rejections_delta": 0
    })
}

fn preflight_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "status": "warn",
        "summary": {
            "policy": "configured",
            "mutable_path_mode": "strict",
            "max_object_bytes": 1048576,
            "policy_diagnostics": {
                "rule_count": 1,
                "repo_pattern_count": 2,
                "actions": ["read", "write", "dedup", "admin"]
            }
        },
        "checks": [
            {"name": "enterprise profile", "status": "ok"},
            {"name": "tls", "status": "warn", "code": "tls_not_configured"}
        ]
    }))
    .unwrap()
        + "\n"
}

fn redacted_config() -> String {
    r#"[server]
listen_addr = "127.0.0.1:0"
mutable_path_mode = "strict"
policy_path = "policy.yaml"
drain_timeout_secs = 1

[auth]
mechanism = "psk"
psk_hash = "<redacted>"

[origin]
url = "s3://crab"

[cache]
root = "/tmp/cache"
max_bytes = 16777216

[dedup]
scope = "all"
"#
    .to_string()
}

fn redacted_policy() -> String {
    r#"rules:
  - principal: "<redacted>"
    repos: ["<run-scope>", ".crab"]
    actions: ["read", "write", "dedup", "admin"]
"#
    .to_string()
}
