"""Own a local Jupyter kernel through the installed reference client."""

import json
import queue
import subprocess
import sys
import tempfile
import threading

from jupyter_client import KernelManager
from jupyter_client.kernelspec import KernelSpecManager


def emit(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=True) + "\n")
    sys.stdout.flush()


def reply(request_id, result=None, error=None):
    emit({"kind": "reply", "id": request_id, "result": result, "error": error})


def run():
    commands = queue.Queue()

    def read_commands():
        try:
            for line in sys.stdin:
                commands.put(json.loads(line))
        finally:
            # A closed Electron pipe still owns kernel shutdown, including
            # when the application exits without an explicit shutdown request.
            commands.put(None)

    threading.Thread(target=read_commands, daemon=True).start()
    manager = None
    client = None
    jobs = {}
    request = None

    with tempfile.TemporaryDirectory(prefix="crab-kernel-") as directory:
        try:
            while True:
                try:
                    request = commands.get(timeout=0 if client else None)
                except queue.Empty:
                    request = False
                if request is None:
                    break
                if request:
                    method = request["method"]
                    params = request.get("params", {})
                    request_id = request["id"]
                    try:
                        if method == "start":
                            manager = KernelManager(kernel_name=params["kernelName"], connection_file=directory + "/connection.json")
                            # The worker's stdout is the control protocol;
                            # native kernel writes must not enter that pipe.
                            manager.start_kernel(cwd=params["cwd"], stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=sys.stderr)
                            client = manager.client()
                            client.start_channels()
                            client.wait_for_ready(timeout=30)
                            reply(request_id)
                            emit({"kind": "status", "status": "idle"})
                        elif method == "execute":
                            msg_id = client.execute(params["code"], allow_stdin=False, stop_on_error=True)
                            jobs[msg_id] = {"id": request_id, "reply": None, "idle": False}
                        elif method == "interrupt":
                            manager.interrupt_kernel()
                            reply(request_id)
                        elif method == "restart":
                            for job in jobs.values():
                                reply(job["id"], error="Kernel restarted before execution completed")
                            jobs.clear()
                            client.stop_channels()
                            manager.restart_kernel(now=False)
                            client = manager.client()
                            client.start_channels()
                            client.wait_for_ready(timeout=30)
                            reply(request_id)
                            emit({"kind": "status", "status": "idle"})
                        else:
                            raise ValueError("Unknown kernel operation")
                    except Exception as error:
                        reply(request_id, error=str(error))
                        if method in ("start", "restart"):
                            break

                if client is None:
                    continue
                if not manager.is_alive():
                    raise RuntimeError("The kernel process exited")
                # Drain both channels without starving interrupt/shutdown
                # admission. Execution completes only after its reply AND idle.
                for channel in (client.iopub_channel, client.shell_channel):
                    for _ in range(100):
                        try:
                            message = channel.get_msg(timeout=0.005)
                        except queue.Empty:
                            break
                        parent = message["parent_header"].get("msg_id")
                        job = jobs.get(parent)
                        kind = message["header"]["msg_type"]
                        content = message["content"]
                        if kind == "status":
                            emit({"kind": "status", "status": content["execution_state"]})
                            if job and content["execution_state"] == "idle":
                                job["idle"] = True
                        elif job and kind == "execute_reply":
                            job["reply"] = content["status"]
                        elif job:
                            emit({"kind": "output", "requestId": job["id"], "msgType": kind, "content": content})
                        if job and job["idle"] and job["reply"] is not None:
                            reply(job["id"], {"status": job["reply"]})
                            del jobs[parent]
        finally:
            # KernelManager joins its provisioner and removes connection data;
            # the worker must remain alive until this cleanup acknowledges.
            try:
                if manager is not None:
                    if manager.has_kernel:
                        manager.shutdown_kernel(now=False)
                    else:
                        # Failed kernelspec admission creates no process;
                        # release its resources without a shutdown request.
                        manager.cleanup_resources()
            finally:
                if client is not None:
                    client.stop_channels()
            for job in jobs.values():
                reply(job["id"], error="Kernel disconnected before execution completed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--list"]:
        specs = KernelSpecManager().get_all_specs()
        emit([{"name": name, "displayName": value["spec"]["display_name"], "language": value["spec"]["language"]} for name, value in specs.items()])
    else:
        try:
            run()
        except Exception as error:
            emit({"kind": "failed", "error": str(error)})
            sys.exit(1)
