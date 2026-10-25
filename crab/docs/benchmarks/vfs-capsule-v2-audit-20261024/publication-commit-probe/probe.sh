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
import json,mmap,os,pathlib,subprocess
root=pathlib.Path('/e2e')
mapping={}
with (root/'run/mount/models/model.bin').open('rb') as model:
    for name,access in [('shared',mmap.ACCESS_READ),('private',mmap.ACCESS_COPY)]:
        try:
            with mmap.mmap(model.fileno(),0,access=access) as view:
                model.seek(0)
                mapping[name]={'ok':view[:4096]==model.read(4096)}
        except OSError as error:
            mapping[name]={'ok':False,'errno':error.errno,'error':str(error)}
(root/'mmap-observation.json').write_text(json.dumps(mapping,indent=2)+'\n')
print(json.dumps({'mmap':mapping}),flush=True)
assert all(value['ok'] for value in mapping.values()), 'mmap admission failed'
path=root/'run/mount/open-through-commit.txt' 
with path.open('w+b',buffering=0) as held:
    held.write(b'before')
    os.fsync(held.fileno())
    commit=subprocess.run(['crab','mount','commit','--mountpoint',str(path.parent),'--message','retain open file through commit','--json'],capture_output=True,text=True,check=True)
    (root/'commit.json').write_text(commit.stdout)
    (root/'logs/commit.err').write_text(commit.stderr)
    held.seek(0)
    held.write(b'AFTER!')
    os.fsync(held.fileno())
    held.seek(0)
    observed={'held':held.read().decode(),'path':path.read_bytes().decode()}
    (root/'observation.json').write_text(json.dumps(observed,indent=2)+'\n')
    diff=subprocess.run(['crab','mount','diff','--mountpoint',str(path.parent),'--json'],capture_output=True,text=True,check=True)
    (root/'diff.json').write_text(diff.stdout)
    print(json.dumps(observed))
    assert observed['held']==observed['path']=='AFTER!', 'successful post-commit descriptor write disappeared from the pathname'

mapped_path=root/'run/mount/mapped-through-commit.txt'
mapped_path.write_bytes(b'A'*4096)
with mapped_path.open('r+b',buffering=0) as file:
    with mmap.mmap(file.fileno(),0,access=mmap.ACCESS_WRITE) as view:
        view[0]=ord('B')
        commit=subprocess.run(['crab','mount','commit','--mountpoint',str(mapped_path.parent),'--message','capture dirty mapping without msync','--json'],capture_output=True,text=True,check=True)
        (root/'mapped-commit.json').write_text(commit.stdout)
        (root/'logs/mapped-commit.err').write_text(commit.stderr)
        oid=json.loads(commit.stdout)['data']['result']['commit_oid']
        gitdir=(mapped_path.parent/'.git').read_text().removeprefix('gitdir: ').strip()
        stored=subprocess.check_output(['git','--git-dir',gitdir,'show',f'{oid}:mapped-through-commit.txt'])
        assert stored==b'B'+b'A'*4095, 'commit omitted dirty mapped bytes'
        view[1]=ord('C')
        view.flush()
        os.fsync(file.fileno())
        current=mapped_path.read_bytes()
        assert current==b'BC'+b'A'*4094, 'mapped write after commit disappeared'
        result={'committed_prefix':stored[:2].decode(),'post_commit_prefix':current[:2].decode()}
        (root/'mapped-observation.json').write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps(result),flush=True)

PROBE
