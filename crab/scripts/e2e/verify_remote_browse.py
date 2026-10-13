#!/usr/bin/env python3
"""Compare qualify_browse JSONL with a read-only source Git repository."""
import argparse
import ast
import hashlib
import json
import subprocess
from pathlib import Path


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def check_line_origins(git, head: str, path: bytes, ranges: list[dict]) -> int:
    native = []
    source_path = None
    for line in git("blame", "--first-parent", "--root", "--line-porcelain", head, "--", path).splitlines():
        fields = line.split()
        if not line.startswith(b"\t") and len(fields) >= 3 and len(fields[0]) == 40 and fields[1].isdigit() and fields[2].isdigit():
            origin = fields[0].decode()
            final_line = int(fields[2])
        elif line.startswith(b"filename "):
            source_path = line[9:]
            if source_path.startswith(b'"'):
                source_path = ast.literal_eval("b" + source_path.decode("ascii"))
        elif line.startswith(b"\t"):
            native.append((final_line, origin, source_path))
    actual = [(line, item["commit"], bytes(item["source_path"])) for item in ranges
              for line in range(item["start"], item["start"] + item["lines"])]
    require(actual == native, "blame line origins differ")
    return len(native)


def verify_queries(source: Path, records: list[dict], revision: str) -> dict:
    """Check isolated qualification queries against native Git, including every blame line."""
    def git(*args) -> bytes:
        return subprocess.check_output(["git", "-C", str(source), *args])

    head = git("rev-parse", revision).decode().strip()
    results = [record for record in records if record["kind"] == "result"]
    names = [record["operation"] for record in results]
    expected_names = {"resolve", "commit", "tree", "blob", "object-point", "object-batch",
                      "history", "path-history", "attribution", "blame"}
    selection = records[-1]["selection"]
    require(len(names) == len(set(names)), "duplicate query evidence")
    require(set(names) == (expected_names if selection == "all" else {selection}), "missing query evidence")
    measurements = [record for record in records if record["kind"] == "measurement"]
    for name in names:
        pair = [record for record in measurements if record["operation"] == name]
        require([record["cache"] for record in pair] == ["cold", "warm"], "missing cold/warm measurement")
    require(sum(record["operation"] == "open" for record in measurements) == 2 * len(names), "missing open measurement")
    require(all(record["outcome"] == "ok" and record["active_backend_operations"] == 0
                for record in measurements), "failed or unfinished query")
    root = []
    for item in git("ls-tree", "-z", head).split(b"\0"):
        if item:
            metadata, path = item.split(b"\t", 1)
            mode, kind, oid = metadata.split()
            root.append({"path":list(path), "oid":oid.decode(), "mode":int(mode, 8)})

    def check_object(value: dict) -> None:
        body = git("cat-file", value["kind"], value["oid"])
        header = f"{value['kind']} {len(body)}\0".encode()
        require(value["computed_oid"] == value["oid"] == hashlib.sha1(header + body).hexdigest(), "object body hash differs")
        require(value["bytes"] == len(body), "object size differs")

    lines_checked = 0
    for record in results:
        name, value, path = record["operation"], record["value"], bytes(record["path"])
        if name == "resolve":
            require(value["commit"] == head, "resolved commit differs")
        elif name == "commit":
            require(value["oid"] == head, "commit differs")
            require(value["tree"] == git("rev-parse", f"{head}^{{tree}}").decode().strip(), "commit tree differs")
            require(value["parents"] == git("rev-list", "--parents", "-n", "1", head).decode().split()[1:], "commit parents differ")
        elif name == "tree":
            require(value["entries"] == root[:100], "directory page differs")
            require(value["has_more"] == (len(root) > 100), "directory continuation differs")
        elif name in {"blob", "object-point"}:
            require(value["oid"] == git("rev-parse", head.encode() + b":" + path).decode().strip(), "file object differs")
            check_object(value)
        elif name == "object-batch":
            require([item["oid"] for item in value["objects"]] == [item["oid"] for item in root[:100]], "object batch order differs")
            for item in value["objects"]:
                check_object(item)
        elif name in {"history", "path-history"}:
            args = ["log", "--first-parent", "--format=%H", "--max-count=1001", head]
            if name == "path-history":
                args += ["--", path]
            expected = git(*args).decode().splitlines()
            require(value["commits"] == expected[:1000], "history differs")
            require(value["has_more"] == (len(expected) > 1000), "history continuation differs")
        elif name == "attribution":
            require([(item["path"], item["oid"]) for item in value["entries"]] ==
                    [(item["path"], item["oid"]) for item in root[:100]], "attribution page differs")
            for item in value["entries"]:
                expected = git("log", "--first-parent", "-1", "--format=%H", head, "--", bytes(item["path"])).decode().strip()
                require(item["commit"] == expected, "last-change commit differs")
        elif name == "blame":
            lines_checked += check_line_origins(git, head, path, value["ranges"])
        elif name == "graph-pages":
            rows = value["records"]
            require(len(rows) == 4 and rows[1] == rows[3], "graph batch order/duplicates differ")
            for item in rows:
                body = git("cat-file", "commit", item["oid"])
                headers = body.split(b"\n\n", 1)[0].splitlines()
                tree = next(line[5:].decode() for line in headers if line.startswith(b"tree "))
                parents = [line[7:].decode() for line in headers if line.startswith(b"parent ")]
                committer = next(line for line in headers if line.startswith(b"committer "))
                require(item["tree"] == tree and item["parents"] == parents
                        and item["committer_seconds"] == int(committer.rsplit(b" ", 2)[1]),
                        "graph record differs from native Git")
        else:
            raise AssertionError("unverified qualification operation: " + name)
    return {"status":"passed", "head":head, "queries_checked":len(results),
            "blame_lines_checked":lines_checked, "measurements":measurements}


