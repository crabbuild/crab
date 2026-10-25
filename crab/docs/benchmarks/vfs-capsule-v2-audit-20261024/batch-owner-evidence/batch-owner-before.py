import json, os, subprocess
from pathlib import Path

def run(args):
    print("RUN", " ".join(args), flush=True)
    subprocess.run(args, cwd="/src", check=True)

command=["cargo","test","-p","crab-vfs","--locked","--lib","--no-default-features","--features","nfs,fuse","--no-run","--message-format=json"]
print("RUN", " ".join(command), flush=True)
result=subprocess.run(command,cwd="/src",stdout=subprocess.PIPE,text=True)
executables=[]
for line in result.stdout.splitlines():
    record=json.loads(line)
    if record.get("reason")=="compiler-message": print(record["message"].get("rendered",""),flush=True)
    if record.get("reason")=="compiler-artifact" and record.get("profile",{}).get("test") and record.get("executable"):
        executables.append(record["executable"])
result.check_returncode()
assert len(executables)==1,executables
print("TEST_EXECUTABLE",executables[0],flush=True)
subprocess.run([executables[0],"mount_control::tests::listing_rejects_incomplete_coordinator_discovery","--exact","--nocapture"],cwd=os.environ["TMPDIR"],check=True)
