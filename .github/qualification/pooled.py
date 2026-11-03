import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import time

sys.dont_write_bytecode = True
root = Path(os.environ['QUAL_ROOT'])
repo = Path(os.environ['GITHUB_WORKSPACE'])
sys.path.insert(0, str(repo / 'crab/scripts/e2e'))
import run_concurrent_push_smoke as q
build = json.loads((root / 'provenance/install.json').read_text())
binary = Path(build['binary'])
assert build['status'] == 'ok' and hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
os.environ['CRAB_E2E_TRACE_REQUEST_PATHS'] = '1'
sys.argv = ['pooled', '--root', str(root), '--run-id', 'pooled', '--bucket', 'crab-final-pooled',
            '--endpoint-url', os.environ['AWS_ENDPOINT_URL'], '--crab-bin', str(binary), '--git-bin', '/usr/bin/git',
            '--source', str(root / 'medium-source'), '--agents', '1', '--skip-same-branch',
            '--timeout', '7200', '--push-timeout', '7200']
class Smoke(q.ConcurrentPushSmoke):
    def install_helper_alias(self):
        super().install_helper_alias()
        (self.bin_dir / 'crab').symlink_to(binary)
s = Smoke(q.parse_args())
state = {'status': 'running', 'started_at': time.time(), 'commit': build['commit'],
         'binary_sha256': build['binary_sha256'], 'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
         'scope': '500 actual CLI publications, automatic compaction, independent protocolv2 transition fetch and20000exact blob bodies. Raw footer shape is diagnostic only; product fetch/fullno-repairfsck authenticate all sections. No latency capacity claim.'}
receipt = root / 'provenance/pooled.json'
assert not receipt.exists()
def save():
    receipt.write_text(json.dumps(state, indent=2) + '\n')
save()
try:
    s.preflight()
    s.init_seed()
    s.run_crab(s.seed, ['repack', '--json'])
    base = s.git_value(s.seed, ['rev-parse', 'HEAD'])
    reader = s.run_root / 'cold-baseline'
    s.run_git(reader, ['config', 'protocol.version', '2'])
    chunks = ['commit refs/heads/main\ncommitter Crab Proof <proof@example.invalid> 1793650000 +0000\ndata 17\nlarge pooled data\nfrom ' + base + '\n']
    for i in range(20000):
        body = f'large-pooled-proof-{i:05d}\n'
        chunks.append(f'M 100644 inline proof/file-{i:05d}.raw\ndata {len(body)}\n{body}')
    chunks.append('\ndone\n')
    p = subprocess.run(['/usr/bin/git', 'fast-import', '--quiet'], cwd=s.seed, env=s.env, input=''.join(chunks), text=True, capture_output=True)
    assert p.returncode == 0, p.stderr
    s.run_crab(s.seed, ['push', 'origin', 'main'])
    state['acknowledged'] = 1
    tip = s.git_value(s.seed, ['rev-parse', 'HEAD'])
    tree = s.git_value(s.seed, ['rev-parse', 'HEAD^{tree}'])
    for i in range(1, 500):
        tip = s.git_value(s.seed, ['commit-tree', tree, '-p', tip, '-m', f'pooled proof {i}'])
        s.run_git(s.seed, ['update-ref', 'refs/heads/main', tip])
        s.run_crab(s.seed, ['push', 'origin', 'main'])
        state['acknowledged'] += 1
        if i % 100 == 0:
            save()
            print('pooled acknowledged', i + 1, flush=True)
    save()
    s.check('five-hundred-publications-acknowledged', state['acknowledged'] == 500)
    prefix = f'{q.REMOTE_PREFIX}/{s.run_id}'
    def get(key, label):
        output = s.artifacts / (label + '.bin')
        s.run_cmd('diagnostic ' + label, ['aws', '--endpoint-url', s.args.endpoint_url, 's3api', 'get-object',
                                        '--bucket', s.args.bucket, '--key', key, str(output)], s.run_root)
        return output.read_bytes()
    head = json.loads(get(prefix + '/v2/refs/heads/' + b'refs/heads/main'.hex() + '.json', 'ref-head'))
    frontier = head['committed']['frontier']
    s.check('one-automatic-five-hundred-run', len(frontier) == 1 and frontier[0]['capsule_count'] == 500 and frontier[0]['level'] > 0)
    pointer = frontier[0]
    body = get(prefix + '/v2/capsules/' + pointer['hash'][:2] + '/' + pointer['hash'], 'compacted-run')
    assert body[-8:] == b'CRBRUN01'
    length = int.from_bytes(body[-48:-40], 'big')
    footer = json.loads(body[-48-length:-48])
    offset = footer['visibility_pool']['offset']
    sections = []
    for c in footer['capsules']:
        if c['visibility']:
            length = c['visibility']['length']
            sections.append({'start': offset, 'end': offset + length, 'length': length})
            offset += length
    big = max(sections, key=lambda x: x['length'])
    s.check('large-pooled-visibility', big['length'] > 3 * 65536, big)
    before = s.request_proxy.snapshot()
    record = s.run_git(reader, ['fetch', 'origin'], extra_env={'CRAB_LOG': 'error,crab_read::upload_pack=info,crab::git::upload_pack_wire=info'})
    s.check('tip-bound-transition', 'tip_bound_transition' in Path(record.stderr_log).read_text())
    paths = s.request_proxy.paths_since(before['path_count'])
    (s.artifacts / 'fetch-requests.json').write_text(json.dumps(paths, indent=2) + '\n')
    exact = sum(x.get('range') == f"bytes={big['start']}-{big['end']-1}" and x['key'].endswith(pointer['hash']) for x in paths)
    s.check('one-exact-large-bulk-transfer', exact == 1, {'transfers': exact, 'range': big})
    s.check('independent-exact-tip', s.git_value(reader, ['rev-parse', 'refs/remotes/origin/main']) == tip)
    s.check('independent-exact-history-length', int(s.git_value(reader, ['rev-list', '--count', base + '..refs/remotes/origin/main'])) == 500)
    s.check('independent-object-directory', not (reader / '.git/objects/info/alternates').exists())
    s.run_git(reader, ['fsck', '--strict', '--full'])
    archive = s.artifacts / 'proof.tar'
    with archive.open('wb') as stream:
        p = subprocess.run(['/usr/bin/git', 'archive', 'refs/remotes/origin/main', 'proof'], cwd=reader, env=s.env, stdout=stream, stderr=subprocess.PIPE)
        assert p.returncode == 0, p.stderr
    seen = set()
    with tarfile.open(archive) as tar:
        for member in tar:
            if not member.isfile():
                continue
            assert member.name.startswith('proof/file-') and member.name.endswith('.raw')
            index = int(member.name[11:-4])
            assert 0 <= index < 20000 and index not in seen
            assert tar.extractfile(member).read() == f'large-pooled-proof-{index:05d}\n'.encode()
            seen.add(index)
    s.check('all-new-blob-bytes', len(seen) == 20000)
    s.run_fsck()
    s.check('readonly-input-preserved', s.source_state() == s.source_identity)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
    s.report.status = 'ok'
    s.write_report(force=True)
    state.update(status='ok', completed_at=time.time(), checks=len(s.report.checks), exact_transfers=exact,
                 large_visibility=big, report_sha256=hashlib.sha256((s.artifacts / 'report.json').read_bytes()).hexdigest())
    save()
except BaseException as error:
    state.update(status='failed', error=str(error), completed_at=time.time())
    save()
    raise
finally:
    if s.request_proxy:
        s.request_proxy.close()
