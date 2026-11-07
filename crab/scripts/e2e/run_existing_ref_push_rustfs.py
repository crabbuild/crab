#!/usr/bin/env python3
"""Measure independent existing-ref updates with explicit bulk setup receipts.

Reuse the concurrent driver's real-source, cold-clone and no-repair checks.
Bootstrap agent refs in one native transaction before timing their updates;
bootstrap latency stays visible in workload.ref_setup and the command ledger.
"""

from pathlib import Path

from run_concurrent_push_smoke import (
    ConcurrentPushSmoke, SmokeError, first_json_object, parse_args,
)


class ExistingRefPush(ConcurrentPushSmoke):
    def init_seed(self) -> None:
        super().init_seed()
        base = self.git_value(self.seed, ["rev-parse", "HEAD"])
        expected = {f"refs/heads/agent-{index:03d}": base for index in range(self.args.agents)}
        refspecs = [f"{base}:{name}" for name in expected]
        before = self.request_proxy.snapshot() if self.request_proxy else None
        record = self.run_crab(self.seed, ["push", "--json", "origin", *refspecs],
                               name="bootstrap agent refs")
        payload = first_json_object(Path(record.stdout_log).read_text(encoding="utf-8"), "push")
        refs = payload.get("data", {}).get("refs", []) if payload else []
        acknowledged = {ref.get("dst"): ref.get("src") for ref in refs if ref.get("status") == "ok"}
        self.check("all-agent-refs-bootstrapped", len(refs) == len(expected) and acknowledged == expected)
        self.report.workload["ref_setup"] = {
            "method": "bulk-bootstrap", "refs": len(expected), "base_oid": base,
            "duration_ms": record.duration_ms,
        }
        self.report.artifacts["bootstrap_push"] = record.stdout_log
        self.request_snapshot("bootstrap", before, attempted_pushes=1, successful_pushes=1)
        self.store_snapshot("bootstrap", attempted_pushes=1, successful_pushes=1)
        self.write_report(force=True)


def main() -> int:
    args = parse_args()
    if (args.skip_branch_fanout or not args.skip_same_branch
            or args.crash_boundary or args.marker_faults):
        raise SmokeError("existing-ref measurements require branch fanout, --skip-same-branch, "
                         "and no crash/fault scenarios")
    return ExistingRefPush(args).run()


if __name__ == "__main__":
    raise SystemExit(main())
