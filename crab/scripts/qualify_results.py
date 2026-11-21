"""Run native Results acceptance against an owned S3-compatible fixture."""
from pathlib import Path
import hashlib
import importlib.util
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / "crab/scripts" / file)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


fixture = module("sdk_results_fixture", "qualify_sdk_native_rustfs.py")
verifier = module("results_evidence", "verify_results_evidence.py")


def sha(file):
    with file.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def source_tree():
    digest = hashlib.sha256()
    files = git("ls-files", "-z", "--cached", "--others", "--exclude-standard", "--",
                "Cargo.toml", "Cargo.lock", ".cargo",
                "crab", "crates", "packages/desktop", "packages/ui", "packages/workflow-results",
                "packages/web/public/examples/ml-results-lab",
                ".github/workflows/desktop.yml", ".github/workflows/workflow-native.yml",
                ".github/workflows/sdk.yml").split(b"\0")
    for name in sorted(set(files) - {b""}):
        file = ROOT / os.fsdecode(name)
        if file.suffix in {".md", ".mdx", ".pyc"}:
            continue
        digest.update(name + b"\0")
        digest.update((sha(file) if file.is_file() else "missing").encode())
    return digest.hexdigest()


def run_test(env, output, log_name, arguments, test_name):
    log = output / log_name
    with log.open("w") as stream:
        subprocess.run(arguments, cwd=ROOT, env=env, stdout=stream, stderr=stream, check=True)
    if not re.search(r"test " + re.escape(test_name) + r" \.\.\. ok", log.read_text()):
        raise RuntimeError(f"No successful relevant test in {log_name}")


def desktop_proof(proof):
    desktop = ROOT / "packages/desktop"
    release = desktop / "release"
    system = platform.system()
    native_app = {
        "Darwin": "mac-arm64/Crab.app/Contents/MacOS/Crab" if platform.machine() == "arm64" else "mac/Crab.app/Contents/MacOS/Crab",
        "Linux": "linux-unpacked/crab",
        "Windows": "win-unpacked/Crab.exe",
    }[system]
    app = Path(os.environ["CRAB_PACKAGED_APP"]) if os.environ.get("CRAB_PACKAGED_APP") else release / native_app
    if not app.is_file():
        raise RuntimeError("Build the unpacked desktop application before qualification")
    resources = app.parent.parent / "Resources" if system == "Darwin" else app.parent / "resources"
    suffix = ".exe" if system == "Windows" else ""
    proof["binaries"].update(packaged_cli=sha(resources / "agent" / ("crab" + suffix)),
                              agent=sha(resources / "agent" / ("crab-desktop-agent" + suffix)),
                              application=sha(resources / "app.asar"))


def specs(suite):
    for spec in suite.get("specs", []):
        yield spec
    for child in suite.get("suites", []):
        yield from specs(child)


