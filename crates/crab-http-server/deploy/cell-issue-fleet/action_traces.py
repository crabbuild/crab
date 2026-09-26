#!/usr/bin/env python3
"""Join acknowledged issue writes to entry and owner events without clock subtraction."""

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
FIELD = re.compile(r'\b([a-z][a-z0-9_]*)=(?:"([^"\n]*)"|([^\s{}]+))')
EVENTS = {
    "application_submission", "http_response_ready", "cell_invocation_started",
    "cell_invocation_completed", "cell_execution_started", "cell_execution_completed",
    "cell_worker_started", "cell_worker_completed", "cell_capture_completed",
    "cell_proof_completed", "cell_command_response", "cell_publication_started",
    "cell_publication_completed",
}


def parse_log(text: str, node: str) -> list[dict]:
    events = []
    for line in text.splitlines():
        fields = {key: quoted or bare for key, quoted, bare in FIELD.findall(ANSI.sub("", line))}
        if fields.get("event") not in EVENTS:
            continue
        for key, value in fields.items():
            if key.endswith(("_us", "_ns", "_ms", "_bytes")) or key in ("status", "commit_sequence", "operation_id"):
                if not value.isdecimal():
                    raise ValueError(f"invalid {key} in {node} {fields['event']}")
                fields[key] = int(value)
            elif key == "succeeded":
                if value not in ("true", "false"):
                    raise ValueError(f"invalid success flag in {node} trace")
                fields[key] = value == "true"
        events.append({"node": node, **fields})
    return events


def one(events: list[dict], name: str) -> dict:
    selected = [event for event in events if event["event"] == name]
    if len(selected) != 1:
        raise ValueError(f"expected one {name}, found {len(selected)}; attribution is incomplete or ambiguous")
    return selected[0]


def join(samples: list[dict], events: list[dict]) -> list[dict]:
    try:
        http = defaultdict(list)
        mutations = defaultdict(list)
        for event in events:
            if "request_id" in event:
                http[event["request_id"]].append(event)
            if "owner_session" in event and "mutation_request_id" in event:
                mutations[tuple(event.get(key) for key in ("cell", "incarnation", "mutation_request_id"))].append(event)
        actions = []
        for sample in samples:
            if "acknowledged" not in sample:
                continue
            write = next(operation for operation in sample["operations"] if operation["operation"] == "write")
            request_id = write["http_request_id"]
            attempts = write["attempts"]
            if not attempts or attempts[-1]["status"] != 201 or attempts[-1]["http_request_id"] != request_id:
                raise ValueError(f"acknowledgement has no matching successful HTTP attempt: {request_id}")
            entry = http[request_id]
            submission = one(entry, "application_submission")
            invocation = one(entry, "cell_invocation_completed")
            response = one(entry, "http_response_ready")
            if submission["submission_id"] != sample["request_id"] or response["status"] != 201 or invocation["outcome"] != "committed":
                raise ValueError(f"HTTP acknowledgement does not match its submission: {request_id}")
            if any(event["node"] != write["entry"] for event in (submission, invocation, response)):
                raise ValueError(f"HTTP acknowledgement changed entry node: {request_id}")
            key = tuple(invocation[field] for field in ("cell", "incarnation", "mutation_request_id"))
            owner = mutations[key]
            released = one(owner, "cell_command_response")
            if released["commit_sequence"] != invocation["commit_sequence"] or released["source"] not in ("Object", "Fleet", "Recorded"):
                raise ValueError(f"receipt or proof mismatch for {request_id}")
            if len({(event["node"], event["owner_session"]) for event in owner}) != 1:
                raise ValueError(f"mutation has multiple execution owners: {request_id}")
            execution = one(owner, "cell_execution_completed")
            worker = one(owner, "cell_worker_completed")
            if execution["succeeded"] is not True or worker["succeeded"] is not True:
                raise ValueError(f"acknowledged mutation has failed execution: {request_id}")
            phases = {
                "http_response_ready_us": response["elapsed_us"],
                "client_invocation_us": invocation["elapsed_us"],
                "actor_queue_us": one(owner, "cell_execution_started")["actor_queue_us"],
                "worker_queue_us": one(owner, "cell_worker_started")["worker_queue_us"],
                "worker_execute_us": worker["worker_execute_us"],
                "worker_round_trip_us": execution["worker_round_trip_us"],
                "runtime_response_us": released["response_us"],
                "confirmation_us": released["confirmation_us"],
            }
            if released["source"] != "Recorded":
                proof = one(owner, "cell_proof_completed")
                if proof["succeeded"] is not True or proof["commit_sequence"] != released["commit_sequence"]:
                    raise ValueError(f"acknowledgement has no matching proof: {request_id}")
                phases["proof_wait_us"] = proof["proof_wait_us"]
            captures = [event for event in owner if event["event"] == "cell_capture_completed"]
            if released["source"] != "Recorded" and not captures:
                raise ValueError(f"acknowledgement has no capture observation: {request_id}")
            if any(event["succeeded"] is not True for event in captures):
                raise ValueError(f"acknowledged mutation has failed capture: {request_id}")
            actions.append({
                "submission_id": sample["request_id"], "http_request_id": request_id,
                "mutation_request_id": invocation["mutation_request_id"],
                "cell": invocation["cell"], "incarnation": invocation["incarnation"],
                "commit_sequence": invocation["commit_sequence"],
                "entry": write["entry"], "owner": released["node"],
                "owner_session": released["owner_session"], "proof": released["source"].lower(),
                "http_latency_ms": write["latency_ms"], "attempts": write["attempts"],
                "phases": phases, "captures": captures,
            })
        return actions
    except (KeyError, StopIteration) as error:
        raise ValueError(f"incomplete acknowledgement trace: missing {error}") from error


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=Path, required=True)
    parser.add_argument("--node-log", action="append", required=True, help="node-name=log-path")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    events = []
    for item in args.node_log:
        node, path = item.split("=", 1)
        events.extend(parse_log(Path(path).read_text(), node))
    samples = [json.loads(line) for line in args.samples.read_text().splitlines()]
    actions = join(samples, events)
    with args.output.open("x") as output:
        for action in actions:
            output.write(json.dumps(action) + "\n")


if __name__ == "__main__":
    main()
