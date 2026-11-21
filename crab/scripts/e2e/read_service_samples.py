"""Exact journal statistics with disk-backed, bounded-cache sample storage."""
from contextlib import closing
import json
import math
from pathlib import Path
import sqlite3
import tempfile


def _ranks(count):
    return [(name, math.ceil(count * fraction) - 1)
            for name, fraction in (("p50", .5), ("p95", .95), ("p99", .99))
            if name != "p99" or count >= 1000]


def percentiles(values):
    values = sorted(values)
    return {name:values[rank] for name, rank in _ranks(len(values))} if values else {}


def read_journal(path: Path, start=0, end=None):
    with path.open() as source:
        source.seek(start)
        while end is None or source.tell() < end:
            line = source.readline()
            if not line:
                break
            yield json.loads(line)


def summarize(records, elapsed: float, directory: Path) -> dict:
    # The journal remains authoritative. Only one decoded record is retained;
    # exact order statistics spill to disk after the timed request loop ends.
    with tempfile.TemporaryDirectory(prefix="sample-summary-", dir=directory) as workspace:
        with closing(sqlite3.connect(str(Path(workspace) / "samples.sqlite"))) as database:
            database.execute("PRAGMA cache_size=-2048")
            database.execute("PRAGMA temp_store=FILE")
            database.execute("PRAGMA mmap_size=0")
            database.execute("""CREATE TABLE samples (
                workload TEXT, success INTEGER, latency REAL, ttfb REAL,
                capture REAL, query REAL, encoding REAL, total REAL)""")
            summaries = {}
            for record in records:
                summary = summaries.setdefault(record["workload"], {
                    "attempts":0, "successes":0, "rejected":0, "errors":0,
                    "oracle_mismatches":0, "client_cancellations":0,
                    "warmup_attempts":0, "setup_attempts":0})
                phase = record["phase"]
                if phase != "measured":
                    if phase in {"warmup", "setup"}:
                        summary[phase + "_attempts"] += 1
                    continue
                success = record["status"] == 200 and record["error"] is None
                rejected = record["status"] == 429 and record["error"] is None
                summary["attempts"] += 1
                summary["successes"] += success
                summary["rejected"] += rejected
                summary["errors"] += not (success or rejected)
                summary["oracle_mismatches"] += record["error"] == "GitOracleMismatch"
                summary["client_cancellations"] += record["error"] == "ClientCancelled"
                timings = record.get("server_timing_ms", {}) if success else {}
                database.execute("INSERT INTO samples VALUES (?, ?, ?, ?, ?, ?, ?, ?)", (
                    record["workload"], success, record["elapsed_us"],
                    record["ttfb_us"] if success else None,
                    *(timings.get(name) for name in ("open", "read", "encode", "total"))))
            database.commit()
            indexed = set()

            def ordered(workload, column, successes=False):
                # Repeated percentile sorts would rebuild a large temporary
                # tree for every rank. One covering index serves all workloads
                # and ranks without rescanning or retaining their sample lists.
                if column not in indexed:
                    database.execute(f"CREATE INDEX samples_{column} ON samples "
                                     f"(workload, {column}, success) WHERE {column} IS NOT NULL")
                    indexed.add(column)
                predicate = f"workload=? AND {column} IS NOT NULL"
                if successes:
                    predicate += " AND success=1"
                count = database.execute(
                    f"SELECT count(*) FROM samples WHERE {predicate}", (workload,)).fetchone()[0]
                values = {name:database.execute(
                    f"SELECT {column} FROM samples WHERE {predicate} ORDER BY {column} LIMIT 1 OFFSET ?",
                    (workload, rank)).fetchone()[0] for name, rank in _ranks(count)} if count else {}
                return count, values

            for workload, summary in summaries.items():
                summary.update(
                    elapsed_seconds=elapsed,
                    offered_requests_per_second=summary["attempts"] / elapsed,
                    successful_requests_per_second=summary["successes"] / elapsed,
                    latency_us=ordered(workload, "latency")[1],
                    successful_latency_us=ordered(workload, "latency", True)[1],
                    ttfb_us=ordered(workload, "ttfb", True)[1],
                    server_timing_ms={})
                for name, column in (("open", "capture"), ("read", "query"),
                                     ("encode", "encoding"), ("total", "total")):
                    count, values = ordered(workload, column, True)
                    if count:
                        summary["server_timing_ms"][name] = {"samples":count, **values}
            return summaries
