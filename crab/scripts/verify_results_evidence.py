"""Validate source-bound native Results qualification evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re

CORE = {
    "queued_runs", "actual_metrics_and_curves", "pinned_reproduction", "apply",
    "artifact_promotion_cas", "checkpoint_resume", "immutable_after_rename", "historical_action_guard",
    "run_removal", "scale_10000", "repository_gc", "cold_remote_retrieval", "retained_report_reopen", "captured_inputs_before_mutation", "produced_inputs_regenerated", "admitted_input_identities", "managed_results_transfer_refusal", "reproduction_lineage", "empty_private_settings_reproduction",
    "materialized_inputs_reproduced", "directory_inputs_reproduced", "chart_response_bounds", "dvc_template_parity", "large_matrix_exact_counts", "cli_matrix_exports",
    "partial_chart_evidence", "authored_plot_capture", "partial_template_evidence",
    "results_read_admission", "schema_discovery",
    "metadata_migration", "migration_source_validation", "migration_preserves_capture_roots",
    "private_template_boundary", "saved_template_assets", "custom_template_capture_failure", "recorded_template_reproduction",
    "saved_observations", "workspace_action_integrity", "workspace_target_boundaries",
    "directory_output_restore", "checkpoint_target_boundaries", "streaming_restore",
}
DESKTOP = {"desktop_compound_visualization", "desktop_resource_envelope", "desktop_partial_template_evidence", "desktop_authored_plot_failures", "desktop_storage_disconnect", "desktop_inflight_session_close", "desktop_metadata_migration", "desktop_saved_template", "desktop_structured_cli_errors", "desktop_saved_observation", "desktop_workspace_integrity", "desktop_queue_compare", "desktop_restart", "desktop_cold_retrieval", "desktop_failure_state", "native_session_scope", "offline_html", "desktop_apply", "desktop_reproduce", "desktop_checkpoint_resume", "desktop_artifact_promotion_cas", "desktop_queue_reply_ordering", "desktop_metadata_focus", "offline_foreground_paint", "desktop_matrix_counts", "desktop_corrupt_evidence", "desktop_missing_evidence", "desktop_closed_session", "desktop_partial_comparison", "desktop_failure_layout", "desktop_cross_file_failure", "desktop_bare_remote"}
FAULTS = {"failed_capture", "interrupted_capture", "interrupted_transfers", "gc_writer_release"}
HTTP = {"http_read_timeout_recovery", "http_real_storage", "http_current_repository_grants"}
# Desktop exports still require browser-rendered offline report proof.
BROWSER = {"browser_authored_plot_semantics", "browser_builtin_run_isolation", "browser_dvc_template_parity", "browser_large_matrix_counts", "browser_maximum_overlay_budget", "browser_missing_plot_gaps"}


def verify_resources(report, processes):
    if not isinstance(report, dict) or report.get("schema") != 1 \
            or report.get("cycles") != 20 or report.get("warmup_cycles") != 5 \
            or not re.fullmatch(r"[0-9a-f]{64}", str(report.get("report", ""))):
        raise ValueError("Results resource qualification is missing or malformed")
    samples = report.get("samples", [])
    if not isinstance(samples, list) or len(samples) != 21:
        raise ValueError("Results resource qualification omits required cycles")
    previous_reads = 0
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict) or type(sample.get("cycle")) is not int or sample["cycle"] != index:
            raise ValueError("Results resource qualification has missing cycles")
        readers = sample.get("readers", {})
        descriptors = sample.get("fd_counts", {})
        if not isinstance(readers, dict) or set(readers) != {"active", "peak", "started", "finished"} \
                or not isinstance(descriptors, dict) or set(descriptors) != processes:
            raise ValueError("Results resource qualification omits measured owners")
        numbers = [sample.get("heap_bytes"), *readers.values(), *descriptors.values()]
        if any(type(number) is not int or number < 0 or number > 2**53 - 1 for number in numbers):
            raise ValueError("Results resource qualification has invalid measurements")
        if readers["active"] != 0 or readers["started"] != readers["finished"] \
                or readers["started"] <= previous_reads or not 0 < readers["peak"] <= readers["started"]:
            raise ValueError("Results resource qualification has unfinished or unobserved reads")
        if any(count <= 0 or count - samples[0]["fd_counts"][owner] > 8
               for owner, count in descriptors.items()):
            raise ValueError("Results descriptor envelope exceeded its warmed baseline")
        if sample["heap_bytes"] - samples[0]["heap_bytes"] > 16 * 1024 * 1024:
            raise ValueError("Results heap envelope exceeded its warmed baseline")
        previous_reads = readers["started"]
    if "agent" in processes:
        terminal = report.get("after_cancellation", {})
        if not isinstance(terminal, dict) or set(terminal) != {"active", "peak", "started", "finished"} \
                or any(type(number) is not int or number < 0 for number in terminal.values()) \
                or terminal["active"] != 0 or terminal["started"] != terminal["finished"] \
                or terminal["started"] <= previous_reads:
            raise ValueError("Native Results resource qualification omits cancellation cleanup")


def verify(evidence, binding, desktop=False, http=False, browser=False):
    required = CORE | FAULTS | (DESKTOP if desktop else set()) | (HTTP if http else set()) | (BROWSER if browser else set())
    if evidence.get("schema") != 1 or evidence.get("passed") is not True:
        raise ValueError("Results qualification did not pass")
    for key in ("source_sha", "source_tree", "runtime_sha256", "platform", "workflow_run", "workflow_attempt"):
        expected = binding.get(key)
        if not expected or evidence.get(key) != expected:
            raise ValueError(f"Results evidence has a different {key}")
    checks = evidence.get("checks", {})
    if any(checks.get(name) is not True for name in required):
        raise ValueError("Results evidence omits required successful capabilities")
    counts = evidence.get("tests", {})
    expected_counts = {"core": 1, "checkpoint": 1, "inputs": 5, "settings": 1, "workspace": 8, "migration": 3, "restore": 2,
                       "charts": 12, "templates": 9, "authored_plots": 2, "partial_templates": 1, "matrices": 5, "matrix_cli": 1, "faults": 2, "transfers": 1, "transport": 1, "sdk": 1, "discovery": 1}
    if desktop:
        expected_counts.update(desktop=5, agent=1, agent_errors=3)
    if http:
        expected_counts.update(http=2)
    if browser:
        expected_counts.update(browser=4)
    if any(counts.get(name, 0) != count for name, count in expected_counts.items()):
        raise ValueError("Results evidence has missing, skipped, or zero relevant tests")
    digests = evidence.get("binaries", {})
    names = {"cli"} | ({"packaged_cli", "agent", "application"} if desktop else set())
    expected_digests = binding.get("binaries", {})
    if any(not re.fullmatch(r"[0-9a-f]{64}", str(digests.get(name, "")))
           or digests.get(name) != expected_digests.get(name) for name in names):
        raise ValueError("Results evidence has missing or different binary/package digests")
    if evidence.get("provider") != "native-rustfs-s3" or not evidence.get("platform"):
        raise ValueError("Results evidence lacks platform/provider binding")
    resources = evidence.get("resources", {})
    if not isinstance(resources, dict):
        raise ValueError("Results resource qualification is malformed")
    if desktop:
        verify_resources(resources.get("desktop"), {"main", "agent"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--source", required=True)
    parser.add_argument("--tree", required=True)
    parser.add_argument("--runtime", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--workflow-run", required=True)
    parser.add_argument("--workflow-attempt", required=True)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--packaged-cli", type=Path)
    parser.add_argument("--agent", type=Path)
    parser.add_argument("--application", type=Path)
    parser.add_argument("--desktop", action="store_true")
    parser.add_argument("--http", action="store_true")
    parser.add_argument("--browser", action="store_true", help="Require offline HTML report rendering proof")
    args = parser.parse_args()
    binaries = {}
    for name in ("cli", "packaged_cli", "agent", "application"):
        path = getattr(args, name)
        if path is not None:
            with path.open("rb") as source:
                binaries[name] = hashlib.file_digest(source, "sha256").hexdigest()
    binding = {"source_sha": args.source, "source_tree": args.tree,
               "runtime_sha256": args.runtime, "platform": args.platform,
               "workflow_run": args.workflow_run, "workflow_attempt": args.workflow_attempt,
               "binaries": binaries}
    verify(json.loads(args.report.read_text()), binding, args.desktop, args.http, args.browser)
    print("Verified native Results acceptance evidence")


if __name__ == "__main__":
    main()
