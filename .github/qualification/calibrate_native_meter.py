import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
sys.dont_write_bytecode = True
root = Path(os.environ['QUAL_ROOT'])
sys.path.insert(0, str(Path(os.environ['GITHUB_WORKSPACE']) / 'crab/scripts/e2e'))
from native_meter import NativeMeter
endpoint = 'http://127.0.0.1:19020'
bucket = 'crab-meter-calibration'
meter = NativeMeter(endpoint, bucket, root / 'metrics/access.jsonl')
body = hashlib.shake_256(b'native-meter-calibration').digest(1024**2)
source = root / 'metrics/body.bin'
source.write_bytes(body)
def aws(*args, success=True):
    p = subprocess.run(['aws', '--endpoint-url', endpoint, 's3api', *args], capture_output=True, text=True)
    if success:
        assert p.returncode == 0, p.stderr
        return json.loads(p.stdout) if p.stdout.strip() else None
    assert p.returncode != 0 and 'PreconditionFailed' in p.stderr, p.stderr
aws('create-bucket', '--bucket', bucket)
aws('put-object', '--bucket', bucket, '--key', 'plain', '--body', str(source))
aws('put-object', '--bucket', bucket, '--key', 'plain', '--body', str(source), '--if-none-match', '*', success=False)
aws('put-object', '--bucket', bucket, '--key', 'plain', '--body', str(source), '--if-match', '"' + '0'*32 + '"', success=False)
aws('head-object', '--bucket', bucket, '--key', 'plain')
aws('get-object', '--bucket', bucket, '--key', 'plain', str(root / 'metrics/plain.bin'))
assert (root / 'metrics/plain.bin').read_bytes() == body
aws('get-object', '--bucket', bucket, '--key', 'plain', '--range', 'bytes=1-3', str(root / 'metrics/range.bin'))
assert (root / 'metrics/range.bin').read_bytes() == body[1:4]
aws('list-objects-v2', '--bucket', bucket, '--prefix', 'plain')
aws('delete-object', '--bucket', bucket, '--key', 'plain')
upload = aws('create-multipart-upload', '--bucket', bucket, '--key', 'multipart')['UploadId']
part = aws('upload-part', '--bucket', bucket, '--key', 'multipart', '--upload-id', upload, '--part-number', '1', '--body', str(source))
parts = root / 'metrics/parts.json'
parts.write_text(json.dumps({'Parts': [{'PartNumber': 1, 'ETag': part['ETag']}]}))
aws('complete-multipart-upload', '--bucket', bucket, '--key', 'multipart', '--upload-id', upload, '--multipart-upload', 'file://' + str(parts))
aws('get-object', '--bucket', bucket, '--key', 'multipart', str(root / 'metrics/multipart.bin'))
assert (root / 'metrics/multipart.bin').read_bytes() == body
time.sleep(.05)
snapshot = meter.snapshot()
assert snapshot['requests'] == 13, snapshot
assert snapshot['operations'] == {'put': 4, 'head': 1, 'get': 3, 'list': 1, 'delete': 1, 'multipart_create': 1, 'multipart_part': 1, 'multipart_complete': 1}, snapshot
assert snapshot['statuses'] == {'2xx': 11, '4xx': 2} and not snapshot['proxy_errors'], snapshot
(root / 'provenance/native-meter-calibration.json').write_text(json.dumps({'status': 'ok', 'snapshot': snapshot,
    'checks': ['all13HTTPattempts', '2conditionalconflicts', 'exactrangebytes', 'exactordinaryandmultipartbytes'],
    'scope': 'No proxy caching, buffering, signature Host rewrite or proxy retries; selected nonsecret query flags only in logs. Body-byte counters use Content-Length/NGINX body_bytes_sent; no modeled AWS billing.'}, indent=2) + '\n')
print('native meter calibrated', snapshot['requests'], flush=True)
