import fcntl,hashlib,json,os,pathlib,shutil,subprocess,sys,uuid
base=pathlib.Path('/e2e');phase=sys.argv[2];out=base/('mixed-'+phase);out.mkdir(exist_ok=True)
work=pathlib.Path(os.environ['TMPDIR'])/('batch-owner-'+uuid.uuid4().hex);work.mkdir()
os.environ['PATH']=os.environ['PROBE_BIN_DIR']+':'+os.environ['PATH']
os.environ['CRAB_CACHE_DIR']=str(work/'cache')
mounts={backend:pathlib.Path('/mnt/mixed-'+backend) for backend in ['nfs','fuse']}
remotes={backend:json.loads((pathlib.Path('/qualified')/(backend+'-completed')/'result.json').read_text())['remote'] for backend in mounts}
for mount in mounts.values():mount.mkdir()
seq=0;cached={};findings=[];checks=0

def run(args,label,success=True):
 global seq
 seq+=1;r=subprocess.run(args,cwd=work,capture_output=True,timeout=120)
 (out/(str(seq)+'-'+label+'.stdout')).write_bytes(r.stdout);(out/(str(seq)+'-'+label+'.stderr')).write_bytes(r.stderr)
 if r.returncode:
  logs=pathlib.Path('/root/.crab/mounts/logs')
  if logs.exists():shutil.copytree(logs,out/('helper-logs-'+str(seq)),dirs_exist_ok=True)
 if success:assert r.returncode==0,(label,r.returncode,r.stderr[-1500:])
 return r

def cache_released(cache):
 global checks
 key=hashlib.sha256(str(cache).encode()).hexdigest()[:12]
 with (cache.parent/'.mount-locks'/(key+'.lock')).open('r+b') as lock:
  fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB);fcntl.flock(lock,fcntl.LOCK_UN)
 checks+=1

try:
 for index,readonly in enumerate([False,False,True]):
  for backend,mount in mounts.items():
   args=['crab','mount','--repo',remotes[backend],'--mountpoint',str(mount),'--ref','other','--backend',backend,'--no-refresh']
   if readonly:args+=['--read-only']
   run(args,'mount-'+backend)
   scratch=mount/('batch-'+backend+'.txt');expected=('retained '+backend+' batch bytes').encode()
   if readonly:
    assert not scratch.exists()
    rejected=run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'readonly-diff-'+backend,False)
    assert rejected.returncode!=0
   else:
    if index==0:scratch.write_bytes(expected)
    assert scratch.read_bytes()==expected
    diff=json.loads(run(['crab','mount','diff','--mountpoint',str(mount),'--json'],'diff-'+backend).stdout)
    cached[backend]=pathlib.Path(diff['data']['cache_dir'])
  result=run(['crab','unmount','--all'],'batch-unmount')
  finding={'round':index,'readonly':readonly,'exit':result.returncode,'still_mounted':{backend:os.path.ismount(mount) for backend,mount in mounts.items()},'stdout':result.stdout.decode()}
  findings.append(finding);print(json.dumps(finding),flush=True)
  assert not any(finding['still_mounted'].values()),'batch omitted a live backend owner'
  assert 'Unmounted 2 mount(s).' in finding['stdout']
  for cache in cached.values():cache_released(cache)
  registry=pathlib.Path('/root/.crab/mounts/mounts.json')
  assert not registry.exists() or json.loads(registry.read_text())==[]
finally:
 for backend,mount in mounts.items():
  if os.path.ismount(mount):run(['crab','unmount','--mountpoint',str(mount)],'cleanup-'+backend)
(out/'result.json').write_text(json.dumps({'findings':findings,'immediate_cache_handoff_assertions':checks},indent=2)+'\n')
print('mixed_batch_provider=ok',flush=True)
