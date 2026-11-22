"""Check the managed issuer evidence producer through the collector entry point."""

import hashlib
import json
import sys
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import collect_cdn_managed_issuer as COLLECTOR
import cdn_qualification as EVIDENCE
from collect_cdn_product_metrics import EVENT, NATIVE_TARGET, TARGET, VERIFIED_GIT_EVENT
from test_cdn_transport_qualification import fixture


APPROVAL = "a" * 64
ISSUER = "b" * 64
REPOSITORY = "c" * 64
BODY = b"verified managed content"


def digest(content):
    return hashlib.sha256(content).hexdigest()


def write_json(path, value):
    raw = json.dumps(value).encode()
    path.write_bytes(raw)
    return digest(raw)


def event(target, **fields):
    return {"target": target, "fields": fields}


class CdnManagedIssuerQualificationTests(unittest.TestCase):
    def inputs(self, root, edge):
        config = fixture(edge, "private")
        config["scope"] = config["scope"].replace("/direct", "/managed")
        components = dict.fromkeys(EVIDENCE.deployment_component_names(edge, "private"), "e" * 64)
        policy_digest = EVIDENCE.deployment_policy_digest(edge, "private", components)
        config["deployment_policy_sha256"] = policy_digest
        fixture_digest = write_json(root / "fixture.json", config)
        provenance = {
            "scope": config["scope"], "configuration_sha256": fixture_digest, "source_sha": "f" * 40,
            "workflow_run_id": "112", "workflow_run_attempt": "2",
        }
        write_json(root / "policy.json", provenance | {
            "schema": "crab.cdn-deployment-policy/v1", "deployment_policy_sha256": policy_digest, "components": components,
        })
        report = provenance | {"platform": "linux-x86_64", "product_action": {
            "name": "git-fetch", "observed_sha256": digest(BODY), "observed_bytes": len(BODY), "observed_edge_gets": 1,
        }}
        write_json(root / "observations.json", report)
        registration = {key: provenance[key] for key in ("scope", "configuration_sha256", "source_sha")} | {
            "schema": "crab.cdn-benchmark/v1", "deployment_policy_sha256": policy_digest,
            "workload": "git-fetch", "samples": 30, "p95_budget_ms": 100, "cost_budget_microusd": 100,
            "regions": {"near": "near-origin", "distant": "distant-edge"},
            "scoped_view_sha256": "d" * 64, "cache_reuse_sha256": "d" * 64, "rotation_overlap_policy_sha256": "d" * 64,
            "managed_issuer": {"identity_sha256": ISSUER, "version": "1.2.3", "approval_sha256": APPROVAL},
        }
        benchmarks = write_json(root / "benchmarks.json", {f"{config['scope']}/linux-x86_64": registration})
        identity = {"issuer_identity_sha256": ISSUER, "issuer_version": "1.2.3", "repository_sha256": REPOSITORY}
        origin, prefix, viewer = (
            digest(config[key].encode()) for key in ("native_origin", "fixture_prefix", "base_url")
        )
        for role in COLLECTOR.ROLES:
            directory = root / role
            directory.mkdir()
            ok = role in ("approved_v2", "v1_peer")
            version = 1 if role == "v1_peer" else 2
            traces = [
                event(COLLECTOR.TARGET, **identity, message=COLLECTOR.RESOLUTION,
                      binding_approval_sha256=APPROVAL, binding_origin_sha256=origin,
                      binding_prefix_sha256=prefix, binding_viewer_sha256=viewer),
                event(COLLECTOR.TARGET, **identity, message=COLLECTOR.GRANT, operation="Fetch", request_version=version,
                      request_approval_sha256=APPROVAL if version == 2 else "", status=403 if role == "denied_v2" else 201,
                      grant_version=0 if role == "denied_v2" else version, accepted=ok,
                      grant_approval_sha256={"approved_v2": APPROVAL, "mismatched_v2": "7" * 64}.get(role, ""),
                      grant_origin_sha256=origin if ok else "", grant_prefix_sha256=prefix if ok else ""),
            ]
            provider = EVIDENCE.PROVIDER_FOR_EDGE[edge]
            if role == "approved_v2":
                route = {"provider": provider, "access": "private", "consumer": "cli", "family": "git", "profile": "managed"}
                traces.extend([
                    event(TARGET, **route, message=EVENT, status=200, cache="hit", elapsed_ms=1, range=False,
                          bytes=len(BODY), complete=True, request_sha256="6" * 64),
                    event(TARGET, **route, message=VERIFIED_GIT_EVENT, source="edge"),
                ])
            if role == "v1_peer":
                traces.append(event(NATIVE_TARGET, provider=provider, message="Native object store read", method="get",
                                    status=200, bytes=len(BODY)))
            if ok:
                (directory / "payload").write_bytes(BODY)
            (directory / "exit-code").write_text("0\n" if ok else "1\n")
            (directory / "stderr.jsonl").write_text("\n".join(json.dumps(trace) for trace in traces) + "\n")
        return [
            "probe", "--fixture", str(root / "fixture.json"), "--configuration-sha256", fixture_digest,
            "--benchmarks", str(root / "benchmarks.json"), "--benchmarks-sha256", benchmarks,
            "--observations", str(root / "observations.json"), "--deployment-policy", str(root / "policy.json"),
            "--actions", str(root), "--output", str(root / "managed_v2_issuer.json"),
        ]

    def test_collects_all_four_managed_issuer_roles_for_each_provider(self):
        for edge in EVIDENCE.PROVIDER_FOR_EDGE:
            with self.subTest(edge=edge), TemporaryDirectory() as temporary:
                root = Path(temporary)
                args = self.inputs(root, edge)
                with patch.object(sys, "argv", args):
                    self.assertEqual(COLLECTOR.main(), 0)
                trace = json.loads((root / "managed_v2_issuer.json").read_bytes())
                operations = trace["operations"]
                self.assertEqual([operations[role]["status"] for role in COLLECTOR.ROLES], [201, 201, 403, 201])
                self.assertEqual(operations["approved_v2"]["edge_payload_requests"], 1)
                self.assertEqual(operations["v1_peer"]["native_payload_reads"], 1)
                self.assertNotIn(REPOSITORY, json.dumps(trace))

    def test_managed_rejection_cannot_replay_or_start_payload_traffic(self):
        for case, reason in (("replay", "one observed grant POST"), ("origin", "must read no edge or native payload"),
                             ("succeeded", "did not exit with the expected result")):
            with self.subTest(case=case), TemporaryDirectory() as temporary:
                root = Path(temporary)
                args = self.inputs(root, "cloudfront")
                rejected = root / "mismatched_v2"
                logs = rejected / "stderr.jsonl"
                if case == "replay":
                    with logs.open("a") as output:
                        output.write(logs.read_text().splitlines()[-1] + "\n")
                elif case == "origin":
                    with logs.open("a") as output:
                        output.write(json.dumps(event(NATIVE_TARGET, message="Native object store read", provider="s3",
                                                      method="get", status=0, bytes=0)) + "\n")
                else:
                    (rejected / "exit-code").write_text("0\n")
                with patch.object(sys, "argv", args), redirect_stderr(StringIO()) as error:
                    self.assertEqual(COLLECTOR.main(), 1)
                self.assertIn(reason, error.getvalue())
                self.assertFalse((root / "managed_v2_issuer.json").exists())


if __name__ == "__main__":
    unittest.main()
