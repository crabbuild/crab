import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import sys
import time

sys.dont_write_bytecode = True
repo = Path(os.environ['GITHUB_WORKSPACE'])
root = Path(os.environ['QUAL_ROOT'])
sys.path.insert(0, str(repo / 'crab/scripts/e2e'))
import run_concurrent_push_smoke as q
from run_mixed_team_rustfs import MixedTeam

p = argparse.ArgumentParser()
p.add_argument('--agents', type=int, choices=[8, 64, 1000], required=True)
p.add_argument('--label', required=True)
p.add_argument('--meter', action='store_true')
a = p.parse_args()
build = json.loads((root / 'provenance/install.json').read_text())
binary = Path(build['binary'])
assert build['status'] == 'ok' and hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
source_names = ['run_mixed_team_rustfs.py', 'run_concurrent_push_smoke.py']
source_hashes = {n: hashlib.sha256((repo / 'crab/scripts/e2e' / n).read_bytes()).hexdigest() for n in source_names}
receipt = root / 'provenance' / (a.label + '.json')
assert not receipt.exists()
state = {'status': 'running', 'started_at': time.time(), 'source_commit': build['commit'],
         'binary_sha256': build['binary_sha256'], 'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
         'source_hashes': source_hashes, 'agents': a.agents, 'commits_per_developer': 10,
         'workers': 16, 'request_meter': a.meter,
         'scope': 'Local RustFS on an isolated hosted runner; 16 active writers, not1000simultaneous arrivals. All historical Git/Xet/LFS bytes and full no-repair audit. Metered snapshots include client preparation; service timing is push-command wall time. No AWS capacity/cost claim.'}

def save():
    temporary = receipt.with_suffix('.tmp')
    temporary.write_text(json.dumps(state, indent=2) + '\n')
    temporary.replace(receipt)

class Team(MixedTeam):
    def verify_payloads(self, index):
        super().verify_payloads(index)
        if self.args.agents != 1000:
            return
        for parent in ['payload-readers', 'payload-caches']:
            path = self.run_root / parent / f'agent-{index:03d}'
            assert not path.is_symlink() and path.resolve().is_relative_to(self.run_root.resolve())
            if path.exists():
                shutil.rmtree(path)
        with self.report_lock:
            self.report.artifacts.setdefault('retired_verified_payload_consumers', []).append(index)
            self.write_report()

    def verify_cold_team(self):
        before = self.git_value(self.seed, ['ls-remote', '--refs', self.remote_url])
        result = self.run_crab(self.seed, ['repack', '--json'], name='drained explicit checkpoint')
        body = json.loads(Path(result.stdout_log).read_text())
        self.check('checkpoint-acknowledged', body.get('schema') == 'repack' and 'error' not in body)
        self.check('checkpoint-preserves-exact-refs', before == self.git_value(self.seed, ['ls-remote', '--refs', self.remote_url]))
        super().verify_cold_team()

save()
for key in ['AWS_PROFILE', 'AWS_DEFAULT_PROFILE', 'AWS_SESSION_TOKEN', 'CRAB_CACHE_SERVICE_URL', 'CRAB_CACHE_PSK',
            'CRAB_CACHE_TOKEN', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES']:
    os.environ.pop(key, None)
run_id = f'final-workday-10-{a.agents}'
argv = ['final-team', '--root', str(root / a.label), '--run-id', run_id,
        '--bucket', 'crab-' + a.label, '--endpoint-url', os.environ['AWS_ENDPOINT_URL'],
        '--crab-bin', str(binary), '--git-bin', '/usr/bin/git', '--source', str(root / 'medium-source'),
        '--agents', str(a.agents), '--branch-update-rounds', '9', '--skip-same-branch', '--consumer-mode', 'cold-final',
        '--max-parallel-pushes', '16', '--max-parallel-readers', '4', '--timeout', '7200', '--push-timeout', '7200']
if not a.meter:
    argv.append('--no-request-capture')
sys.argv = argv
s = Team(q.parse_args())
try:
    code = s.run()
    report = s.artifacts / 'report.json'
    data = json.loads(report.read_text())
    assert code == 0 and data['status'] == 'ok'
    assert source_hashes == {n: hashlib.sha256((repo / 'crab/scripts/e2e' / n).read_bytes()).hexdigest() for n in source_names}
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
    batches = data['push_batches']
    assert len(batches) == 10 and all(b['jobs'] == a.agents and b['successful'] == a.agents for b in batches)
    samples = [x['command']['duration_ms'] for k in ['branch_fanout', 'branch_updates'] for x in data[k]]
    assert len(samples) == 10 * a.agents
    samples.sort()
    p95 = samples[math.ceil(.95 * len(samples)) - 1]
    batch_p95 = [b['service_ms']['p95'] for b in batches]
    state.update(integrity='passed', checks=len(data['checks']), report=str(report), report_sha256=hashlib.sha256(report.read_bytes()).hexdigest(),
                 p95_ms=p95, batch_p95_ms=batch_p95, target_p95_ms=10000,
                 performance='passed' if p95 <= 10000 and max(batch_p95) <= 10000 else 'failed')
    save()
    if a.agents == 1000:
        assert state['performance'] == 'passed', '1000developer p95 exceeds10000ms'
    state.update(status='ok', completed_at=time.time())
    save()
    print(json.dumps(state), flush=True)
except BaseException as error:
    state.update(status='failed', error=str(error), completed_at=time.time())
    save()
    raise
