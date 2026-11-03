import http.client
import json
from pathlib import Path
import threading
import run_concurrent_push_smoke as q

class NativeMeter(q.RequestCountingProxy):
    def __init__(self, endpoint, key_prefix, log):
        super().__init__(endpoint, key_prefix)
        self.endpoint = endpoint
        self.log = Path(log)
        self.offset = self.log.stat().st_size
        self.read_lock = threading.Lock()
        self.trace_paths = False
        self.unknown_request_body_lengths = 0

    @property
    def url(self):
        return self.endpoint

    def snapshot(self, *, include_paths=True):
        with self.read_lock:
            with self.log.open('rb') as stream:
                stream.seek(self.offset)
                for raw in stream:
                    if not raw.endswith(b'\n'):
                        break
                    row = json.loads(raw)
                    headers = http.client.HTTPMessage()
                    for name, field in [('Range', 'range'), ('x-amz-copy-source', 'copy')]:
                        if row[field] not in ('', '-'):
                            headers[name] = row[field]
                    query = row['query']
                    if row['prefix'] not in ('', '-'):
                        query += '&prefix=' + row['prefix']
                    path = row['path'] + '?' + query
                    length = row['content_length']
                    if row['transfer_encoding'] not in ('', '-') and not length.isdigit():
                        self.unknown_request_body_lengths += 1
                    error = 'native-upstream' if row['upstream_status'] in ('', '-') and int(row['status']) >= 500 else None
                    self.record(row['method'], path, headers, int(length) if length.isdigit() else 0,
                                int(row['body_bytes']), int(row['status']),
                                elapsed_ms=round(float(row['elapsed']) * 1000), proxy_error=error)
                    self.offset = stream.tell()
            result = super().snapshot(include_paths=False)
            result['unknown_request_body_lengths'] = self.unknown_request_body_lengths
            result['native_log_offset'] = self.offset
            return result

