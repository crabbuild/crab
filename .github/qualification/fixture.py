import hashlib
import json
import os
from pathlib import Path
import subprocess

root = Path(os.environ['QUAL_ROOT'])
source = root / 'medium-source'
assert not source.exists()
source.mkdir()
subprocess.run(['git', 'init', '-b', 'main', str(source)], check=True)
chunks = []
for i in range(10000):
    message = f'synthetic history {i}'
    body = hashlib.shake_256(f'crab-final-history:{i}'.encode()).digest(1024)
    chunks.append((f'commit refs/heads/main\ncommitter Crab Fixture <fixture@example.invalid> {1793400000+i} +0000\ndata {len(message)}\n{message}\nM 100644 inline history/file-{i%64:02}.raw\ndata 1024\n').encode() + body + b'\n\n')
chunks.append(b'done\n')
subprocess.run(['git', 'fast-import', '--quiet'], cwd=source, input=b''.join(chunks), check=True)
subprocess.run(['git', 'reset', '--hard', 'main'], cwd=source, check=True)
subprocess.run(['git', 'fsck', '--strict', '--full'], cwd=source, check=True)
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
assert subprocess.check_output(['git', 'rev-list', '--count', 'HEAD'], cwd=source, text=True).strip() == '10000'
assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=source, text=True).strip()
(root / 'provenance/fixture.json').write_text(json.dumps({'head': head, 'commits': 10000, 'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'scope': 'New deterministic generated Git fixture; no external checkout modified; common input for paired64and1000developer workdays'}, indent=2) + '\n')
