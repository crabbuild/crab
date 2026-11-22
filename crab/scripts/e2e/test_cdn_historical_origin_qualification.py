from contextlib import contextmanager, redirect_stderr, redirect_stdout
from io import StringIO
import hashlib
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from urllib.parse import urlencode, urlsplit

import collect_cdn_historical_origin as HISTORY
import run_cdn_transport_checks as TRANSPORT
from cdn_qualification import CDN_PROVENANCE_FIELDS, verify_historical_origin
from test_cdn_transport_qualification import fixture, response


RETIRED = b"private retained version"
SHA256 = hashlib.sha256(RETIRED).hexdigest()
CASES = (
    ("cloudfront", "versionId", "fixture+version/="),
    ("cloud-cdn", "generation", "1234567890"),
    ("front-door", "versionid", "2026-11-22T00:00:00.0000000Z"),
    ("front-door", "snapshot", "2026-11-22T00:00:00.0000000Z"),
)


class HistoricalOriginQualificationTests(unittest.TestCase):
    def test_probes_the_same_native_version_with_and_without_authority(self):
        for edge, kind, selector in CASES:
            with self.subTest(edge=edge, kind=kind):
                selected = fixture(edge)
                key = f"{selected['fixture_prefix']}/{selected['object_path']}"
                entry = {"kind": kind, "selector": selector, "size": len(RETIRED), "sha256": SHA256}
                base = {
                    "cloudfront": "https://s3.us-east-1.amazonaws.com/fixture-bucket",
                    "cloud-cdn": "https://storage.googleapis.com/fixture-bucket",
                    "front-door": selected["native_origin"],
                }[edge]
                target = f"{urlsplit(base).path}/{key}?" + urlencode({kind: selector})
                requests = []

                @contextmanager
                def https(origin, method, path, headers, _check):
                    self.assertEqual((origin.geturl(), method, path), (base, "GET", target))
                    requests.append(headers)
                    if "Authorization" in headers:
                        self.assertEqual(headers["Authorization"], "Bearer fixture")
                        yield response(200, RETIRED, X_Goog_Generation=selector)
                    else:
                        yield response(403)

                def aws(profile, region, service, operation, *arguments):
                    requests.append({"Authorization": "ambient"})
                    self.assertEqual((profile, region, service, operation), ("crab", "us-east-1", "s3api", "get-object"))
                    self.assertEqual(arguments[:-1], ("--bucket", "fixture-bucket", "--key", key, "--version-id=" + selector,
                                                      "--expected-bucket-owner", "210987654321", "--range", f"bytes=0-{len(RETIRED)-1}",
                                                      "--endpoint-url", "https://s3.us-east-1.amazonaws.com"))
                    Path(arguments[-1]).write_bytes(RETIRED)
                    return {"VersionId": selector, "ContentLength": len(RETIRED),
                            "ContentRange": f"bytes 0-{len(RETIRED)-1}/{len(RETIRED)}"}

                with (patch.object(HISTORY, "native_base", return_value=urlsplit(base)),
                      patch.object(HISTORY, "bearer", return_value="Bearer fixture"),
                      patch.object(HISTORY, "aws", side_effect=aws),
                      patch.object(HISTORY, "https_response", side_effect=https),
                      patch.object(TRANSPORT, "https_response", side_effect=https)):
                    versions = HISTORY.collect(selected, edge, key, [entry], "crab")
                self.assertEqual(len(requests), 2)
                self.assertNotIn("Authorization", requests[1])
                self.assertEqual(versions, [{
                    "kind": kind, "selector_sha256": hashlib.sha256(selector.encode()).hexdigest(),
                    "sha256": SHA256, "size": len(RETIRED), "anonymous_status": 403,
                    "authorized_status": 206 if edge == "cloudfront" else 200,
                }])

    def test_gcs_requires_the_actual_generation(self):
        with patch.object(HISTORY, "https_response") as https:
            https.return_value.__enter__.return_value = response(200, RETIRED, X_Goog_Generation="456")
            with self.assertRaisesRegex(ValueError, "approved historical generation"):
                HISTORY.token_history(urlsplit("https://storage.googleapis.com/fixture-bucket"), "cloud-cdn",
                                      "key", "?generation=123", {"selector": "123", "size": len(RETIRED)}, "Bearer fixture")

    def test_candidate_digest_is_read_only_and_wrong_preregistration_stops_collection(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            selected = fixture("cloud-cdn")
            fixture_path, history_path, output = (root / name for name in ("fixture.json", "history.json", "trace.json"))
            raw = json.dumps(selected).encode()
            fixture_path.write_bytes(raw)
            history_path.write_text(json.dumps({"versions": [
                {"kind": "generation", "selector": "1234", "size": len(RETIRED), "sha256": SHA256},
            ]}))
            args = ["probe", "--fixture", str(fixture_path), "--history", str(history_path),
                    "--configuration-sha256", hashlib.sha256(raw).hexdigest()]
            with (patch.object(HISTORY, "collect") as collect, patch.object(sys, "argv", args), redirect_stdout(StringIO()) as stdout):
                self.assertEqual(HISTORY.main(), 0)
                self.assertRegex(stdout.getvalue(), r"^[0-9a-f]{64}\n$")
            collect.assert_not_called()
            approval = stdout.getvalue().strip()
            args += ["--output", str(output), "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "1"]
            with (patch.object(HISTORY, "collect") as collect,
                  patch.object(sys, "argv", args + ["--historical-origin-sha256", "0" * 64]),
                  redirect_stderr(StringIO()) as stderr):
                self.assertEqual(HISTORY.main(), 1)
                self.assertIn("differs from preregistration", stderr.getvalue())
            collect.assert_not_called()
            self.assertFalse(output.exists())
            approved = {"kind": "generation", "size": len(RETIRED), "sha256": SHA256,
                        "selector_sha256": hashlib.sha256(b"1234").hexdigest(),
                        "authorized_status": 200, "anonymous_status": 403}
            with (patch.object(HISTORY, "collect", return_value=[approved]),
                  patch.object(sys, "argv", args + ["--historical-origin-sha256", approval])):
                self.assertEqual(HISTORY.main(), 0)
            trace = json.loads(output.read_bytes())
            verify_historical_origin(
                trace, {field: trace[field] for field in CDN_PROVENANCE_FIELDS},
                {"historical_origin_sha256": approval},
                hashlib.sha256(selected["native_origin"].encode()).hexdigest(),
                {"key_sha256": hashlib.sha256(f"{selected['fixture_prefix']}/{selected['object_path']}".encode()).hexdigest(),
                 "body_sha256": selected["sha256"]},
            )


if __name__ == "__main__":
    unittest.main()