def main():
    target = Path(os.environ["CARGO_TARGET_DIR"])
    if not target.is_absolute() or not target.is_dir():
        raise RuntimeError("Use this checkout's external Cargo target directory")
    target = target.resolve()
    cli = target / "debug" / ("crab.exe" if platform.system() == "Windows" else "crab")
    output = target / "results-acceptance"
    output.mkdir(exist_ok=True)
    desktop = "--desktop" in sys.argv
    http = "--http" in sys.argv
    browser = "--browser" in sys.argv
    source = git("rev-parse", "HEAD").decode().strip()
    tree = source_tree()
    runtime = sha(ROOT / "crates/crab-workflow/assets/results-runtime.js")
    binding = {"source_sha": source, "source_tree": tree, "runtime_sha256": runtime,
               "platform": f"{platform.system()}-{platform.machine()}",
               "workflow_run": os.environ.get("GITHUB_RUN_ID", "local"),
               "workflow_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "1")}
    proof = {"schema": 1, "passed": False, **binding, "provider": "native-rustfs-s3",
             "checks": {}, "tests": {}, "binaries": {}, "resources": {}}
    # Invalidate an earlier successful artifact before fixture setup can fail.
    (output / "evidence.json").write_text(json.dumps(proof, indent=2, sort_keys=True) + "\n")
    for name in ("native-resource-envelope.json", "http-resource-envelope.json"):
        (output / name).unlink(missing_ok=True)
    executable, asset, digest = fixture.install_rustfs(output / "tools")
    proof["storage_fixture"] = {"asset": asset, "sha256": digest}
    env = None
    try:
        # Windows needs the shared short object-key layout. Unix compiler
        # wrapper sockets retain the shorter Results prefix for macOS SUN_LEN.
        parent, prefix, data_name = fixture.rustfs_work_location(output) if os.name == "nt" else (target, "r-", "data")
        with tempfile.TemporaryDirectory(prefix=prefix, dir=parent) as directory:
            work = Path(directory)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            env = fixture.fixture_environment(work, output, f"http://127.0.0.1:{port}", "results",
                                              Path(shutil.which("git")), cli, target)
            env["CRAB_RESULTS_EVIDENCE"] = str(output)
            data = work / data_name
            data.mkdir()
            server_env = {**env, "RUSTFS_ACCESS_KEY": env["AWS_ACCESS_KEY_ID"],
                          "RUSTFS_SECRET_KEY": env["AWS_SECRET_ACCESS_KEY"], "RUSTFS_CONSOLE_ENABLE": "false"}
            with (output / "storage.log").open("w") as log:
                server = subprocess.Popen([str(executable), "server", "--address", f"127.0.0.1:{port}", str(data)],
                                          env=server_env, cwd=work, stdout=log, stderr=log)
                try:
                    deadline = time.monotonic() + 120
                    while True:
                        if server.poll() is not None:
                            raise RuntimeError("Results storage fixture exited")
                        try:
                            fixture.aws(env, "s3api", "list-buckets")
                            break
                        except RuntimeError as error:
                            if time.monotonic() >= deadline:
                                raise RuntimeError(f"Results storage readiness timed out: {error}") from None
                            time.sleep(0.5)
                    fixture.aws(env, "s3api", "create-bucket", "--bucket", "results")
                    run_test(env, output, "journey.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        "--", "--ignored", "--nocapture"],
                        "queued_runs_retain_real_results_and_reports_survive_cold_remote_retrieval")
                    deferred = {"results_read_admission", "schema_discovery", "partial_chart_evidence", "checkpoint_resume", "scale_10000", "captured_inputs_before_mutation",
                                "produced_inputs_regenerated", "admitted_input_identities", "materialized_inputs_reproduced", "directory_inputs_reproduced", "managed_results_transfer_refusal", "empty_private_settings_reproduction", "chart_response_bounds", "dvc_template_parity", "large_matrix_exact_counts", "cli_matrix_exports"}
                    workspace_cases = [
                        ("saved::template_refs_cannot_publish_private_repository_state", "private_template_boundary"),
                        ("saved::saved_templates_retain_declared_assets_without_private_crab_state", "saved_template_assets"),
                        ("saved::uncaptured_custom_templates_do_not_select_same_name_builtins", "custom_template_capture_failure"),
                        ("saved::execution_and_reproduction_keep_templates_from_the_recorded_tree", "recorded_template_reproduction"),
                        ("saved::saved_experiments_retain_observations_without_claiming_execution", "saved_observations"),
                        ("saved::result_workspace_actions_refuse_changed_outputs_before_touching_the_workspace", "workspace_action_integrity"),
                        ("saved::result_apply_preserves_files_outside_workspace_directory_symlinks", "workspace_target_boundaries"),
                        ("saved::result_apply_restores_directory_outputs_with_their_captured_modes", "directory_output_restore"),
                    ]
                    migration_cases = [
                        ("migration::metadata_migration_preserves_recorded_values_without_claiming_execution", "metadata_migration"),
                        ("migration::metadata_migration_rejects_invalid_sources_before_installing_roots", "migration_source_validation"),
                        ("migration::metadata_migration_preserves_existing_captures_and_refuses_corrupt_roots", "migration_preserves_capture_roots"),
                    ]
                    deferred.update(capability for _, capability in workspace_cases + migration_cases)
                    deferred.update({"checkpoint_target_boundaries", "streaming_restore", "authored_plot_capture", "partial_template_evidence"})
                    proof["checks"].update(dict.fromkeys(verifier.CORE - deferred, True))
                    proof["tests"]["core"] = 1
                    proof["binaries"]["cli"] = sha(cli)
                    for group, cases in [("workspace", workspace_cases), ("migration", migration_cases)]:
                        for case, capability in cases:
                            run_test(env, output, f"{capability}.log", ["cargo", "test", "-p", "crab", "--locked",
                                "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                                case, "--", "--exact"], case)
                            proof["checks"][capability] = True
                        proof["tests"][group] = len(cases)
                    for package, case, capability in [
                        ("crab", "cmd::exp::workspace_action::tests::checkpoint_restore_rejects_symlink_parents_without_changing_external_files", "checkpoint_target_boundaries"),
                        ("crab-workflow", "materialize::tests::streaming_read_failure_preserves_target_and_discards_partial_sidecar", "streaming_restore"),
                    ]:
                        run_test(env, output, f"{capability}.log", ["cargo", "test", "-p", package, "--locked",
                            "--no-default-features", "--lib", case, "--", "--exact"], case)
                        proof["checks"][capability] = True
                    proof["tests"]["restore"] = 2
                    settings_case = "empty_private_settings_allow_reproduction_without_retaining_private_values"
                    run_test(env, output, "private-settings.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        settings_case, "--", "--exact"], settings_case)
                    proof["tests"]["settings"] = 1
                    proof["checks"]["empty_private_settings_reproduction"] = True
                    for log_name, test_name, capability in [
                        ("failure.log", "failed_immediate_and_queued_runs_retain_partial_evidence_after_cleanup", "failed_capture"),
                        ("cancellation.log", "interrupted_queue_run_retains_partial_evidence_and_preserves_completed_runs", "interrupted_capture"),
                    ]:
                        run_test(env, output, log_name, ["cargo", "test", "-p", "crab", "--locked",
                            "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                            test_name, "--", "--exact"], test_name)
                        proof["checks"][capability] = True
                    proof["tests"]["faults"] = 2
                    input_cases = [
                        ("input-capture.log", "reproduction_retains_overridden_params_and_copied_inputs_before_commands_mutate_them", "captured_inputs_before_mutation"),
                        ("produced-inputs.log", "reproduction_regenerates_selected_producer_outputs_from_retained_source_inputs", "produced_inputs_regenerated"),
                        ("external-inputs.log", "admission_keeps_external_digest_when_command_mutates_the_source", "admitted_input_identities"),
                        ("materialized-inputs.log", "reproduction_retains_materialized_output_inputs_across_generations", "materialized_inputs_reproduced"),
                        ("directory-inputs.log", "reproduction::directory_replay_preserves_deleted_children_and_empty_directories", "directory_inputs_reproduced"),
                    ]
                    for log_name, input_case, capability in input_cases:
                        run_test(env, output, log_name, ["cargo", "test", "-p", "crab", "--locked",
                            "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                            input_case, "--", "--exact"], input_case)
                        proof["checks"][capability] = True
                    proof["tests"]["inputs"] = len(input_cases)
                    transport_case = "transport::managed_results_transfers_refuse_before_direct_credentials"
                    run_test(env, output, "managed-transport.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        transport_case, "--", "--exact"], transport_case)
                    proof["checks"]["managed_results_transfer_refusal"] = True
                    proof["tests"]["transport"] = 1
                    discovery_case = "transport::results_schema_is_advertised_by_version"
                    run_test(env, output, "schema-discovery.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        discovery_case, "--", "--exact"], discovery_case)
                    proof["checks"]["schema_discovery"] = True
                    proof["tests"]["discovery"] = 1
                    transfer_case = "results::tests::interrupted_report_transfers_publish_no_partial_roots_and_release_gc_admission"
                    run_test(env, output, "transfer-interruption.log", ["cargo", "test", "-p", "crab-workflow", "--locked", "--lib",
                        transfer_case, "--", "--exact"], transfer_case)
                    proof["tests"]["transfers"] = 1
                    proof["checks"].update(interrupted_transfers=True, gc_writer_release=True)
                    run_test(env, output, "checkpoint.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_exp",
                        "checkpoint_lineage_apply_reset_and_resume_preserve_source", "--", "--exact"],
                        "checkpoint_lineage_apply_reset_and_resume_preserve_source")
                    proof["checks"]["checkpoint_resume"] = True
                    proof["tests"]["checkpoint"] = 1
                    run_test(env, output, "scale.log", ["cargo","test","-p","crab-workflow","--locked","--lib",
                        "results::tests::ten_thousand_run_catalog_keeps_bounded_pages_and_records_timings",
                        "--","--ignored","--exact","--nocapture"],
                        "results::tests::ten_thousand_run_catalog_keeps_bounded_pages_and_records_timings")
                    proof["checks"]["scale_10000"] = True
                    chart_cases = [
                        "comparison_keeps_healthy_curves_when_retained_plot_bytes_are_unavailable",
                        "unavailable_charts_keep_classified_states_without_provider_diagnostics",
                        "missing_plot_values_are_gaps_without_changing_numeric_axes_or_cli_rows",
                        "curve_sampling_retains_missing_intervals_and_endpoints",
                        "oversized_transforms_refuse_sampling_that_would_change_their_statistics",
                        "cross_file_coordinates_keep_absence_separate_from_literal_categories",
                        "plots_refuse_excess_series_and_normalized_bytes",
                        "sampling_refuses_more_missing_transitions_than_the_display_budget",
                        "equal_template_paths_require_equal_captured_bytes_for_overlays",
                        "builtin_comparisons_emit_revision_isolation_evidence",
                        "confusion_grid_expansion_is_bounded_before_rendering",
                        "maximum_overlays_emit_bounded_renderer_evidence",
                    ]
                    chart_prefix = "results::tests::chart_behavior::"
                    for name in ("chart-scale.json", "chart-scale.html", "chart-semantics.html", "chart-builtins.html"):
                        (output / name).unlink(missing_ok=True)
                    run_test(env, output, "chart-bounds.log", ["cargo", "test", "-p", "crab-workflow", "--locked", "--lib",
                        chart_prefix, "--", "--include-ignored", "--nocapture"], chart_prefix + chart_cases[-1])
                    chart_log = (output / "chart-bounds.log").read_text()
                    if any(not re.search(r"test " + re.escape(chart_prefix + case) + r" \.\.\. ok", chart_log) for case in chart_cases):
                        raise RuntimeError("Chart qualification omitted a required owner regression")
                    proof["tests"]["charts"] = len(chart_cases)
                    proof["checks"]["chart_response_bounds"] = True
                    proof["checks"]["partial_chart_evidence"] = True
                    sdk_case = "bare_results_use_captured_storage_and_preserve_operation_limits"
                    run_test(env, output, "sdk-results.log", ["cargo", "test", "-p", "crab-sdk", "--locked",
                        "--no-default-features", "--features", "results", "--test", "results",
                        sdk_case, "--", "--exact"], sdk_case)
                    proof["tests"]["sdk"] = 1
                    proof["checks"]["results_read_admission"] = True
                    template_cases = [
                        "templates_keep_each_selected_y_field_separate_in_every_run",
                        "dvc_optional_anchors_preserve_json_types_and_raw_columns",
                        "shared_dvc_plot_ids_retain_all_source_files",
                        "literal_field_names_are_escaped_for_vega_without_renaming_data",
                        "template_metadata_never_overwrites_selected_source_columns",
                        "horizontal_bar_templates_keep_dvc_numeric_x_and_categorical_y",
                        "multi_y_confusion_bounds_include_every_selected_class_column",
                        "dvc_anchor_validation_does_not_reinterpret_captured_values",
                        "literal_backslash_coordinates_keep_linked_numeric_domains",
                    ]
                    template_prefix = "results::tests::template_parity::"
                    for name in ("chart-template-parity.html", "chart-authored-hover.json"):
                        (output / name).unlink(missing_ok=True)
                    run_test(env, output, "template-parity.log", ["cargo", "test", "-p", "crab-workflow", "--locked", "--lib",
                        template_prefix, "--", "--nocapture"], template_prefix + template_cases[0])
                    template_log = (output / "template-parity.log").read_text()
                    if any(not re.search(r"test " + re.escape(template_prefix + case) + r" \.\.\. ok", template_log) for case in template_cases):
                        raise RuntimeError("Template qualification omitted a required owner regression")
                    proof["tests"]["templates"] = len(template_cases)
                    proof["checks"]["dvc_template_parity"] = True
                    partial_owner = "results::tests::template_availability::partial_template_evidence_preserves_the_verified_revision_in_either_order"
                    run_test(env, output, "partial-template-owner.log", ["cargo", "test", "-p", "crab-workflow", "--locked", "--lib",
                        partial_owner, "--", "--exact"], partial_owner)
                    proof["tests"]["partial_templates"] = 1
                    authored_case = "plots::authored_plot_failures_keep_real_metrics_and_export_the_retained_sources"
                    for name in ("authored-failures.html", "authored-failures-report.json", "authored-metrics.html"):
                        (output / name).unlink(missing_ok=True)
                    run_test(env, output, "authored-plots.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        authored_case, "--", "--exact"], authored_case)
                    proof["checks"]["authored_plot_capture"] = True
                    partial_cli = "plots::partial_template_reports_keep_verified_curves_after_cold_retrieval"
                    for name in ("partial-template.html", "partial-template-report.json"):
                        (output / name).unlink(missing_ok=True)
                    run_test(env, output, "partial-template-cli.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        partial_cli, "--", "--ignored", "--exact"], partial_cli)
                    proof["checks"]["partial_template_evidence"] = True
                    proof["tests"]["authored_plots"] = 2
                    matrix_cases = [
                        "large_matrices_count_every_observation_before_display_bounding",
                        "matrix_class_identities_distinguish_missing_literal_and_delimited_values",
                        "named_matrix_templates_remain_authored_when_they_override_a_builtin",
                        "matrix_numeric_classes_keep_javascript_equality_and_numeric_order",
                        "matrix_labels_are_data_and_dimension_roles_stay_distinct",
                    ]
                    matrix_prefix = "results::tests::matrix_behavior::"
                    for name in ("chart-large-matrices.html", "chart-matrix-identities.html", "chart-matrix-labels.html"):
                        (output / name).unlink(missing_ok=True)
                    run_test(env, output, "matrix-counts.log", ["cargo", "test", "-p", "crab-workflow", "--locked", "--lib",
                        matrix_prefix, "--", "--nocapture"], matrix_prefix + matrix_cases[0])
                    matrix_log = (output / "matrix-counts.log").read_text()
                    if any(not re.search(r"test " + re.escape(matrix_prefix + case) + r" \.\.\. ok", matrix_log) for case in matrix_cases):
                        raise RuntimeError("Matrix qualification omitted a required owner regression")
                    proof["tests"]["matrices"] = len(matrix_cases)
                    proof["checks"]["large_matrix_exact_counts"] = True
                    matrix_cli_case = "plots::cli_matrix_export_counts_complete_inputs_and_preserves_tagged_json_rows"
                    run_test(env, output, "matrix-cli.log", ["cargo", "test", "-p", "crab", "--locked",
                        "--no-default-features", "--features", "simd-accel,tier,watch", "--test", "workflow_results",
                        matrix_cli_case, "--", "--exact"], matrix_cli_case)
                    proof["tests"]["matrix_cli"] = 1
                    proof["checks"]["cli_matrix_exports"] = True

                    if http:
                        run_test(env, output, "http.log", ["cargo", "test", "-p", "crab-server", "--locked", "--lib",
                            "api::results::tests::real_storage_reports_follow_current_http_repository_grants", "--", "--ignored", "--exact"],
                            "api::results::tests::real_storage_reports_follow_current_http_repository_grants")
                        timeout_case = "server::http::tests::read_timeout_recovery_does_not_claim_an_unconfirmed_write"
                        run_test(env, output, "http-read-timeout.log", ["cargo", "test", "-p", "crab-server", "--locked", "--lib",
                            timeout_case, "--", "--exact"], timeout_case)
                        proof["checks"].update(dict.fromkeys(verifier.HTTP, True))
                        proof["tests"]["http"] = 2
                    if desktop:
                        desktop_proof(proof)
                        agent_case = "repository::results::tests::workflow_results_read_local_and_bare_evidence_and_reject_closed_scopes"
                        run_test(env, output, "agent.log", ["cargo", "test", "-p", "crab-desktop-agent", "--locked", agent_case, "--", "--exact"], agent_case)
                        proof["tests"]["agent"] = 1
                        error_cases = [
                            "shellout::tests::structured_failures_preserve_semantic_error_details_instead_of_tracing_logs",
                            "shellout::tests::failed_processes_without_error_envelopes_preserve_diagnostics_and_never_return_data",
                            "cache::tests::partial_inspection_keeps_issues_but_failed_cleanup_is_never_success",
                        ]
                        for index, case in enumerate(error_cases):
                            run_test(env, output, f"agent-error-{index}.log", ["cargo", "test", "-p", "crab-desktop-agent",
                                "--locked", case, "--", "--exact"], case)
                        proof["tests"]["agent_errors"] = len(error_cases)
                        env.update(E2E_SMOKE="1", PLAYWRIGHT_JSON_OUTPUT_FILE=str(output / "desktop.json"))
                        with (output / "desktop.log").open("w") as stream:
                            subprocess.run(["node", str(ROOT / "packages/desktop/node_modules/@playwright/test/cli.js"), "test", "tests/e2e/workflow-results.spec.ts",
                                "tests/e2e/results-offline.spec.ts", "tests/e2e/results-actions.spec.ts", "tests/e2e/results-authored-failures.spec.ts", "tests/e2e/results-complex.spec.ts", "--project=e2e-smoke", "--workers=1", "--reporter=list,json"],
                                cwd=ROOT / "packages/desktop", env=env, stdout=stream, stderr=stream, timeout=480, check=True)
                        tests = list(specs(json.loads((output / "desktop.json").read_text())))
                        if len(tests) != 5 or any(test["tests"][0]["results"][0]["status"] != "passed" for test in tests):
                            raise RuntimeError("Packaged Results tests were missing, skipped, or failed")
                        proof["checks"].update(dict.fromkeys(verifier.DESKTOP, True))
                        proof["tests"]["desktop"] = 5
                        proof["resources"]["desktop"] = json.loads((output / "native-resource-envelope.json").read_text())
                    if browser:
                        env["PLAYWRIGHT_JSON_OUTPUT_FILE"] = str(output / "browser.json")
                        with (output / "browser.log").open("w") as stream:
                            subprocess.run(["node",str(ROOT / "packages/ui/node_modules/@playwright/test/cli.js"),"test","tests/browser/results-chart-runtime.e2e.ts","--workers=1","--reporter=list,json"],
                                cwd=ROOT / "packages/ui",env=env,stdout=stream,stderr=stream,timeout=180,check=True)
                        tests = list(specs(json.loads((output / "browser.json").read_text())))
                        if len(tests) != 4 or any(test["tests"][0]["results"][0]["status"] != "passed" for test in tests):
                            raise RuntimeError("Offline Results report tests were missing, skipped, or failed")
                        proof["checks"].update(dict.fromkeys(verifier.BROWSER,True))
                        proof["tests"]["browser"] = 4

                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=20)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
        if source_tree() != tree or git("rev-parse", "HEAD").decode().strip() != source:
            raise RuntimeError("Source changed during Results qualification; rerun on the final source")
        current = {"binaries": {"cli": sha(cli)}}
        if desktop:
            desktop_proof(current)
        binding["binaries"] = current["binaries"]
        verifier.verify({**proof, "passed": True}, binding, desktop, http, browser)
        proof["passed"] = True
    finally:
        if env:
            for file in output.glob("*.log"):
                text = file.read_text(errors="replace")
                for key in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"):
                    text = text.replace(env[key], "[redacted]")
                file.write_text(text)
        (output / "evidence.json").write_text(json.dumps(proof, indent=2, sort_keys=True) + "\n")
    print("Verified queued runs, immutable Results, and cold remote recovery")


if __name__ == "__main__":
    main()
