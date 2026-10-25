import json, os, pathlib, socket, subprocess, sys, tempfile, threading
prefix=sys.argv[1];phase=sys.argv[2]
results=[]
for batch in [False,True]:
 with tempfile.TemporaryDirectory(prefix="failed-control-") as work:
  home=pathlib.Path(work);mount=home/"view";mount.mkdir()
  registry=home/".crab/mounts/mounts.json";registry.parent.mkdir(parents=True,mode=0o700)
  listener=socket.socket();listener.bind(("127.0.0.1",0));listener.listen(1)
  def peer():
   stream,_=listener.accept()
   with stream:
    request=b""
    while not request.endswith(b"\n"):request+=stream.recv(4096)
    assert json.loads(request)["op"]=="shutdown"
    stream.sendall(b'{"ok":false,"error":"injected live shutdown rejection"}\n')
   listener.close()
  child=subprocess.Popen(["sleep","60"])
  entry={"mountpoint":str(mount),"source":"crab://bucket/owner","ref":"refs/heads/main","pid":child.pid,"start_time":"2026-10-25T00:00:00Z","read_only":False,"name":"owner","backend":"nfs","control_endpoint":"tcp:127.0.0.1:%d?token=probe"%listener.getsockname()[1]}
  registry.write_text(json.dumps([entry]))
  thread=threading.Thread(target=peer,daemon=True);thread.start()
  try:
   args=[str(pathlib.Path(prefix)/"crab"),"unmount"]+(["--all"] if batch else ["--mountpoint",str(mount)])
   result=subprocess.run(args,env=dict(os.environ,HOME=str(home)),stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=45)
   thread.join(timeout=2);assert not thread.is_alive()
   alive=child.poll() is None
   preserved=json.loads(registry.read_text())==[entry]
   results.append({"batch":batch,"exit":result.returncode,"owner_alive":alive,"registry_preserved":preserved,"original_error_visible":b"injected live shutdown rejection" in result.stderr,"stderr":result.stderr.decode(errors="replace")[-1500:]})
   print(json.dumps(results[-1]),flush=True)
  finally:
   if child.poll() is None:child.terminate()
   child.wait(timeout=5)
if phase=="before":
 assert all(not x["owner_alive"] for x in results),results
else:
 assert all(x["exit"]!=0 and x["owner_alive"] and x["registry_preserved"] and x["original_error_visible"] for x in results),results
print("failed_live_control_probe=ok "+phase,flush=True)
