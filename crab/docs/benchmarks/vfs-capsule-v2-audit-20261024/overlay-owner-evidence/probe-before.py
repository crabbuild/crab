import hashlib,json,os,pathlib,subprocess,sys,uuid,shutil
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

try:
    start(False)
    scratch=mount/'unpublished-owner.txt';scratch.write_bytes(b'retained unpublished content')
    diff=run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'rw-diff')
    assert 'unpublished-owner.txt' in diff.stdout.decode()
    stop();start(True)
    assert not scratch.exists(),'read-only view unexpectedly exposes the writable overlay'
    status=json.loads(run(['crab','mount','status','--mountpoint',str(mount),'--live-only','--json'],'ro-status').stdout)
    assert status['read_only'] is True
    diff=run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'ro-diff',False)
    finding={'backend':backend,'readonly_diff_exit':diff.returncode,'invisible_path_reported':'unpublished-owner.txt' in diff.stdout.decode(),'readonly_view_has_path':scratch.exists()}
    (out/'finding.json').write_text(json.dumps(finding,indent=2)+'\n');print(json.dumps(finding),flush=True)
    stop();start(False)
    assert scratch.read_bytes()==b'retained unpublished content','inspection lost retained writable bytes'
    stop()
    assert diff.returncode!=0,'read-only inspection bypassed its live owner and reported hidden writable state'
finally:
    if mounted:stop()
print('overlay_owner_mode_cycle=ok',backend,flush=True)
