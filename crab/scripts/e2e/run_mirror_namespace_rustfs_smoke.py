#!/usr/bin/env python3
"""Qualify fresh mirrors, complete source namespaces and guarded initialization."""

import json
from pathlib import Path

import run_protocol_v2_partial_clone_rustfs_smoke as runner


def run(smoke):
    health = smoke.endpoint_health()
    smoke.check("RustFS healthy", health.get("ready") is True)
    smoke.ensure_bucket()
    source = smoke.source
    smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
    (source / "data.txt").write_text("original source content\n")
    smoke.run_git(source, ["add", "data.txt"])
    smoke.run_git(source, ["commit", "-m", "baseline"])
    old = smoke.git_value(source, ["rev-parse", "HEAD"], name="original source tip")
    smoke.run_git(source, ["update-ref", "refs/remotes/crab/main", old])
    smoke.run_git(source, ["switch", "-c", "release"])
    smoke.run_git(source, ["commit", "--allow-empty", "-m", "new main tip"])
    tip = smoke.git_value(source, ["rev-parse", "HEAD"], name="source tip")
    smoke.run_git(source, ["tag", "-a", "version-one", "-m", "annotated source tag"])
    smoke.run_git(source, ["update-ref", "refs/notes/source", tip])
    # A bare source has no applicable local pre-push hook policy.
    bare = smoke.run_root / "source.git"
    smoke.run_git(smoke.run_root, ["clone", "--mirror", "--no-local", str(source), str(bare)])
    smoke.record_provenance(tip, health)
    cache = smoke.run_root / "cache.git"
    mirror = [str(smoke.crab_bin), "mirror", str(bare), smoke.remote_url, "--cache-dir", str(cache)]
    prefix = f"{runner.REMOTE_PREFIX}/{smoke.run_id}/"

    def refs(target, label):
        rows = smoke.git_value(smoke.run_root, ["ls-remote", "--refs", str(target)], name=label)
        return dict((name, oid) for oid, name in (row.split() for row in rows.splitlines()))

    def inventory(label):
        result = smoke.run_aws(["list-objects-v2", "--bucket", smoke.args.bucket, "--prefix", prefix], name=label)
        return {item["Key"]: item["ETag"] for item in json.loads(smoke.stdout(result)).get("Contents", [])}

    def inspect(label, path=None, allow=False):
        flags = ["--check", "--ci", "--json"]
        if path:
            flags += ["--write-plan", str(path)]
        if allow:
            flags += ["--allow-delete-refs"]
        result = smoke.run_cmd(label, mirror + flags, smoke.run_root, check=False)
        return result, smoke.json_data(result, "mirror.check")

    def apply(label, path, allow=False):
        flags = ["--apply-plan", str(path), "--json"]
        if allow:
            flags += ["--allow-delete-refs"]
        return smoke.json_data(smoke.run_cmd(label, mirror + flags, smoke.run_root), "mirror.apply")

    smoke.check("fresh prefix is empty", not inventory("before inspection"))
    result, check = inspect("check cannot create a missing repository")
    smoke.check("inspection remains read-only and fails closed", result["exit_code"] != 0
                and check["state"] == "unverifiable" and not inventory("after inspection"))
    smoke.run_cmd("mirror into fresh prefix without separate init", mirror + ["--json"], smoke.run_root)
    expected = refs(bare, "all source refs")
    smoke.check("fresh mirror preserves every ref", refs(smoke.remote_url, "fresh destination refs") == expected)
    advertised_head = smoke.git_value(smoke.run_root, ["ls-remote", "--symref", smoke.remote_url, "HEAD"], name="destination default branch")
    source_head = smoke.git_value(smoke.run_root, ["ls-remote", "--symref", str(bare), "HEAD"], name="source default branch")
    smoke.check("fresh mirror preserves source default branch", advertised_head == source_head)
    smoke.check("push cannot overwrite source tracking refs in cache", refs(cache, "cache after push") == expected)
    smoke.run_cmd("repeat full mirror", mirror + ["--json"], smoke.run_root)
    smoke.check("repeated cache preserves complete namespace", refs(cache, "cache after repeat") == expected)

    name = "refs/remotes/crab/source-owned"
    smoke.run_git(bare, ["update-ref", name, tip])
    plan = smoke.artifacts / "tracking-add-plan.json"
    result, check = inspect("source-only tracking ref is real drift", plan)
    data = json.loads(plan.read_text())
    smoke.check("tracking-only drift cannot report healthy equality", result["exit_code"] != 0
                and check["state"] == "source_ahead" and not check["ci_passed"]
                and not data["blocked"] and len(data["actions"]) == 1
                and data["actions"][0]["ref_name"] == name)
    applied = apply("apply source-owned tracking ref", plan)
    smoke.check("tracking ref published with receipt", applied["actions_applied"] == 1
                and bool(applied.get("transaction_id")) and refs(smoke.remote_url, "tracking ref applied")[name] == tip)
    protected = smoke.artifacts / "tracking-protected-plan.json"
    result, check = inspect("deletion approval cannot hide an existing source ref", protected, allow=True)
    smoke.check("matching tracking ref is never proposed for deletion", result["exit_code"] == 0
                and check["ci_passed"] and not json.loads(protected.read_text())["actions"])
    apply("apply empty approved plan", protected, allow=True)
    smoke.check("approved empty plan preserves tracking ref", refs(smoke.remote_url, "protected destination ref")[name] == tip)

    smoke.run_git(bare, ["update-ref", name, old])
    smoke.run_cmd("legacy mirror updates a tracking-only change", mirror + ["--json"], smoke.run_root)
    smoke.check("tracking-only force update is not skipped", refs(smoke.remote_url, "updated tracking ref")[name] == old)
    smoke.run_git(bare, ["update-ref", "-d", name])
    deletion = smoke.artifacts / "tracking-delete-plan.json"
    inspect("plan real source tracking deletion", deletion, allow=True)
    deleted = apply("apply reviewed tracking deletion", deletion, allow=True)
    smoke.check("only source-absent tracking ref is deleted", deleted["actions_applied"] == 1
                and refs(smoke.remote_url, "after tracking deletion") == refs(bare, "remaining source refs"))

    # Reachability through this namespace must protect data as well as refs.
    smoke.run_git(source, ["switch", "-c", "pointer-fixture"])
    (source / "missing.ptr").write_text("version https://crab.dev/spec/v1\nfile-hash " + "f" * 64 + "\nsize 1\n")
    smoke.run_git(source, ["add", "missing.ptr"])
    smoke.run_git(source, ["commit", "-m", "pointer reachable only from source tracking ref"])
    smoke.run_git(source, ["push", str(bare), "HEAD:" + name])
    missing = smoke.artifacts / "tracking-pointer-plan.json"
    result, check = inspect("tracking ref cannot hide missing pointer dependencies", missing)
    smoke.check("namespace-only pointer blocks publication", result["exit_code"] != 0
                and check["pointers"]["state"] == "missing" and json.loads(missing.read_text())["blocked"])
    smoke.run_git(bare, ["update-ref", "-d", name])

    # Fault only captured objects in this disposable prefix; retain and restore
    # their bytes even if a refusal regression interrupts qualification.
    for suffix in ("layout", "manifest"):
        key = prefix + suffix
        original = smoke.artifacts / (suffix + "-original.json")
        smoke.run_aws(["get-object", "--bucket", smoke.args.bucket, "--key", key, str(original)], name="capture " + suffix)
        smoke.run_aws(["delete-object", "--bucket", smoke.args.bucket, "--key", key], name="remove isolated " + suffix)
        try:
            before = inventory("inventory after removing " + suffix)
            refused = smoke.run_cmd("legacy mirror refuses missing " + suffix, mirror + ["--json"], smoke.run_root, check=False)
            smoke.check("missing " + suffix + " cannot trigger in-place repair", refused["exit_code"] not in (0, -124)
                        and inventory("inventory after " + suffix + " refusal") == before)
        finally:
            smoke.run_aws(["put-object", "--bucket", smoke.args.bucket, "--key", key,
                           "--body", str(original)], name="restore isolated " + suffix)
    result, check = inspect("final recovered integrity")
    smoke.check("restored repository passes integrity", result["exit_code"] == 0 and check["ci_passed"])
    clone = smoke.run_root / "restored.git"
    smoke.run_git(smoke.run_root, ["clone", "--mirror", smoke.remote_url, str(clone)])
    smoke.run_git(clone, ["fsck", "--strict", "--full"])
    smoke.check("cold clone has exact final namespace", refs(clone, "cold clone refs") == refs(bare, "final source refs"))
    smoke.redaction_check()


def main():
    smoke = runner.ProtocolV2PartialCloneSmoke(runner.parse_args())
    digest = runner.sha256_file(smoke.crab_bin)
    smoke.report.update({"schema": "crab.mirror-namespace-smoke", "version": "1.0",
                         "candidate_binary_sha256": digest, "driver_sha256": runner.sha256_file(Path(__file__))})
    try:
        run(smoke)
        smoke.check("candidate unchanged", runner.sha256_file(smoke.crab_bin) == digest)
        smoke.report["status"] = "passed"
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = runner.redact_text(str(error), smoke.credentials())
    finally:
        smoke.write_report()
    print(json.dumps({"status": smoke.report["status"], "error": smoke.report.get("error"), "report": str(smoke.artifacts / "report.json")}))
    return 0 if smoke.report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
