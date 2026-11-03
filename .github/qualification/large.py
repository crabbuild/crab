import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from unittest.mock import patch

sys.dont_write_bytecode = True
root = Path(os.environ['QUAL_ROOT'])
repo = Path(os.environ['GITHUB_WORKSPACE'])
sys.path.insert(0, str(repo / 'crab/scripts/e2e'))
import run_mixed_history_rustfs as history
from run_lfs_qualification import parse_args, write_deterministic
build = json.loads((root / 'provenance/install.json').read_text())
binary = Path(build['binary'])
assert build['status'] == 'ok' and hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
subprocess.run(['aws', '--endpoint-url', os.environ['AWS_ENDPOINT_URL'], 's3api', 'create-bucket', '--bucket', 'crab-final-large'], check=True)
a = parse_args(['--crab-bin', str(binary), '--remote-url', 'crab://crab-final-large/history',
                '--evidence-dir', str(root / 'large'), '--profile', 'large-object', '--object-count', '3',
                '--commit-count', '3', '--path-count', '1', '--min-size', str(2*1024**3), '--max-size', str(2*1024**3)])
class BoundedHistory(history.MixedHistory):
    def create_history(self):
        try:
            with patch.object(history, 'write_deterministic', bounded_initial_git):
                super().create_history()
        finally:
            self.report['workload'].pop('ordinary_git_blob_bytes', None)
            self.report['workload'].update(ordinary_git_initial_bytes=256*1024**2, ordinary_git_edited_bytes=1025*1024**2)
            self.save()
s = BoundedHistory(a)
s.report.update(schema='crab.mixed-history-qualification', profile='mixed-history')
s.report['qualification'] = {'commit': build['commit'], 'binary_sha256': build['binary_sha256'],
                             'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                             'scope': 'Three2GiB Xet/LFS versions plus ordinaryGit256MiB initial/1025MiB edited, rename/delete/merge, native/Git publication, independent historical cold hashes and fullno-repair audit. Product limits unchanged.'}
def bounded_initial_git(path, size, seed, index):
    return write_deterministic(path, 256*1024**2 if path.name == 'large.raw' else size, seed, index)
try:
    s.run_all()
    s.report['workload'].update(ordinary_git_initial_bytes=256*1024**2, ordinary_git_edited_bytes=1025*1024**2)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == build['binary_sha256']
    assert s.report['status'] == 'passed' and all(c['ok'] for c in s.report['checks'])
    s.save()
except BaseException as error:
    s.report.update(status='failed', error=str(error))
    s.save()
    raise

