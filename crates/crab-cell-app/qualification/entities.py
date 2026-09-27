"""Audit scheduled entity traffic against receipts, ownership and resource samples."""

from __future__ import annotations

import bisect
import csv
import hashlib
from pathlib import Path

STAGES = (3, 5, 10, 20)
SHAPES = ("uniform", "hot", "skewed")
POINTS = ((1, 4), (4, 16), (16, 64))
CELLS_PER_NODE = 4
SECONDS = 10


def rows(path: Path) -> list[dict]:
    with path.open(newline="") as source:
        return list(csv.DictReader(source, delimiter="\t"))


def destination(shape: str, arrival: int, cells: int) -> tuple[int, str]:
    if shape == "uniform":
        return arrival % cells, "write"
    if shape == "hot":
        return (1 + (arrival // 5) % (cells - 1) if arrival % 5 == 4 else 0), "write"
    assert shape == "skewed"
    return ((arrival // 5) % cells, "write") if arrival % 5 == 0 else (arrival % 4, "read")


def distribution(values: list[int]) -> dict:
    ordered = sorted(values)
    if not ordered:
        return dict(count=0)
    return dict(count=len(values), **{
        f"p{p}_ms": ordered[(len(ordered) * p + 99) // 100 - 1] / 1000
        for p in (50, 95, 99, 100)
    })


def verify_window(control: Path, nodes: int, shape: str, rate_per_node: int,
                  concurrency: int, window_id: int, positions: dict[int, list[int]]) -> dict:
    label = f"entities-{nodes}-{shape}-{rate_per_node}"
    metadata, = rows(control / f"{label}-window.tsv")
    assert metadata["shape"] == shape
    for key, expected in dict(window_id=window_id, nodes=nodes, rate_per_node=rate_per_node,
                              concurrency=concurrency, seconds=SECONDS).items():
        assert int(metadata[key]) == expected
    elapsed_us = int(metadata["elapsed_us"])
    assert elapsed_us >= SECONDS * 1_000_000
    assert int(metadata["ended_ms"]) >= int(metadata["started_ms"]) + SECONDS * 1000
    rate = nodes * rate_per_node
    planned = rate * SECONDS
    samples = sorted(rows(control / f"{label}.tsv"), key=lambda row: int(row["arrival"]))
    assert [int(row["arrival"]) for row in samples] == list(range(planned)), "missing or duplicate arrival"
    successes, arrival_latencies, writes = [], [], [0] * nodes
    outcomes, intervals = {}, []
    new_positions = {entity: [] for entity in range(nodes * CELLS_PER_NODE)}
    for sample in samples:
        arrival = int(sample["arrival"])
        scheduled = int(sample["scheduled_us"])
        started = int(sample["started_us"])
        elapsed = int(sample["elapsed_us"])
        entity = int(sample["entity"])
        sequence, read_sequence, count = [int(sample[key]) for key in ("sequence", "read_sequence", "count")]
        outcome = sample["outcome"]
        assert scheduled == arrival * 1_000_000 // rate
        assert started >= scheduled and elapsed >= 0
        assert (entity, sample["kind"]) == destination(shape, arrival, nodes * CELLS_PER_NODE)
        assert outcome in {"ok", "resolved", "write_only", "not_started", "absent", "client_full", "scheduler_late", "read_failed"}
        outcomes[outcome] = outcomes.get(outcome, 0) + 1
        if outcome in ("client_full", "scheduler_late"):
            assert elapsed == sequence == read_sequence == count == 0
            if outcome == "scheduler_late":
                assert started >= (arrival + 1) * 1_000_000 // rate
            continue
        assert started < (arrival + 1) * 1_000_000 // rate
        assert started + elapsed <= elapsed_us
        intervals += [(started, 1), (started + elapsed, -1)]
        if sequence:
            assert sample["kind"] == "write" and outcome in ("ok", "resolved", "write_only")
            new_positions[entity].append(sequence)
            writes[entity // CELLS_PER_NODE] += 1
        if outcome in ("ok", "resolved"):
            assert read_sequence >= max(sequence, 1)
            assert sequence > 0 if sample["kind"] == "write" else sequence == 0
            successes.append(elapsed)
            arrival_latencies.append(started + elapsed - scheduled)
        else:
            assert read_sequence == count == 0
            assert bool(sequence) == (outcome == "write_only")
    for entity, values in new_positions.items():
        previous = positions.setdefault(entity, [])
        assert len(values) == len(set(values)), "different requests reused one write receipt"
        assert not values or min(values) > max(previous, default=0), "write receipt regressed"
        previous.extend(sorted(values))
    for sample in samples:
        if sample["outcome"] in ("ok", "resolved"):
            expected = bisect.bisect_right(positions[int(sample["entity"])], int(sample["read_sequence"]))
            assert int(sample["count"]) == expected, "read value disagrees with its receipt"
    checks = rows(control / f"{label}-readback.tsv")
    assert [int(row["entity"]) for row in checks] == list(range(nodes * CELLS_PER_NODE))
    for row in checks:
        positions_for_cell = positions[int(row["entity"])]
        assert int(row["actual"]) == int(row["expected"]) == len(positions_for_cell), "lost or duplicated write"
        assert int(row["sequence"]) >= max(positions_for_cell, default=0)
    inflight, peak = 0, 0
    for _, delta in sorted(intervals):
        inflight += delta
        assert inflight >= 0
        peak = max(peak, inflight)
    assert inflight == 0 and peak <= concurrency
    return dict(nodes=nodes, shape=shape, rate_per_node=rate_per_node, concurrency=concurrency,
                planned=planned, outcomes=outcomes, fully_served_arrivals=len(successes) == planned,
                acknowledged_writes_by_node=writes, completed_actions=len(successes),
                completed_per_second=len(successes) * 1_000_000 / elapsed_us,
                service_latency=distribution(successes), arrival_latency=distribution(arrival_latencies),
                started_ms=int(metadata["started_ms"]), ended_ms=int(metadata["ended_ms"]),
                elapsed_us=elapsed_us, peak_client_inflight=peak)


def verify_entities(control: Path) -> dict:
    owners = rows(control / "entity-owners.tsv")
    identity = {}
    for stage in STAGES:
        selected = [row for row in owners if int(row["stage"]) == stage]
        assert [int(row["entity"]) for row in selected] == list(range(stage * CELLS_PER_NODE))
        for row in selected:
            entity = int(row["entity"])
            assert int(row["owner"]) == entity // CELLS_PER_NODE
            value = (row["cell"], row["owner"], row["epoch"], row["incarnation"])
            assert identity.setdefault(entity, value) == value, "existing Cell ownership changed"
        ingress = list(map(int, (control / f"entity-ingress-{stage}.txt").read_text().split()))
        assert len(ingress) == stage and min(ingress) > 0 and max(ingress) - min(ingress) <= 1
    assert len({value[0] for value in identity.values()}) == 80, "entity targets collapsed"
    windows, positions = [], {}
    for nodes in STAGES:
        for shape in SHAPES:
            for rate, concurrency in POINTS:
                windows.append(verify_window(control, nodes, shape, rate, concurrency, len(windows), positions))
        roots = rows(control / f"entity-roots-{nodes}.tsv")
        assert [int(row["entity"]) for row in roots] == list(range(nodes * CELLS_PER_NODE))
        for row in roots:
            entity = int(row["entity"])
            assert (row["cell"], row["owner"], row["epoch"], row["incarnation"]) == identity[entity]
            assert positions[entity], f"Cell {entity} received no acknowledged writes"
            assert int(row["root_sequence"]) >= max(positions[entity]), "published root does not cover writes"
        for node in range(nodes):
            assert sum(window["acknowledged_writes_by_node"][node] for window in windows if window["nodes"] == nodes) > 0
    resources = {}
    for node in range(20):
        samples = rows(control / f"node-{node}-resources.tsv")
        assert len(samples) >= 2
        assert all(int(row["active_cells"]) == CELLS_PER_NODE for row in samples)
        for column in ("at_ms", "cpu_usage_us", "throttled_us", "object_started", "object_finished", "bytes_read", "bytes_written"):
            values = [int(row[column]) for row in samples]
            assert values == sorted(values), f"node {node}: {column} regressed"
        assert {int(row["stage"]) for row in samples} >= {stage for stage in STAGES if node < stage}
        local, forwarded = map(int, (control / f"node-{node}.counts").read_text().split())
        assert local > 0 and forwarded > 0
        observations = rows(control / f"node-{node}-objects.tsv")
        assert len(observations) == 99 and len({(row["operation"], row["outcome"]) for row in observations}) == 99
        assert all(int(row["count"]) >= 0 for row in observations)
        assert any(row["operation"] == "put" and row["outcome"] == "success" and int(row["count"]) > 0 for row in observations)
        waits = [int(row["object_wait_us"]) for row in rows(control / f"node-{node}-durability.tsv")]
        assert waits and min(waits) >= 0
        resources[node] = dict(samples=len(samples), max_memory_current_bytes=max(int(row["memory_current_bytes"]) for row in samples),
                               max_disk_file_bytes=max(int(row["disk_bytes"]) for row in samples),
                               gateway_local=local, gateway_forwarded=forwarded, object_wait=distribution(waits),
                               logical_object_operations=sum(int(row["count"]) for row in observations))
        for window in windows:
            if node >= window["nodes"]:
                continue
            observed = [row for row in samples if window["started_ms"] <= int(row["at_ms"]) <= window["ended_ms"]]
            assert len(observed) >= 2, "missing in-window node samples"
            first, last = observed[0], observed[-1]
            window.setdefault("node_samples", {})[node] = dict(
                first_ms=int(first["at_ms"]), last_ms=int(last["at_ms"]),
                cpu_usage_us=int(last["cpu_usage_us"]) - int(first["cpu_usage_us"]),
                throttled_us=int(last["throttled_us"]) - int(first["throttled_us"]),
                logical_object_started=int(last["object_started"]) - int(first["object_started"]),
                logical_object_finished=int(last["object_finished"]) - int(first["object_finished"]),
                max_memory_current_bytes=max(int(row["memory_current_bytes"]) for row in observed),
                max_disk_file_bytes=max(int(row["disk_bytes"]) for row in observed),
                max_worker_jobs=max(int(row["worker_jobs"]) for row in observed))
    return dict(integrity_verified=True, windows=windows, resources=resources,
                verified_cells=len(positions), acknowledged_writes=sum(map(len, positions.values())),
                raw_sha256={path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                            for path in sorted(control.glob("*.tsv"))})
