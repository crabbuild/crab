import json, os, subprocess
from pathlib import Path

def run(args):
    print("RUN", " ".join(args), flush=True)
    subprocess.run(args, cwd="/src", check=True)

command=["cargo","test","-p","crab-vfs","--locked","--lib","--no-default-features","--features","nfs,fuse","--no-run","--message-format=json"]
print("RUN", " ".join(command), flush=True)
result=subprocess.run(command,cwd="/src",stdout=subprocess.PIPE,text=True,check=True)
executables=[]
for line in result.stdout.splitlines():
    record=json.loads(line)
    if record.get("reason")=="compiler-artifact" and record.get("profile",{}).get("test") and record.get("executable"):
        executables.append(record["executable"])
assert len(executables)==1,executables
print("TEST_EXECUTABLE",executables[0],flush=True)
subprocess.run([executables[0]],cwd=os.environ["TMPDIR"],check=True)
subprocess.run([executables[0],"native_mount::","--ignored","--nocapture","--test-threads=1"],cwd=os.environ["TMPDIR"],check=True)
run(["cargo","clippy","-p","crab-vfs","--locked","--all-targets","--no-default-features","--features","nfs,fuse","--","-D","warnings"])
run(["cargo","check","-p","crab-read","--locked","--no-default-features"])
run(["cargo","check","-p","crab-sdk","--locked","--no-default-features","--lib"])
for features in ["","nfs","fuse"]:
    args=["cargo","check","-p","crab-vfs","--locked","--no-default-features"]
    if features: args += ["--features",features]
    run(args)
target=os.environ["CARGO_TARGET_DIR"]
prefix="/qualification/split-control-provider/bin"
run(["make","-C","crab","install","RELEASE_FLAGS=","CRAB_CLI_FEATURES_WITH_FUSE=nfs,fuse","CRAB_CLI_FEATURES_NO_FUSE=nfs","TARGET_DIR="+target,"RELEASE_BIN="+target+"/debug/crab","FUSE_MOUNT_RELEASE_BIN="+target+"/debug/crab-fuse-mount","NFS_MOUNT_RELEASE_BIN="+target+"/debug/crab-nfs-mount","CACHE_SERVER_RELEASE_BIN="+target+"/debug/crab-cache-server","PREFIX="+prefix,"CARGO_BIN="+prefix])
run(["cargo","test","-p","crab","--locked","--no-default-features","--features","nfs,fuse","--test","coordinator_lifecycle","--test","prop_coordinator"])
print("integration_qualification=ok",flush=True)
