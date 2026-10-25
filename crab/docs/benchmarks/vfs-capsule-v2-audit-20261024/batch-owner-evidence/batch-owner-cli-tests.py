import json,os,subprocess
command=['cargo','test','-p','crab','--locked','--lib','--no-default-features','--features','nfs','--no-run','--message-format=json']
env=dict(os.environ,COMPILER_PATH='/root/Workspace/crabbuild-target/vfs-audit/linker',CARGO_BUILD_JOBS='1')
r=subprocess.run(command,cwd='/src',env=env,stdout=subprocess.PIPE,text=True)
executables=[]
for line in r.stdout.splitlines():
 record=json.loads(line)
 if record.get('reason')=='compiler-message':print(record['message'].get('rendered',''),flush=True)
 if record.get('reason')=='compiler-artifact' and record.get('profile',{}).get('test') and record.get('executable'):executables.append(record['executable'])
r.check_returncode();assert len(executables)==1,executables
subprocess.run([executables[0],'unmount','--nocapture','--test-threads=1'],cwd=os.environ['TMPDIR'],env=env,check=True)
print('batch_owner_cli_tests=ok',flush=True)
