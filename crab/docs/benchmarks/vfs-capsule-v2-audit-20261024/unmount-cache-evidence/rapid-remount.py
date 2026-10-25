import fcntl,hashlib,json,os,pathlib,subprocess,sys,uuid,shutil,time,statistics
backend=sys.argv[1]; phase=sys.argv[2]
base=pathlib.Path('/e2e'); previous=json.loads((pathlib.Path('/qualified')/(backend+'-completed')/'result.json').read_text())
out=base/(phase+'-'+backend);out.mkdir(exist_ok=True)
work=pathlib.Path(os.environ['TMPDIR'])/('overlay-owner-'+backend+'-'+uuid.uuid4().hex);work.mkdir()
mount=pathlib.Path('/mnt/owner');mount.mkdir()
os.environ['PATH']=os.environ.get('PROBE_BIN_DIR','/qualified/bin')+':'+os.environ['PATH']
os.environ['CRAB_CACHE_DIR']=str(work/'cache')
sequence=0; mounted=False

def run(args,label,success=True):
    global sequence
    sequence+=1
    result=subprocess.run(args,cwd=work,capture_output=True,timeout=90)
    (out/(str(sequence)+'-'+label+'.stdout')).write_bytes(result.stdout)
    (out/(str(sequence)+'-'+label+'.stderr')).write_bytes(result.stderr)
    if result.returncode:
        logs=pathlib.Path('/root/.crab/mounts/logs')
        if logs.exists(): shutil.copytree(logs,out/('helper-logs-'+str(sequence)),dirs_exist_ok=True)
    if success: assert result.returncode==0,(label,result.returncode,result.stderr[-1500:])
    return result

def start(readonly):
    global mounted
    args=['crab','mount','--repo',previous['remote'],'--mountpoint',str(mount),'--ref','other','--backend',backend,'--no-refresh']
    if readonly: args+=['--read-only']
    run(args,'mount-ro' if readonly else 'mount-rw');mounted=True

def stop():
    global mounted
    run(['crab','unmount','--mountpoint',str(mount)],'unmount');mounted=False

cache_dir=None
checks=[]
def assert_cache_released():
    assert cache_dir is not None
    key=hashlib.sha256(str(cache_dir).encode()).hexdigest()[:12]
    lock_path=cache_dir.parent/'.mount-locks'/(key+'.lock')
    with lock_path.open('r+b') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        fcntl.flock(lock,fcntl.LOCK_UN)
    checks.append({'cache_free_at_unmount_return':True})
try:
    for cycle in range(10):
        start(False)
        scratch=mount/'unpublished-owner.txt'
        if cycle==0:scratch.write_bytes(b'retained unpublished content')
        assert scratch.read_bytes()==b'retained unpublished content'
        diff=json.loads(run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'rw-diff').stdout)
        cache_dir=pathlib.Path(diff['data']['cache_dir'])
        assert 'unpublished-owner.txt' in json.dumps(diff)
        stop();assert_cache_released()
        start(True)
        assert not scratch.exists()
        diff=run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'ro-diff',False)
        assert diff.returncode!=0
        stop();assert_cache_released()
        print(json.dumps({'backend':backend,'cycle':cycle,'rw_ro_immediate_remount':True,'cache_handoff_assertions':len(checks)}),flush=True)
finally:
    if mounted:stop()
(out/'result.json').write_text(json.dumps({'backend':backend,'cycles':10,'checks':checks},indent=2)+'\n')
print('rapid_remount_cache_handoff=ok '+backend,flush=True)
