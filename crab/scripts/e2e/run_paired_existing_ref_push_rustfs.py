#!/usr/bin/env python3
"""Compare two release artifacts on one Linux/RustFS runner in ABBA order.

CI supplies PAIR_ROOT, PAIR_SOURCE, PAIR_CONTROL, PAIR_CANDIDATE, PAIR_CLIENT
and GITHUB_RUN_ID. Every cohort retains cold readback and strict integrity checks.
"""

import hashlib
import json
import math
import os
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

root = Path(os.environ['PAIR_ROOT'])
root.mkdir(exist_ok=False)
source = Path(os.environ['PAIR_SOURCE'])
control = Path(os.environ['PAIR_CONTROL'])
candidate = Path(os.environ['PAIR_CANDIDATE'])
client = os.environ['PAIR_CLIENT']
container = f"crab-paired-{os.environ['GITHUB_RUN_ID']}-{client}"
image = 'rustfs/rustfs:1.0.0-beta.8-glibc'
endpoint = 'http://127.0.0.1:19280'
env = os.environ | {
    'AWS_ACCESS_KEY_ID': 'crab', 'AWS_SECRET_ACCESS_KEY': 'crab',
    'AWS_REGION': 'us-east-1', 'AWS_ENDPOINT_URL': endpoint,
    'GIT_AUTHOR_DATE': '2026-11-07T22:40:00Z',
    'GIT_COMMITTER_DATE': '2026-11-07T22:40:00Z',
    'NO_PROXY': '127.0.0.1,localhost',
}


def command(args, **kwargs):
    return subprocess.check_output([str(x) for x in args], **kwargs).decode().strip()
provenance = {
    'client': client, 'git': command(['git', '--version']), 'cpu': command(['lscpu']),
    'candidate_build': json.loads((candidate.parent / 'candidate-build.json').read_text()),
    'variants': {},
}
for label, binary in [('control', control), ('candidate', candidate)]:
    provenance['variants'][label] = {
        'version': command([binary, 'version']),
        'sha256': hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest(),
    }
assert provenance['candidate_build']['sha256'] == provenance['variants']['candidate']['sha256']
subprocess.run(['docker', 'pull', image], check=True)
provenance['rustfs_digest'] = json.loads(command(['docker', 'image', 'inspect', image, '--format', '{{json .RepoDigests}}']))
(root / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
rows = []
plans = {}
trials = [
    ('a1', 'control', control), ('b1', 'candidate', candidate),
    ('b2', 'candidate', candidate), ('a2', 'control', control),
]
for trial, label, binary in trials:
    data = root / f'data-{trial}'
    data.mkdir()
    subprocess.run(['sudo', 'chown', '10001:10001', str(data)], check=True)
    subprocess.run([
        'docker', 'run', '-d', '--name', container,
        '--label', f"crab-paired={os.environ['GITHUB_RUN_ID']}-{client}",
        '-p', '127.0.0.1:19280:9000',
        '-e', 'RUSTFS_ACCESS_KEY=crab', '-e', 'RUSTFS_SECRET_KEY=crab',
        '-v', f'{data}:/data', image, 'server', '/data', '--address', ':9000',
    ], check=True)
    try:
        for _ in range(120):
            try:
                with urllib.request.urlopen(endpoint + '/health/ready', timeout=2) as response:
                    if response.status == 200:
                        break
            except (OSError, urllib.error.HTTPError):
                pass
            time.sleep(0.5)
        else:
            raise RuntimeError('private RustFS readiness timeout')
        for actors in [50, 10, 1]:
            rounds = 7 if actors == 50 else 100 // actors
            run_id = f'paired-writers-{actors}'
            args = [
                'python3', '-B', 'crab/scripts/e2e/run_existing_ref_push_rustfs.py',
                '--root', root / trial, '--run-id', run_id, '--bucket', 'crab-paired',
                '--endpoint-url', endpoint, '--crab-bin', binary, '--source', source,
                '--push-client', client, '--agents', actors,
                '--branch-update-rounds', rounds, '--max-parallel-pushes', actors,
                '--consumer-mode', 'cold-final', '--skip-same-branch',
                '--no-request-capture', '--timeout', 600, '--push-timeout', 600,
            ]
            print(json.dumps({'started': trial, 'variant': label, 'actors': actors, 'client': client}), flush=True)
            with (root / f'{trial}-{actors}.log').open('wb') as log:
                subprocess.run([str(x) for x in args], env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=1800)
            run = root / trial / run_id
            report = json.loads((run / 'artifacts/report.json').read_text())
            updates = report['branch_updates']
            assert report['status'] == 'ok' and report['checks'] and all((c['ok'] for c in report['checks']))
            assert len(updates) == actors * rounds and report['workload']['update_payload_bytes'] == 4096
            assert all((x['status'] == 'ok' and x['command']['exit_code'] == 0 for x in updates))
            tips = command(['git', '-C', run / 'cold-final', 'for-each-ref', '--format=%(refname) %(objectname)', 'refs/remotes/origin'])
            plan = {'base': report['workload']['ref_setup']['base_oid'], 'tips': tips}
            if actors in plans:
                assert plans[actors] == plan, 'variant Git plans differ'
            else:
                plans[actors] = plan
            timings = sorted((x['command']['duration_ms'] for x in updates))
            batches = report['push_batches'][1:]
            assert len(batches) == rounds and all((b['successful'] == actors and b['workers'] == actors for b in batches))
            row = {
                'trial': trial, 'variant': label, 'client': client, 'actors': actors,
                'samples': len(timings),
                'p50_ms': timings[math.ceil(len(timings) * 0.5) - 1],
                'p95_ms': timings[math.ceil(len(timings) * 0.95) - 1],
                'max_ms': max(timings),
                'wave_pushes_per_second': len(timings) * 1000 / sum(b['elapsed_ms'] for b in batches),
                'all_integrity_checks_passed': True, 'identical_git_plan': True,
            }
            rows.append(row)
            (root / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
            (root / 'git-plans.json').write_text(json.dumps(plans, indent=2) + '\n')
            print(json.dumps(row), flush=True)
    finally:
        with (root / f'{trial}-rustfs.log').open('wb') as log:
            subprocess.run(['docker', 'logs', container], stdout=log, stderr=subprocess.STDOUT)
        subprocess.run(['docker', 'stop', container], check=True)
        subprocess.run(['docker', 'rm', container], check=True)
print(json.dumps({'completed': True, 'timed_updates': sum((r['samples'] for r in rows)), 'all_private_servers_stopped': True}), flush=True)
