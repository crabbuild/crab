import statistics,time
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
 for backend,mount in mounts.items():
  run(['crab','mount','--repo',remotes[backend],'--mountpoint',str(mount),'--ref','other','--backend',backend,'--no-refresh'],'mount-'+backend)
 samples={'previous':[],'current':[]};canonical=None
 programs={'previous':'/root/Workspace/crabbuild-target/vfs-audit/unmount-cache/bin/crab','current':'/root/Workspace/crabbuild-target/vfs-audit/batch-owner/bin/crab'}
 for attempt in range(13):
  order=['previous','current'] if attempt%2==0 else ['current','previous']
  for name in order:
   started=time.monotonic();r=subprocess.run([programs[name],'mount','list','--json'],cwd=work,capture_output=True,timeout=30);elapsed=time.monotonic()-started
   assert r.returncode==0,r.stderr[-1000:]
   data=json.loads(r.stdout);assert len(data)==2
   assert {row['mountpoint'] for row in data}=={str(x) for x in mounts.values()}
   assert {row['backend'] for row in data}=={'nfs','fuse'}
   assert all(row['state']=='running' for row in data)
   for row in data:row.pop('uptime',None)
   data=sorted(data,key=lambda x:x['mountpoint'])
   if canonical is None:canonical=data
   assert data==canonical,'UI list metadata changed across clients'
   if attempt>=3:samples[name].append(elapsed)
 report={name:{'samples_seconds':values,'median_seconds':statistics.median(values)} for name,values in samples.items()}
 report['canonical_payload_sha256']=hashlib.sha256(json.dumps(canonical,sort_keys=True).encode()).hexdigest()
 report['owner_count']=2
 (out/'list-timings.json').write_text(json.dumps(report,indent=2)+'\n')
 print(json.dumps({name:row['median_seconds'] for name,row in report.items() if isinstance(row,dict)}),flush=True)
finally:
 for backend,mount in mounts.items():
  if os.path.ismount(mount):run(['crab','unmount','--mountpoint',str(mount)],'cleanup-'+backend)
print('mixed_live_list_comparison=ok',flush=True)
