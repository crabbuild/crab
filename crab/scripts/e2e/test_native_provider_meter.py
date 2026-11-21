#!/usr/bin/env python3
"""Keep the streaming performance meter's wire accounting canonical."""
import email.message
import json
from pathlib import Path
import subprocess
import tempfile
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

    def test_streaming_meter_preserves_wire_headers_and_bodies(self):
        script = Path(__file__).with_name("native_provider_meter.mjs").resolve()
        program = """
import http from 'node:http';
import fs from 'node:fs';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {setTimeout} from 'node:timers/promises';
const directory = process.argv[2];
let received;
const upstream = http.createServer(async (request, response) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  received = {path:request.url, body:Buffer.concat(chunks).toString(),
    constructor:request.headers.constructor,
    proxyConnection:request.headers['proxy-connection'] ?? null};
  response.writeHead(200, {'set-cookie':['first=1', 'second=2'],
    'proxy-connection':'close', 'constructor':'response-data'});
  response.end('response body');
});
await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve));
const config = {upstream:'http://127.0.0.1:' + upstream.address().port,
  key_prefix:'bucket/task', services:directory+'/services.json', final:directory+'/final.json'};
fs.writeFileSync(directory+'/config.json', JSON.stringify(config));
const child = spawn(process.execPath, [process.argv[1], directory+'/config.json'], {stdio:'ignore'});
const stopped = once(child, 'exit');
try {
  const deadline = Date.now() + 10000;
  while (!fs.existsSync(config.services)) {
    if (Date.now() >= deadline || child.exitCode !== null) throw Error('Meter did not start');
    await setTimeout(10);
  }
  const {meter_url} = JSON.parse(fs.readFileSync(config.services));
  const forwarded = await new Promise((resolve, reject) => {
    const request = http.request(meter_url+'/bucket/task/encoded%20name', {
      method:'PUT', headers:{'content-length':'12', 'constructor':'request-data',
        'proxy-connection':'close'}, agent:false}, async response => {
      try {
        const chunks = [];
        for await (const chunk of response) chunks.push(chunk);
        resolve({status:response.statusCode, body:Buffer.concat(chunks).toString(),
          cookies:response.headers['set-cookie'], constructor:response.headers.constructor,
          proxyConnection:response.headers['proxy-connection'] ?? null});
      } catch (error) { reject(error); }
    });
    request.on('error', reject);
    request.setTimeout(10000, () => request.destroy(Error('Wire test deadline')));
    request.end('request body');
  });
  child.kill('SIGTERM');
  const [exitCode] = await stopped;
  const final = JSON.parse(fs.readFileSync(config.final));
  process.stdout.write(JSON.stringify({received, forwarded, exitCode,
    requests:final.requests, requestBytes:final.request_body_bytes,
    responseBytes:final.response_body_bytes, active:final.active_requests_at_shutdown}));
} finally {
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGTERM');
    const timer = globalThis.setTimeout(() => child.kill('SIGKILL'), 10000);
    await stopped;
    globalThis.clearTimeout(timer);
  }
  upstream.closeAllConnections();
  await new Promise(resolve => upstream.close(resolve));
}
"""
        with tempfile.TemporaryDirectory(prefix="native-meter-wire-") as directory:
            observed = subprocess.run(
                ["node", "--input-type=module", "--eval", program, str(script), directory],
                text=True, capture_output=True, check=True, timeout=40)
        self.assertEqual(json.loads(observed.stdout), {
            "received": {"path":"/bucket/task/encoded%20name", "body":"request body",
                         "constructor":"request-data", "proxyConnection":None},
            "forwarded": {"status":200, "body":"response body", "cookies":["first=1", "second=2"],
                          "constructor":"response-data", "proxyConnection":None},
            "exitCode":0, "requests":1, "requestBytes":12, "responseBytes":13, "active":0})


if __name__ == "__main__":
    unittest.main()
