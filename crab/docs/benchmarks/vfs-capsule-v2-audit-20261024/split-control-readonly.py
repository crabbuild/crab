import errno, hashlib, json, os, pathlib, subprocess, sys, uuid
backend=sys.argv[1]
base=pathlib.Path('/e2e')
previous=json.loads((base/(backend+'-completed')/'result.json').read_text())
out=base/(backend+'-readonly'); out.mkdir(exist_ok=True)
work=pathlib.Path(os.environ['TMPDIR'])/('split-readonly-'+backend+'-'+uuid.uuid4().hex); work.mkdir()
mount=pathlib.Path('/mnt/readonly'); mount.mkdir()
os.environ['PATH']='/e2e/bin:'+os.environ['PATH']
os.environ['CRAB_CACHE_DIR']=str(work/'cache')
sequence=0

def run(args,label,expected=True):
    global sequence
    sequence+=1
    result=subprocess.run(args,cwd=work,capture_output=True,timeout=180)
    (out/(str(sequence)+'-'+label+'.stdout')).write_bytes(result.stdout)
    (out/(str(sequence)+'-'+label+'.stderr')).write_bytes(result.stderr)
    assert (result.returncode==0)==expected,(label,result.returncode,result.stderr[-1500:])
    return result.stdout

mounted=False
try:
    run(['crab','mount','--repo',previous['remote'],'--mountpoint',str(mount),'--ref','other','--backend',backend,'--read-only','--no-refresh'],'mount')
    mounted=True
    status=json.loads(run(['crab','mount','status','--mountpoint',str(mount),'--live-only','--json'],'status'))
    assert status['read_only'] is True and status['backend']==backend,status
    run(['crab','mount','list','--json'],'list')
    path=mount/'model.bin'
    assert hashlib.sha256(path.read_bytes()).hexdigest()==previous['results'][-1]['clone_sha256']
    try:
        with path.open('r+b',buffering=0) as stream: stream.write(b'!')
    except OSError as error:
        assert error.errno in (errno.EROFS,errno.EACCES,errno.EPERM),error
    else: raise AssertionError('read-only mount accepted a write')
    run(['crab','mount','commit','--mountpoint',str(mount),'--message','read-only rejection','--json'],'commit-rejected',False)
    run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'diff')
    run(['crab','mount','refresh','--mountpoint',str(mount)],'refresh')
    assert hashlib.sha256(path.read_bytes()).hexdigest()==previous['results'][-1]['clone_sha256']
    run(['crab','mount','switch','--mountpoint',str(mount),'--ref','main'],'switch')
    assert hashlib.sha256(path.read_bytes()).hexdigest()==previous['results'][0]['clone_sha256']
    final=json.loads(run(['crab','mount','status','--mountpoint',str(mount),'--live-only','--json'],'status-after'))
    assert final['read_only'] is True
    (out/'result.json').write_text(json.dumps({'backend':backend,'remote':previous['remote'],'main_cli_controls':True,'write_rejected':True,'commit_rejected':True,'refresh_switch_hashes_match':True},indent=2)+'\n')
finally:
    if mounted: run(['crab','unmount','--mountpoint',str(mount)],'unmount')
print('readonly_main_cli_e2e=ok',backend,flush=True)
