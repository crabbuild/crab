#!/usr/bin/env python3
"""Keep the streaming performance meter's wire accounting canonical."""
import email.message
import json
from pathlib import Path
import subprocess
import unittest

from run_concurrent_push_smoke import RequestCountingProxy


class NativeProviderMeterTests(unittest.TestCase):
    def test_native_meter_preserves_canonical_attempt_accounting(self):
        prefix = "bucket/task"
        expected = RequestCountingProxy("http://127.0.0.1:1", prefix)
        paths = ["/" + prefix + "/" + path for path in (
            "v2/refs/a", "metadata/pack/id/key", "metadata/shard/id/key",
            "locks/internal/git-manifest/clock", "refs/journal/a", "manifests/latest",
            "git_object_catalog_db/a/key", "plain%20name/%E2%98%83",
            "utf8-%E4%B8%AD/%25", "directory/", "",
        )] + ["/outside/a", "/bucket", "/bucket?list-type=2&prefix=task%2Fv2%2Frefs%2F",
              "/bucket?versions&prefix=other%2F", "/bucket?list-type=2&prefix=%2Ftask%2Fmetadata%2Fpack%2Fa",
              "/bucket?list-type=2&prefix="]
        cases = []
        for path in paths:
            for query in ("", "?uploads", "?uploadId=opaque", "?partNumber=1&uploadId=x", "?delete"):
                if "?" in path and query:
                    continue
                for method in ("GET", "HEAD", "PUT", "POST", "DELETE", "PATCH"):
                    for copy in (False, True):
                        headers = {"x-amz-copy-source": "/bucket/source"} if copy else {}
                        wire_headers = email.message.Message()
                        for key, value in headers.items():
                            wire_headers[key] = value
                        index = len(cases)
                        sent, received = index % 172, index % 615
                        status = (200, 204, 206, 404, 412, 500)[index % 6]
                        error = "fixture_error" if index % 31 == 0 else None
                        expected.record(method, path + query, wire_headers, sent, received,
                                        status, proxy_error=error)
                        cases.append([method, path + query, headers, sent, received, status, 0, error])
        script = Path(__file__).with_name("native_provider_meter.mjs").resolve().as_uri()
        program = """
import fs from 'node:fs';
const {Counter} = await import(process.argv[1]);
const value = new Counter('bucket/task');
for (const row of JSON.parse(fs.readFileSync(0, 'utf8'))) value.record(...row);
delete value.value.latency_ms_histogram;
process.stdout.write(JSON.stringify(value.value));
"""
        observed = subprocess.run(["node", "--input-type=module", "--eval", program, script],
                                  input=json.dumps(cases), text=True, capture_output=True, check=True)
        self.assertEqual(json.loads(observed.stdout), expected.snapshot(include_paths=False))


if __name__ == "__main__":
    unittest.main()