def verify(source: Path, evidence: Path, revision: str) -> dict:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", "-C", str(source), *args])

    records = [json.loads(line) for line in evidence.read_text().splitlines()]
    if not records or records[-1]["kind"] != "complete":
        raise ValueError("reader did not complete")
    if records[-1].get("schema") == 1 and "selection" in records[-1]:
        return verify_queries(source, records, revision)
    head = git("rev-parse", revision).decode().strip()
    snapshot, = [r for r in records if r["kind"] == "snapshot"]
    require(snapshot["commit"] == head, "snapshot revision differs")
    require(snapshot["tree"] == git("rev-parse", f"{head}^{{tree}}").decode().strip(), 'browse evidence differs from Git')
    refs = [r for r in records if r["kind"] == "ref"]
    require(refs == [{"kind": "ref", "name": "refs/heads/main", "oid": head}], 'browse evidence differs from Git')

    expected = {}
    for line in git("ls-tree", "-r", "-t", "-z", head).split(b"\0"):
        if not line:
            continue
        metadata, path = line.split(b"\t", 1)
        mode, kind, oid = metadata.split()
        expected[path] = (int(mode, 8), kind.decode(), oid.decode())
    entries = [r for r in records if r["kind"] == "entry"]
    actual = {bytes(r["path"]): (r["mode"], r["oid"]) for r in entries}
    require(len(actual) == len(entries), "duplicate directory entry")
    require(actual == {p: (v[0], v[2]) for p, v in expected.items()}, "tree differs")

    attribution = [r for r in records if r["kind"] == "attribution"]
    require(len(attribution) == len(entries), "missing tree attribution")
    require({bytes(r["path"]):r["oid"] for r in attribution} == {p:v[2] for p,v in expected.items()}, "attribution entries differ")
    for item in attribution:
        expected_commit = git("log", "--first-parent", "-1", "--format=%H", head, "--", bytes(item["path"])).decode().strip()
        require(item["commit"] == expected_commit, "tree attribution differs")

    commits = [r for r in records if r["kind"] == "commit"]
    history = git("rev-list", "--first-parent", "--max-count=1000", head).decode().splitlines()
    require([r["oid"] for r in commits] == history, "paginated history differs")
    for commit in commits:
        raw = git("cat-file", "commit", commit["oid"])
        header, message = raw.split(b"\n\n", 1)
        fields = [line.split(b" ", 1) for line in header.splitlines() if not line.startswith(b" ")]
        parents = [v.decode() for k, v in fields if k == b"parent"]
        require(parents == commit["parents"], "commit parents differ")
        values = dict(fields)
        require(values[b"tree"].decode() == commit["tree"], "commit tree differs")
        require(list(message) == commit["message"], "commit message differs")
        identity, timestamp, _ = values[b"author"].rsplit(b" ", 2)
        name, email = identity.rsplit(b" <", 1)
        require(list(name) == commit["author_name"], "author differs")
        require(list(email[:-1]) == commit["author_email"], "author email differs")
        require(int(timestamp) == commit["author_seconds"], "author time differs")
        require(int(values[b"committer"].rsplit(b" ", 2)[1]) == commit["committer_seconds"], 'browse evidence differs from Git')

    selected_history, = [record for record in records if record["kind"] == "path-history"]
    expected_history = git("log", "--first-parent", "--format=%H", "--max-count=1001", head,
                           "--", bytes(selected_history["path"])).decode().splitlines()
    require(selected_history["commits"] == expected_history[:1000], "selected path history differs")
    require(selected_history["has_more"] == (len(expected_history) > 1000), "selected history continuation differs")
    blame, = [record for record in records if record["kind"] == "blame"]
    require(blame["path"] == selected_history["path"], "blame and history paths differ")
    blame_lines = check_line_origins(git, head, bytes(blame["path"]), blame["ranges"])

    samples = [r for r in records if r["kind"] == "blob"]
    require(len(samples) == min(128, sum(v[1] == "blob" for v in expected.values())), 'browse evidence differs from Git')
    for blob in samples:
        oid = expected[bytes(blob["path"])][2]
        require(blob["oid"] == blob["computed_oid"] == oid, "sample content differs")
        require(len(git("cat-file", "blob", oid)) == blob["bytes"], "sample size differs")

    errors, = [r for r in records if r["kind"] == "errors"]
    require(errors == {"kind": "errors", "directory_as_blob": "EntryNotBlob", "cancelled": "Cancelled"}, 'browse evidence differs from Git')

    archive = [r for r in records if r["kind"] == "archive"]
    require(len(archive) == len(expected), "archive count differs")
    require({bytes(r["path"]) for r in archive} == expected.keys(), "archive paths differ")
    blob_count = total_bytes = 0
    for entry in archive:
        mode, kind, oid = expected[bytes(entry["path"])]
        require(entry["mode"] == mode, "archive mode differs")
        if kind == "blob":
            require(entry["computed_oid"] == oid, "archive content differs")
            blob_count += 1
            total_bytes += entry["bytes"]
        else:
            require(entry["computed_oid"] is None and entry["bytes"] is None, 'browse evidence differs from Git')
    return {"status": "passed", "head": head, "commits_checked": len(commits),
            "tree_entries_checked": len(entries), "blame_lines_checked": blame_lines, "blobs_checked": blob_count,
            "blob_bytes_checked": total_bytes, "direct_blob_samples": len(samples),
            "reader": records[-1], "snapshot": snapshot, "error_checks": errors,
            "evidence_sha256": hashlib.sha256(evidence.read_bytes()).hexdigest()}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("evidence", type=Path)
    parser.add_argument("--revision", default="HEAD")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = verify(args.source, args.evidence, args.revision)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
