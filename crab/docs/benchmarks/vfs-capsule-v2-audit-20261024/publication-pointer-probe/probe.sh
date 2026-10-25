set -euo pipefail
export PATH="/e2e/bin:$PATH"
export CRAB_CACHE_DIR=/root/crab-cache
export GIT_TERMINAL_PROMPT=0
git config --global user.name "Crab VFS audit"
git config --global user.email "audit@example.invalid"
mkdir -p /e2e/run/mount /e2e/logs
trap 'crab unmount --mountpoint /e2e/run/mount > /e2e/logs/unmount.log 2>&1 || true' EXIT
crab mount --repo "$1" --mountpoint /e2e/run/mount --ref main --backend fuse --no-refresh > /e2e/logs/mount.log 2>&1
python3 - <<'PROBE'
import json,os,pathlib,subprocess
root=pathlib.Path('/e2e')
path=root/'run/mount/models/model.bin'
with path.open('r+b',buffering=0) as held:
    expected=bytearray(held.read(16))
    for revision in range(2):
        expected[revision]^=255
        held.seek(revision)
        held.write(expected[revision:revision+1])
        os.fsync(held.fileno())
        commit=subprocess.run(['crab','mount','commit','--mountpoint',str(path.parent.parent),'--message',f'pointer with held descriptor {revision}','--json'],capture_output=True,text=True,check=True)
        (root/f'commit-{revision}.json').write_text(commit.stdout)
        (root/f'logs/commit-{revision}.err').write_text(commit.stderr)
    expected[2]^=255
    held.seek(2)
    held.write(expected[2:3])
    os.fsync(held.fileno())
    held.seek(0)
    old=held.read(16)
    with path.open('rb') as current:
        named=current.read(16)
    observation={'held':old.hex(),'path':named.hex(),'expected':expected.hex()}
    (root/'observation.json').write_text(json.dumps(observation,indent=2)+'\n')
    print(json.dumps(observation),flush=True)
    assert old==named==expected, 'unpublished pointer descriptor lost its pathname'
PROBE
