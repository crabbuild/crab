set -euo pipefail
export PATH="/e2e/bin:$PATH"
export CRAB_CACHE_DIR=/root/crab-cache
export GIT_TERMINAL_PROMPT=0
REMOTE="crab://crab/mount-large/$1"
BACKEND="$2"
mkdir -p /e2e/run/seed /e2e/run/mount /e2e/logs
git config --global user.name "Crab VFS audit"
git config --global user.email "audit@example.invalid"
trap 'crab unmount --mountpoint /e2e/run/mount > /e2e/logs/unmount.log 2>&1 || true' EXIT
cd /e2e/run/seed
git init -b main > /e2e/logs/git-init.log 2>&1
crab init "$REMOTE" > /e2e/logs/init.log 2>&1
crab track '*.bin' > /e2e/logs/track.log 2>&1
python3 - <<'SEED'
import hashlib,pathlib
pathlib.Path('model.bin').write_bytes(b''.join(hashlib.sha256(str(i).encode()).digest() for i in range(131072)))
SEED
crab add model.bin > /e2e/logs/add.log 2>&1
git add crab.toml .gitattributes
git commit -m 'seed pointer' > /e2e/logs/commit-seed.log 2>&1
crab push origin HEAD:refs/heads/main > /e2e/logs/push-seed.log 2>&1
crab mount --repo /e2e/run/seed --mountpoint /e2e/run/mount --ref main --backend "$BACKEND" --no-refresh > /e2e/logs/mount.log 2>&1
python3 - "$REMOTE" <<'PROBE'
import hashlib,json,os,pathlib,sqlite3,subprocess,sys
root=pathlib.Path('/e2e')
path=root/'run/mount/model.bin'
gitdir=(path.parent/'.git').read_text().removeprefix('gitdir: ').strip()
subprocess.run(['git','--git-dir',gitdir,'remote','set-url','origin',sys.argv[1]],check=True)
def commit(label,push=False):
    command=['crab','mount','commit','--mountpoint',str(path.parent),'--message',label,'--json']
    if push: command.append('--push')
    result=subprocess.run(command,capture_output=True,text=True)
    (root/f'{label}.json').write_text(result.stdout)
    (root/f'logs/{label}.err').write_text(result.stderr)
    result.check_returncode()
    return json.loads(result.stdout)
def staging_counts():
    gitdir=pathlib.Path((path.parent/'.git').read_text().removeprefix('gitdir: ').strip())
    database=gitdir.parent/'.crab/staging/index.db'
    with sqlite3.connect(f'file:{database}?mode=ro',uri=True) as connection:
        return {name:connection.execute(f'SELECT count(*) FROM {name}').fetchone()[0] for name in ['path_leases','recipe_payload_leases','chunks']}
with path.open('r+b',buffering=0) as held:
    expected=bytearray(held.read())
    assert len(expected)==4*1024*1024, f"fixture was not hydrated: {len(expected)} bytes"
    expected[0]^=255
    held.seek(0);held.write(expected[:1]);os.fsync(held.fileno())
    commit('local-commit')
    before=staging_counts()
    commit('clean-push',True)
    after=staging_counts()
    print(json.dumps({'staging_before':before,'staging_after':after}),flush=True)
    (root/'staging.json').write_text(json.dumps({'before':before,'after':after},indent=2)+'\n')
    assert before['path_leases']>after['path_leases'], 'push did not retire the staged version'
    assert path.read_bytes()==expected, 'published pathname content differs'
    expected[1]^=255
    held.seek(1)
    try:
        held.write(expected[1:2]);os.fsync(held.fileno())
    except OSError as error:
        print(json.dumps({'held_write_errno':error.errno,'error':str(error)}),flush=True)
        raise
    held.seek(0)
    actual=held.read()
    visible=path.read_bytes()
    observed={'held_sha256':hashlib.sha256(actual).hexdigest(),'path_sha256':hashlib.sha256(visible).hexdigest(),'expected_sha256':hashlib.sha256(expected).hexdigest()}
    (root/'observation.json').write_text(json.dumps(observed,indent=2)+'\n')
    print(json.dumps(observed),flush=True)
    assert actual==visible==expected, 'held descriptor lost content or pathname after clean push'
    commit('later-push',True)
    (root/'expected.sha256').write_text(hashlib.sha256(expected).hexdigest()+'\n')
PROBE
crab clone "$REMOTE" /e2e/run/clone --eager > /e2e/logs/clone.log 2>&1
sha256sum /e2e/run/clone/model.bin | cut -d' ' -f1 > /e2e/clone.sha256
cmp /e2e/clone.sha256 /e2e/expected.sha256
echo retirement_e2e=ok
