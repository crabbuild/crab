"""Derive recovery work from matched process snapshots, independent of Cell ownership."""

import json
import sys
from decimal import Decimal, InvalidOperation, ROUND_HALF_UP


KINDS = (
    "candidate_count", "affected_cells", "catalog_shards", "catalog_pages",
    "control_reads", "follower_pages", "follower_frames", "follower_bytes",
    "peer_requests", "bundle_bytes", "object_reads", "object_writes",
)
PHASES = ("claim", "witness", "scope_validation", "pin_attach", "seal")
WORK_PREFIX = "crab_cell_node_log_recovery_work_total"
PHASE_PREFIX = "crab_cell_node_log_recovery_phase_seconds"


def recovery_work(before, after):
    if not isinstance(before, dict) or not isinstance(after, dict) or not before:
        raise ValueError("recovery requires process snapshots")
    if before.keys() != after.keys():
        raise ValueError("recovery process set changed")
    totals = {kind: 0 for kind in KINDS}
    phases = {phase: {"count": 0, "duration_ms": Decimal(0)} for phase in PHASES}
    for service, first in before.items():
        last = after[service]
        if not first["process"] or first["process"] != last["process"]:
            raise ValueError(f"{service}: process restarted")
        start = parse_metrics(first["metrics"])
        end = parse_metrics(last["metrics"])
        for kind in KINDS:
            name = f'{WORK_PREFIX}{{kind="{kind}"}}'
            totals[kind] += int(delta(start, end, name, integer=True))
        for phase in PHASES:
            label = f'{{phase="{phase}"}}'
            phases[phase]["count"] += int(delta(
                start, end, f"{PHASE_PREFIX}_count{label}", integer=True,
            ))
            phases[phase]["duration_ms"] += 1000 * delta(
                start, end, f"{PHASE_PREFIX}_sum{label}", integer=False,
            )
    for phase in phases.values():
        phase["duration_ms"] = int(phase["duration_ms"].to_integral_value(rounding=ROUND_HALF_UP))
    return {**totals, "phases": phases}


def parse_metrics(text):
    if not isinstance(text, str) or not text.strip():
        raise ValueError("missing recovery metrics")
    metrics = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        name, value = line.split()
        if name in metrics:
            raise ValueError(f"duplicate metric: {name}")
        metrics[name] = Decimal(value)
    return metrics


def delta(before, after, name, *, integer):
    if name not in before or name not in after:
        raise ValueError(f"missing metric: {name}")
    first, last = before[name], after[name]
    if not first.is_finite() or not last.is_finite() or first < 0 or last < first:
        raise ValueError(f"invalid or reset counter: {name}")
    if integer and (first != first.to_integral_value() or last != last.to_integral_value()):
        raise ValueError(f"nonintegral counter: {name}")
    return last - first


def main():
    try:
        snapshots = json.load(sys.stdin)
        result = recovery_work(snapshots["before"], snapshots["after"])
    except (ValueError, KeyError, TypeError, InvalidOperation) as error:
        print(f"Invalid recovery evidence: {error}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
